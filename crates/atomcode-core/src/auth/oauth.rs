use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use atomcode_telemetry::{Event, Telemetry};

/// Platform OAuth Broker URL (client_secret is kept on the broker)
pub const PLATFORM_BROKER_URL: &str = "https://acs.atomgit.com";
pub const PLATFORM_LOGIN_URL: &str = "https://acs.atomgit.com/auth/login";
pub const PLATFORM_CHECK_URL: &str = "https://acs.atomgit.com/auth/check";
pub const PLATFORM_TOKEN_URL: &str = "https://acs.atomgit.com/auth/token";
pub const PLATFORM_EXCHANGE_URL: &str = "https://acs.atomgit.com/oauth/exchange";
pub const PLATFORM_REFRESH_URL: &str = "https://acs.atomgit.com/oauth/refresh";

/// AtomGit OAuth endpoints
pub const AUTHORIZE_URL: &str = "https://atomgit.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://atomgit.com/oauth/token";
pub const USER_URL: &str = "https://atomgit.com/api/v5/user";

/// Blocking HTTP client pre-configured with `ATOMCODE_USER_AGENT`. Every
/// OAuth-side request must carry the token or AtomGit's gate rejects it.
/// Centralized so a future UA format change (e.g. append install-id)
/// happens in one spot rather than at each `Client::new()` site.
fn blocking_client() -> reqwest::blocking::Client {
    // Hard timeouts here too — the `get_valid_token` path calls
    // `refresh_access_token` synchronously whenever a stored token
    // looks expired, and that runs on the main TUI thread (via
    // `Client::from_stored_auth` → `/status`, drift monitor, etc.).
    // Without a cap, a slow or unreachable OAuth server would hang
    // the UI indefinitely. Same budget as the coding-plan client.
    reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(crate::ATOMCODE_USER_AGENT)
        .build()
        .unwrap_or_else(|_| reqwest::blocking::Client::new())
}

/// Stored authentication data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthInfo {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_in: Option<i64>,
    /// Unix timestamp (seconds) when this token was obtained
    #[serde(default)]
    pub created_at: i64,
    pub user: UserInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub username: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub avatar_url: Option<String>,
}

// ============================================================================
// Platform API types
// ============================================================================

#[derive(Debug, Deserialize)]
struct PlatformLoginResponse {
    login_url: String,
    state: String,
}

#[derive(Debug, Deserialize)]
struct PlatformCheckResponse {
    valid: bool,
}

#[derive(Debug, Deserialize)]
struct PlatformUserInfo {
    id: String,
    username: String,
    name: Option<String>,
    email: Option<String>,
    avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlatformTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    user: PlatformUserInfo,
}

// ============================================================================
// ESC-cancel support for the OAuth poll loop
// ============================================================================
//
// The poll loop in `login()` historically did `loop { http_check; sleep(2s) }`
// with no input handling — Linux/WSL users with broken `xdg-open` had no way
// to exit short of Ctrl+C (which kills the whole CLI/TUI). We now print the
// auth URL up-front for those users and accept ESC during the wait.
//
// Cooked mode (set by `suspend_for_external` in the TUI, default everywhere
// in CLI mode) line-buffers stdin — ESC alone won't reach `read()` until the
// user hits Enter. So while waiting, we temporarily switch stdin to cbreak
// (non-canonical, no echo) via an RAII `CbreakGuard`, restoring the original
// termios on every drop path. If `tcgetattr`/`tcsetattr` fail (non-tty stdin
// from a pipe or CI), the guard returns `None` and the loop falls back to a
// plain sleep — login still works, ESC just has no effect.
//
// Windows has no `poll(2)` over stdin and the existing
// `read_callback_from_stdin_until_stopped` path is already gated off there
// for the same reason. We follow the same pattern: `CbreakGuard` is a
// zero-sized stub that always returns `None`, and `wait_for_esc_or_timeout`
// degrades to `thread::sleep`.

/// Outcome of waiting for stdin activity during the OAuth poll loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscOutcome {
    /// Bare ESC keypress — user cancelled.
    Cancelled,
    /// poll(2) timed out, or `read` returned 0 / error.
    Timeout,
    /// Some bytes arrived but it wasn't a bare ESC (escape sequence,
    /// stray letter / Enter, paste). Treated identically to Timeout
    /// at the call site — fall through to the HTTP check.
    OtherInput,
}

/// Classify a freshly-read stdin buffer as cancel / timeout / ignore.
///
/// Bare ESC = single 0x1B byte. Anything else (escape sequence, normal
/// keystroke, pasted text) is `OtherInput`. Empty buffer = `Timeout`.
///
/// Terminals batch escape sequences (e.g. arrow up = `\x1B[A`) into a
/// single write to the master pty, so a 32-byte non-blocking read sees
/// the whole sequence at once and we never mistake its prefix for bare
/// ESC. See spec `2026-04-28-show-oauth-url-design.md` §5.
fn classify_input(bytes: &[u8]) -> EscOutcome {
    match bytes {
        [] => EscOutcome::Timeout,
        [0x1B] => EscOutcome::Cancelled,
        _ => EscOutcome::OtherInput,
    }
}

#[cfg(not(target_os = "windows"))]
struct CbreakGuard {
    fd: std::os::unix::io::RawFd,
    orig: libc::termios,
}

#[cfg(target_os = "windows")]
struct CbreakGuard;

impl CbreakGuard {
    /// Try to switch stdin to cbreak. Returns `None` if stdin isn't a
    /// tty (ENOTTY) or if `tcsetattr` fails. On Windows always returns
    /// `None` — no equivalent of the Unix poll-based path.
    #[cfg(not(target_os = "windows"))]
    fn new() -> Option<Self> {
        use std::os::unix::io::AsRawFd;
        let fd = io::stdin().as_raw_fd();
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            return None;
        }
        let mut new = orig;
        new.c_lflag &= !(libc::ICANON | libc::ECHO);
        new.c_cc[libc::VMIN] = 0;
        new.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &new) } != 0 {
            return None;
        }
        Some(Self { fd, orig })
    }

    #[cfg(target_os = "windows")]
    fn new() -> Option<Self> {
        None
    }
}

#[cfg(not(target_os = "windows"))]
impl Drop for CbreakGuard {
    fn drop(&mut self) {
        // Best-effort restore. If this somehow fails the terminal is
        // stuck in cbreak — `stty sane` recovers it. Drop runs on every
        // exit path including panic so the common case is always clean.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig);
        }
    }
}

/// Wait up to `timeout` for stdin activity (ESC keypress) or sleep
/// until the timeout expires. Used to interleave ESC-cancel checks
/// with the OAuth `/auth/check` poll cadence.
///
/// On Windows or when the cbreak guard couldn't be established, this
/// just sleeps and returns `Timeout` — ESC never fires but login still
/// works.
#[cfg(not(target_os = "windows"))]
fn wait_for_esc_or_timeout(guard: &Option<CbreakGuard>, timeout: Duration) -> EscOutcome {
    let Some(g) = guard.as_ref() else {
        thread::sleep(timeout);
        return EscOutcome::Timeout;
    };

    let mut pfd = libc::pollfd {
        fd: g.fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if rc <= 0 {
        // 0 = timeout (no data); <0 = poll error (EINTR etc.). Either
        // way the right move is "fall through to HTTP check"; the
        // outer loop's HTTP round-trip is the natural rate limit.
        return EscOutcome::Timeout;
    }
    let mut buf = [0u8; 32];
    let n = unsafe { libc::read(g.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n <= 0 {
        return EscOutcome::Timeout;
    }
    classify_input(&buf[..n as usize])
}

#[cfg(target_os = "windows")]
fn wait_for_esc_or_timeout(_guard: &Option<CbreakGuard>, timeout: Duration) -> EscOutcome {
    thread::sleep(timeout);
    EscOutcome::Timeout
}

/// Outcome of one `LoginSession::poll_once` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollOutcome {
    /// User hasn't completed the browser sign-in yet — wait and retry.
    Pending,
    /// `/auth/check` reported `valid=true`. Caller should call `finish()`.
    Authorized,
}

/// In-flight OAuth session. Returned by `start_login()`. The caller
/// drives the flow:
///
/// 1. Display `session.url()` and (best-effort) `open_browser()`.
/// 2. Loop `poll_once()` until `Authorized`, sleeping between calls
///    AT THE CALLER'S CADENCE — this lets the TUI interleave UI events
///    (ESC for cancel) and the CLI use a simple `thread::sleep`.
/// 3. Call `finish()` to exchange `state` → token.
pub struct LoginSession {
    state: String,
    login_url: String,
    client: reqwest::blocking::Client,
}

impl LoginSession {
    /// Authorization URL the user must visit. Stable for the lifetime
    /// of the session — safe to show once and reuse.
    pub fn url(&self) -> &str {
        &self.login_url
    }

    /// Best-effort browser launch. Always silent — failures are expected
    /// on Linux/WSL where the URL on screen is the user's actual path.
    pub fn open_browser_best_effort(&self) {
        let _ = open_browser(&self.login_url);
    }

    /// One non-blocking HTTP check against `/auth/check`. Returns
    /// `Pending` until the user finishes the browser flow, then
    /// `Authorized`. Errors only on transport/parse failures; a
    /// "not yet" answer is `Ok(Pending)`, never `Err`.
    pub fn poll_once(&self) -> Result<PollOutcome> {
        let resp = self
            .client
            .get(PLATFORM_CHECK_URL)
            .query(&[("state", &self.state)])
            .send()
            .context("Failed to call /auth/check")?;
        if resp.status().is_success() {
            if let Ok(check) = resp.json::<PlatformCheckResponse>() {
                if check.valid {
                    return Ok(PollOutcome::Authorized);
                }
            }
        }
        Ok(PollOutcome::Pending)
    }

    /// Final step: `/auth/token` exchange + `LoginSuccess` telemetry.
    /// Consumes the session — only call after `poll_once` returned
    /// `Authorized`.
    pub fn finish(self, tel: Option<&Arc<Telemetry>>) -> Result<AuthInfo> {
        let token_resp: PlatformTokenResponse = self
            .client
            .get(PLATFORM_TOKEN_URL)
            .query(&[("state", &self.state)])
            .send()
            .context("Failed to call /auth/token")?
            .json()
            .context("Failed to parse /auth/token response")?;

        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let auth_info = AuthInfo {
            access_token: token_resp.access_token,
            refresh_token: token_resp.refresh_token,
            token_type: token_resp.token_type,
            expires_in: token_resp.expires_in,
            created_at,
            user: UserInfo {
                id: token_resp.user.id,
                username: token_resp.user.username,
                name: token_resp.user.name,
                email: token_resp.user.email,
                avatar_url: token_resp.user.avatar_url,
            },
        };

        if let Some(t) = tel {
            // Push account_id onto the telemetry handle BEFORE emitting
            // login_success so the event itself — and every subsequent event in
            // this process — carries the id. The handle-level setter outlives
            // any task-local scope, so events emitted outside the main scope
            // (e.g. before scope is entered, or from spawned tasks) inherit it.
            t.set_account_id(Some(auth_info.user.id.to_string()));
            t.track(Event::LoginSuccess);
        }

        Ok(auth_info)
    }
}

/// Begin OAuth login: call `/auth/login`, return a session containing
/// the auth URL + state. Cheap (one HTTP round-trip), never blocks on
/// user action — separated from polling so callers can render the URL
/// before yielding control to the wait loop.
pub fn start_login() -> Result<LoginSession> {
    let client = reqwest::blocking::Client::new();
    let resp: PlatformLoginResponse = client
        .get(PLATFORM_LOGIN_URL)
        .query(&[("provider", "atomgit")])
        .send()
        .context("Failed to call /auth/login")?
        .json()
        .context("Failed to parse /auth/login response")?;
    Ok(LoginSession {
        state: resp.state,
        login_url: strip_force_login(&resp.login_url),
        client,
    })
}

/// Drop `force_login=true` from the broker-supplied OAuth URL. The
/// broker emits this flag to force re-authentication on every login;
/// stripping it lets users already signed in to atomgit.com
/// auto-authorize and skip the consent page. State binding via the
/// `state` parameter is unchanged, so the request is still anchored
/// to this specific login attempt.
fn strip_force_login(url: &str) -> String {
    url.replace("&force_login=true", "")
        .replace("?force_login=true&", "?")
        .replace("?force_login=true", "")
}

/// Stdout-driven OAuth login: prints the URL, opens the browser,
/// polls `/auth/check` with stdin-driven ESC cancel. Used by the CLI
/// (`atomcode login`, `atomcode codingplan`) and by `setup.rs`'s
/// `step_login` when the TUI hasn't already pre-flighted login.
///
/// TUI callers should NOT use this — render via `start_login()` +
/// `LoginSession::poll_once()` so the input box stays visible and ESC
/// is captured through `input_rx` (no termios manipulation needed).
///
/// `tel` is optional so non-CLI callers (tests, coding_plan setup) can
/// pass `None` when they don't hold a telemetry handle.
pub fn login(tel: Option<&Arc<Telemetry>>) -> Result<AuthInfo> {
    let session = start_login()?;

    // Always print the URL — `xdg-open` on Linux/WSL silently fails
    // often enough that we can't rely on it. On the desktop happy path
    // the browser opens *and* the URL stays in scrollback as a backup.
    println!("  Browser didn't open? Open the URL below in any browser to sign in:");
    println!("  {}", session.url());

    // Try to enter cbreak so we can detect a bare-ESC keypress. None
    // (non-tty stdin / tcsetattr failure) → fall back to plain sleep,
    // and don't advertise an ESC affordance that wouldn't work.
    let cbreak = CbreakGuard::new();
    if cbreak.is_some() {
        println!();
        println!("  Press ESC to cancel");
    }

    session.open_browser_best_effort();

    loop {
        match session.poll_once()? {
            PollOutcome::Authorized => break,
            PollOutcome::Pending => {}
        }
        match wait_for_esc_or_timeout(&cbreak, Duration::from_secs(2)) {
            EscOutcome::Cancelled => anyhow::bail!("login cancelled by user"),
            EscOutcome::Timeout | EscOutcome::OtherInput => {}
        }
    }

    session.finish(tel)
}

/// Extract state from a pasted callback URL (kept for potential future fallback use)
#[allow(dead_code)]
fn pasted_state(url: &str) -> Option<String> {
    url.split('?')
        .nth(1)?
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            if parts.next()? == "state" {
                Some(urlencoding_decode(parts.next()?))
            } else {
                None
            }
        })
        .next()
}

/// Generate random state string for CSRF protection
#[allow(dead_code)]
fn generate_state() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("atomcode_{}", timestamp)
}

/// Open browser with the authorization URL
#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("cmd")
        .raw_arg(format!("/C start \"\" \"{}\"", url))
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn open_browser(_url: &str) -> Result<()> {
    anyhow::bail!("Unsupported platform for browser auto-open");
}

/// Race a local TCP listener against stdin paste; return the first
/// `(code, state)` that arrives. Listener handles the normal desktop path
/// where the browser hits `127.0.0.1:8765`; stdin path handles WSL /
/// headless Linux where the user copies the callback URL from their
/// browser's address bar and pastes it in.
///
/// Kept for potential future fallback use — the platform-broker flow in
/// `login()` is the active callback path now.
#[allow(dead_code)]
fn await_callback(port: u16) -> Result<(String, String)> {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => Some(l),
        Err(e) => {
            println!("  Could not bind port {} ({}). Paste path only.", port, e);
            None
        }
    };

    println!(
        "  Waiting for callback on http://127.0.0.1:{}/callback",
        port
    );
    println!("  Or paste the full callback URL here and press Enter:");
    println!("  (Ctrl+C to cancel)\n");

    let (tx, rx) = mpsc::channel::<Result<(String, String)>>();
    let stop = Arc::new(AtomicBool::new(false));

    #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
    let has_listener = listener.is_some();
    if let Some(listener) = listener {
        let tx_l = tx.clone();
        let stop_l = Arc::clone(&stop);
        thread::spawn(move || {
            let r = accept_callback_until_stopped(listener, &stop_l);
            let _ = tx_l.send(r);
        });
    }

    // Stdin reader — spawn on Unix **regardless** of listener status. The
    // listener covers the desktop path where the browser hits
    // 127.0.0.1:8765; stdin covers everything else (headless Linux / SSH /
    // Wayland without xdg-open / WSL under X forwarding failure). Earlier
    // versions gated this on `!has_listener`, which silently broke Linux:
    // the listener binds fine but the browser can't reach it, and with
    // no stdin reader spawned the user's pasted URL went nowhere and the
    // whole login hung forever.
    //
    // Must be cancellable: previous revisions used a blocking
    // `stdin.lock().read_line()` + a "zombie thread is harmless" comment.
    // It wasn't harmless — FD 0 and /dev/tty point to the same terminal
    // device on Unix, so the kernel's line discipline delivers each byte
    // to whichever reader calls `read` first. When the listener won the
    // race, the zombie `read_line` was still blocked; the user's first
    // keystroke after login got read by the zombie (parsed as a bad
    // callback URL, dropped) instead of by crossterm's /dev/tty reader.
    // Reported as "Chinese IME commits need two attempts to land".
    //
    // Fix: poll(2)-based loop that checks the `stop` AtomicBool between
    // 100 ms timeouts, so when the listener wins we set `stop=true` and
    // the stdin thread exits before the user types anything.
    //
    // Windows is still gated off because its stdin `read_line` blocks on
    // a console handle that can't be cancelled from another thread and
    // doesn't have an equivalent poll(2) path.
    #[cfg(not(target_os = "windows"))]
    {
        let tx_stdin = tx.clone();
        let stop_stdin = Arc::clone(&stop);
        thread::spawn(move || {
            let r = read_callback_from_stdin_until_stopped(&stop_stdin);
            let _ = tx_stdin.send(r);
        });
    }
    #[cfg(target_os = "windows")]
    {
        if !has_listener {
            let tx_stdin = tx.clone();
            thread::spawn(move || {
                let stdin = io::stdin();
                let mut line = String::new();
                let r = match stdin.lock().read_line(&mut line) {
                    Ok(0) => Err(anyhow::anyhow!("stdin closed")),
                    Ok(_) => parse_pasted_callback(&line),
                    Err(e) => Err(anyhow::Error::new(e).context("Failed to read from stdin")),
                };
                let _ = tx_stdin.send(r);
            });
        }
    }
    // Drop the original `tx` — the listener and stdin readers each
    // cloned their own. Without this drop the channel would never
    // close after both readers finish, so `rx.recv()` on an early
    // cancellation would hang.
    drop(tx);

    let result = rx.recv().context("login cancelled")?;
    stop.store(true, Ordering::Relaxed);
    result
}

/// Accept a single OAuth callback on an already-bound listener, polling a
/// Poll stdin for a pasted callback URL, checking `stop` every 100 ms so
/// the caller can cancel (e.g. when the listener won the race). Returns
/// `Err("stdin cancelled")` on stop, `Err(...)` on a read error or a line
/// that doesn't parse as a callback URL, `Ok((code, state))` on success.
///
/// Uses `poll(2)` + non-blocking reads so we never sit inside a blocking
/// `read_line()` — that was the bug behind "first keystroke after login
/// goes to a zombie stdin thread instead of crossterm". On macOS / Linux,
/// FD 0 (this thread's read) and /dev/tty (crossterm's read) point to
/// the same terminal device; whichever syscall lands on a byte first
/// gets it, and a blocked `read_line` stays in line for the next input.
#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
fn read_callback_from_stdin_until_stopped(stop: &AtomicBool) -> Result<(String, String)> {
    use std::os::unix::io::AsRawFd;

    let stdin = io::stdin();
    let fd = stdin.as_raw_fd();

    // Save original flags so we restore them on exit — leaving stdin
    // non-blocking after login would break subsequent code that expects
    // the normal blocking shape (e.g. any future CLI prompt helper).
    let orig_flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if orig_flags >= 0 {
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, orig_flags | libc::O_NONBLOCK);
        }
    }

    // RAII guard: restore flags on any exit path (stop, error, parse fail).
    struct FlagGuard {
        fd: std::os::unix::io::RawFd,
        orig_flags: i32,
    }
    impl Drop for FlagGuard {
        fn drop(&mut self) {
            if self.orig_flags >= 0 {
                unsafe {
                    libc::fcntl(self.fd, libc::F_SETFL, self.orig_flags);
                }
            }
        }
    }
    let _guard = FlagGuard { fd, orig_flags };

    let mut line = String::new();
    let mut buf = [0u8; 256];
    loop {
        if stop.load(Ordering::Relaxed) {
            anyhow::bail!("stdin cancelled");
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_rc = unsafe { libc::poll(&mut pfd, 1, 100) };
        if poll_rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(anyhow::Error::new(err).context("poll(stdin)"));
        }
        if poll_rc == 0 {
            continue; // timeout — re-check stop, re-poll
        }
        // Data available; drain what's there. read(2) in non-blocking
        // mode returns up to one pipe buffer in a single call.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(anyhow::Error::new(err).context("read(stdin)"));
        }
        if n == 0 {
            anyhow::bail!("stdin closed");
        }
        // Append as UTF-8 (lossy — pasted URLs are ASCII; any weird
        // bytes in a URL would fail `parse_pasted_callback` anyway).
        line.push_str(&String::from_utf8_lossy(&buf[..n as usize]));
        if line.contains('\n') {
            return parse_pasted_callback(&line);
        }
    }
}

/// `stop` flag every 200ms so the caller can cancel (e.g. when the paste
/// path won the race).
#[allow(dead_code)]
fn accept_callback_until_stopped(
    listener: TcpListener,
    stop: &AtomicBool,
) -> Result<(String, String)> {
    listener
        .set_nonblocking(true)
        .context("Failed to set non-blocking mode")?;

    let mut stream = loop {
        if stop.load(Ordering::Relaxed) {
            anyhow::bail!("listener cancelled");
        }
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e).context("Failed to accept connection"),
        }
    };

    stream.set_nonblocking(false)?;

    // Read HTTP request
    let mut reader = io::BufReader::new(&mut stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Parse the request line (GET /callback?code=...&state=... HTTP/1.1)
    let url: String = request_line
        .split_whitespace()
        .nth(1)
        .context("Invalid HTTP request")?
        .to_string();

    // Parse query parameters
    let query_start = url.find('?').context("No query parameters in callback")?;
    let query = &url[query_start + 1..];

    let params: HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts
                .next()
                .map(|v| urlencoding_decode(v))
                .unwrap_or_default();
            Some((key.to_string(), value))
        })
        .collect();

    // Check for error — redirect browser to AtomGit
    if let Some(error) = params.get("error") {
        let error_desc = params
            .get("error_description")
            .map(|s| s.as_str())
            .unwrap_or(error);
        let response = "HTTP/1.1 302 Found\r\nLocation: https://atomgit.com\r\n\r\n";
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        anyhow::bail!("OAuth error: {}", error_desc);
    }

    let code = params.get("code").context("No code in callback")?.clone();
    let state = params.get("state").cloned().unwrap_or_default();

    // Send success response to browser
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
        <html><head><title>AtomCode Login</title>\
        <style>body{font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#1a1a2e;color:#eee}\
        .container{text-align:center;padding:2rem}h1{color:#7c3aed;margin:0}p{color:#888}\
        .success{color:#22c55e;font-size:4rem}</style></head>\
        <body><div class=\"container\">\
        <div class=\"success\">✓</div>\
        <h1>Authorization Successful</h1>\
        <p>You can close this window and return to AtomCode.</p>\
        </div></body></html>";

    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    Ok((code, state))
}

/// Simple URL decoding
fn urlencoding_decode(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }

    result
}

/// Refresh the access token using the stored refresh_token via Platform Broker.
/// Returns updated AuthInfo with new tokens, and saves it to disk.
pub fn refresh_access_token(auth: &AuthInfo) -> Result<AuthInfo> {
    let refresh_token = auth
        .refresh_token
        .as_deref()
        .context("No refresh_token available — please /login again")?;

    let client = blocking_client();

    // Call Platform Broker API for refresh
    let response = client
        .post(PLATFORM_REFRESH_URL)
        .json(&serde_json::json!({ "refresh_token": refresh_token }))
        .send()
        .context("Failed to send refresh token request to broker")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        anyhow::bail!(
            "Token refresh failed ({}): {} — please /login again",
            status,
            body
        );
    }

    #[derive(Deserialize)]
    struct BrokerResponse {
        access_token: String,
        token_type: Option<String>,
        expires_in: Option<i64>,
        refresh_token: Option<String>,
        user: Option<PlatformUserInfo>,
    }

    let broker_resp: BrokerResponse = response.json().context("Failed to parse broker response")?;

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let new_auth = AuthInfo {
        access_token: broker_resp.access_token,
        refresh_token: broker_resp
            .refresh_token
            .or_else(|| auth.refresh_token.clone()),
        token_type: broker_resp
            .token_type
            .unwrap_or_else(|| auth.token_type.clone()),
        expires_in: broker_resp.expires_in.or(auth.expires_in),
        created_at,
        user: broker_resp
            .user
            .map(|u| UserInfo {
                id: u.id,
                username: u.username,
                name: u.name,
                email: u.email,
                avatar_url: u.avatar_url,
            })
            .unwrap_or_else(|| auth.user.clone()),
    };

    save_auth(&new_auth)?;
    Ok(new_auth)
}

/// Get a valid access token, refreshing automatically if expired.
/// Returns the access token string ready to use.
pub fn get_valid_token() -> Result<String> {
    let auth = get_stored_auth().context("Not logged in — please use /login first")?;

    // Check if token is expired (with 5-minute safety margin)
    if let Some(expires_in) = auth.expires_in {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let expires_at = auth.created_at + expires_in;

        if now >= expires_at - 300 {
            // Token expired or about to expire — try refresh
            match refresh_access_token(&auth) {
                Ok(new_auth) => return Ok(new_auth.access_token),
                Err(e) => anyhow::bail!("Token expired and refresh failed: {}", e),
            }
        }
    } else if auth.created_at == 0 {
        // Legacy auth.toml without created_at — no way to know if expired,
        // try refresh if refresh_token is available, otherwise use as-is
        if auth.refresh_token.is_some() {
            if let Ok(new_auth) = refresh_access_token(&auth) {
                return Ok(new_auth.access_token);
            }
        }
    }

    Ok(auth.access_token)
}

/// Logout - clear stored auth.
///
/// Core-layer function: does the filesystem work and returns. User-facing
/// messaging is the caller's job — this was previously `println!`-ing
/// "Logged out successfully" directly, which bypassed the TUI renderer
/// and bled into the input box area on next repaint, and also produced
/// a duplicate line in CLI mode where `handle_command` prints its own
/// confirmation. No `Err` distinguishes "file absent" from "file removed" —
/// both are success from the user's perspective ("you're logged out").
pub fn logout() -> Result<()> {
    let auth_path = auth_file_path();
    if auth_path.exists() {
        std::fs::remove_file(&auth_path).context("Failed to remove auth file")?;
    }
    Ok(())
}

/// Get stored auth info
pub fn get_stored_auth() -> Option<AuthInfo> {
    let auth_path = auth_file_path();
    if !auth_path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&auth_path).ok()?;
    toml::from_str(&content).ok()
}

/// Save auth info to file
pub fn save_auth(auth: &AuthInfo) -> Result<()> {
    let auth_path = auth_file_path();

    // Ensure parent directory exists
    if let Some(parent) = auth_path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create auth directory")?;
        // Set directory permissions to 0o700 (owner only) on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    let content = toml::to_string_pretty(auth).context("Failed to serialize auth info")?;
    super::write_auth_file_secure(&auth_path, &content).context("Failed to write auth file")?;

    // Set file permissions to 0o600 (owner read/write only) on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
            .context("Failed to set auth file permissions")?;
    }

    println!("  Auth saved to: {}\n", auth_path.display());

    Ok(())
}

/// Get path to auth file
pub fn auth_file_path() -> std::path::PathBuf {
    crate::config::Config::config_dir().join("auth.toml")
}

/// Check if user is logged in
pub fn is_logged_in() -> bool {
    get_stored_auth().is_some()
}

/// Get current user info (if logged in)
pub fn current_user() -> Option<UserInfo> {
    get_stored_auth().map(|auth| auth.user)
}

/// Parse a user-pasted OAuth callback URL into (code, state).
///
/// Accepts any URL with a query string containing `code` and `state`.
/// Rejects raw `code` without URL context — state validation is CSRF
/// protection and we want the full round-trip, not a manually typed code.
#[allow(dead_code)]
fn parse_pasted_callback(input: &str) -> Result<(String, String)> {
    // Defensively strip bracketed-paste markers. The TUI disables DECSET
    // 2004 before calling us, but a user pasting into a terminal we didn't
    // configure (or with a stray prior session) can still deliver these.
    let cleaned = input
        .trim()
        .trim_start_matches("\x1b[200~")
        .trim_end_matches("\x1b[201~")
        .trim();

    let query_start = cleaned.find('?').context(
        "Could not parse callback URL — paste the full http://127.0.0.1:8765/callback?... URL",
    )?;
    let query = &cleaned[query_start + 1..];

    let params: HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts
                .next()
                .map(|v| urlencoding_decode(v))
                .unwrap_or_default();
            Some((key.to_string(), value))
        })
        .collect();

    if let Some(error) = params.get("error") {
        let desc = params
            .get("error_description")
            .map(|s| s.as_str())
            .unwrap_or(error);
        anyhow::bail!("OAuth error: {}", desc);
    }

    let code = params
        .get("code")
        .context("Callback URL missing 'code' parameter")?
        .clone();
    let state = params
        .get("state")
        .context("Callback URL missing 'state' parameter (paste the full URL, not just the code)")?
        .clone();

    Ok((code, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_force_login_removes_trailing_param() {
        let url = "https://atomgit.com/oauth/authorize?client_id=abc&state=xyz&force_login=true";
        assert_eq!(
            strip_force_login(url),
            "https://atomgit.com/oauth/authorize?client_id=abc&state=xyz"
        );
    }

    #[test]
    fn strip_force_login_removes_middle_param() {
        let url = "https://atomgit.com/oauth/authorize?client_id=abc&force_login=true&state=xyz";
        assert_eq!(
            strip_force_login(url),
            "https://atomgit.com/oauth/authorize?client_id=abc&state=xyz"
        );
    }

    #[test]
    fn strip_force_login_removes_only_param() {
        let url = "https://atomgit.com/oauth/authorize?force_login=true";
        assert_eq!(
            strip_force_login(url),
            "https://atomgit.com/oauth/authorize"
        );
    }

    #[test]
    fn strip_force_login_removes_first_of_many() {
        let url = "https://atomgit.com/oauth/authorize?force_login=true&state=xyz";
        assert_eq!(
            strip_force_login(url),
            "https://atomgit.com/oauth/authorize?state=xyz"
        );
    }

    #[test]
    fn strip_force_login_passthrough_when_absent() {
        let url = "https://atomgit.com/oauth/authorize?client_id=abc&state=xyz";
        assert_eq!(strip_force_login(url), url);
    }

    #[test]
    fn parse_happy_path_loopback_url() {
        let (code, state) =
            parse_pasted_callback("http://127.0.0.1:8765/callback?code=abc&state=xyz").unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_any_host_with_extra_params() {
        let (code, state) =
            parse_pasted_callback("https://example.com/x?foo=1&code=abc&state=xyz&bar=2").unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_missing_state_errors_with_full_url_hint() {
        let err = parse_pasted_callback("http://127.0.0.1:8765/callback?code=abc")
            .unwrap_err()
            .to_string();
        assert!(err.contains("state"), "got: {err}");
        assert!(err.contains("full URL"), "got: {err}");
    }

    #[test]
    fn parse_missing_code_errors() {
        let err = parse_pasted_callback("http://127.0.0.1:8765/callback?state=xyz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("code"), "got: {err}");
    }

    #[test]
    fn parse_error_response_includes_description() {
        let err = parse_pasted_callback(
            "http://127.0.0.1:8765/callback?error=access_denied&error_description=User+denied",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("User denied"), "got: {err}");
    }

    #[test]
    fn parse_not_a_url_errors() {
        let err = parse_pasted_callback("this is not a url")
            .unwrap_err()
            .to_string();
        assert!(err.contains("full"), "got: {err}");
    }

    #[test]
    fn parse_url_encoded_state_is_decoded() {
        let (_, state) =
            parse_pasted_callback("http://127.0.0.1:8765/callback?code=c&state=atomcode_%3Atest")
                .unwrap();
        assert_eq!(state, "atomcode_:test");
    }

    #[test]
    fn parse_strips_bracketed_paste_markers() {
        let input = "\x1b[200~http://127.0.0.1:8765/callback?code=abc&state=xyz\x1b[201~";
        let (code, state) = parse_pasted_callback(input).unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_trims_surrounding_whitespace() {
        let (code, state) =
            parse_pasted_callback("   http://127.0.0.1:8765/callback?code=abc&state=xyz\n")
                .unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    // ----- classify_input (ESC vs escape-sequence disambiguation) -----

    #[test]
    fn classify_input_bare_esc_cancels() {
        assert_eq!(classify_input(&[0x1B]), EscOutcome::Cancelled);
    }

    #[test]
    fn classify_input_arrow_key_ignored() {
        // Up arrow = ESC [ A — three bytes arriving in a single read.
        assert_eq!(classify_input(b"\x1B[A"), EscOutcome::OtherInput);
    }

    #[test]
    fn classify_input_alt_letter_ignored() {
        // Alt+a delivered as ESC + 'a' on most terminals.
        assert_eq!(classify_input(b"\x1Ba"), EscOutcome::OtherInput);
    }

    #[test]
    fn classify_input_normal_byte_ignored() {
        assert_eq!(classify_input(b"q"), EscOutcome::OtherInput);
    }

    #[test]
    fn classify_input_empty_is_timeout() {
        assert_eq!(classify_input(&[]), EscOutcome::Timeout);
    }

    #[test]
    fn classify_input_pasted_text_ignored() {
        assert_eq!(classify_input(b"hello\n"), EscOutcome::OtherInput);
    }

    #[test]
    fn classify_input_csi_color_code_ignored() {
        // Bracketed-paste / OSC sequences and other CSI fragments must
        // not be mistaken for ESC. `\x1B[31m` = SGR red.
        assert_eq!(classify_input(b"\x1B[31m"), EscOutcome::OtherInput);
    }
}
