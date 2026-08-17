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

use atomcode_config::config::Config;

/// Sanitize a user-supplied base URL: add `http://` if no scheme is present,
/// and strip trailing `/` so path concatenation never produces `//`.
fn sanitize_base_url(raw: &str) -> String {
    let trimmed = raw.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{}", trimmed)
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// Return the Platform server base URL, resolved once by
/// [`atomcode_config::endpoints::platform_server`] (deployment profile +
/// `ATOMCODE_PLATFORM_SERVER` override) and cached for the process lifetime.
/// This ensures all URL-derived functions within a single login/session flow
/// target the same server even if the env var changes mid-flight.
fn platform_base_url() -> &'static str {
    use std::sync::OnceLock;
    static BASE: OnceLock<String> = OnceLock::new();
    BASE.get_or_init(|| sanitize_base_url(atomcode_config::endpoints::platform_server()))
}

/// Platform server URLs (derived from `ATOMCODE_PLATFORM_SERVER`).
pub fn platform_broker_url() -> String {
    platform_base_url().to_string()
}
pub fn platform_login_url() -> String {
    format!("{}/auth/login", platform_base_url())
}
pub fn platform_check_url() -> String {
    format!("{}/auth/check", platform_base_url())
}
pub fn platform_token_url() -> String {
    format!("{}/auth/token", platform_base_url())
}
pub fn platform_exchange_url() -> String {
    format!("{}/oauth/exchange", platform_base_url())
}
pub fn platform_refresh_url() -> String {
    format!("{}/oauth/refresh", platform_base_url())
}

/// Blocking HTTP client pre-configured with `ATOMCODE_USER_AGENT`. Every
/// OAuth-side request must carry the token or AtomGit's gate rejects it.
/// Centralized so a future UA format change (e.g. append install-id)
/// happens in one spot rather than at each `Client::new()` site.
/// Apply the process proxy policy to a blocking reqwest client builder: honor `no_proxy`
/// mode, otherwise leave reqwest's env-based proxy detection intact. Inlined from the former
/// `atomcode_core::proxy` so this crate stays a leaf — it reads only the `atomcode_config::proxy`
/// env contract (no HTTP-stack glue that would pull in core).
fn apply_blocking_proxy_policy(
    builder: reqwest::blocking::ClientBuilder,
    force_tls12: bool,
) -> reqwest::blocking::ClientBuilder {
    atomcode_config::proxy::ensure_runtime_initialized();
    let builder = if std::env::var(atomcode_config::proxy::MODE_ENV)
        .ok()
        .as_deref()
        == Some(atomcode_config::proxy::ProxyMode::NoProxy.as_str())
    {
        builder.no_proxy()
    } else {
        builder
    };
    // Cap at TLS 1.2 when a TLS-1.3-hostile network has been detected/requested
    // (some paths RST the TLS 1.3 handshake to acs.atomgit.com → os error 10054).
    if force_tls12 {
        builder.max_tls_version(reqwest::tls::Version::TLS_1_2)
    } else {
        builder
    }
}

/// The localized network hint for a login HTTP failure, or `None` when the
/// error is not connection-level. Connect resets (e.g. Windows os error 10054)
/// and timeouts mean the endpoint was unreachable on THIS client's path while a
/// browser may still work — usually a proxy/firewall difference.
fn network_connect_hint(err: &reqwest::Error) -> Option<std::borrow::Cow<'static, str>> {
    if err.is_connect() || err.is_timeout() {
        Some(atomcode_config::i18n::t(
            atomcode_config::i18n::Msg::NetworkConnectHint,
        ))
    } else {
        None
    }
}

/// Wrap a login HTTP `send()` result with a failure context, appending the
/// network hint as INNER context (below `ctx`) when the error is
/// connect/timeout — so the display leads with `ctx` and supplements with
/// proxy guidance. Shared by the login GET/exchange calls.
fn with_login_context<T>(result: reqwest::Result<T>, ctx: &'static str) -> Result<T> {
    result.map_err(|e| {
        let hint = network_connect_hint(&e);
        let mut err = anyhow::Error::new(e);
        if let Some(h) = hint {
            err = err.context(h.into_owned());
        }
        err.context(ctx)
    })
}

fn blocking_client() -> Result<reqwest::blocking::Client> {
    blocking_client_with_tls12(atomcode_config::tls::should_cap_url(platform_base_url()))
}

fn blocking_client_with_tls12(force_tls12: bool) -> Result<reqwest::blocking::Client> {
    // Hard timeouts here too — the `get_valid_token` path calls
    // `refresh_access_token` synchronously whenever a stored token
    // looks expired, and that runs on the main TUI thread (via
    // `Client::from_stored_auth` → `/status`, drift monitor, etc.).
    // Without a cap, a slow or unreachable OAuth server would hang
    // the UI indefinitely. Same budget as the coding-plan client.
    //
    // Return `Result` rather than falling back to `Client::new()`: that
    // helper *panics* on TLS/resolver init failure, and with `panic =
    // "abort"` that takes down the whole process. `build()` reports the
    // same failure as a catchable `Err` — propagate it.
    apply_blocking_proxy_policy(reqwest::blocking::Client::builder(), force_tls12)
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(crate::ATOMCODE_USER_AGENT)
        .build()
        .context("failed to build OAuth HTTP client")
}

fn pending_invite_for_login() -> (Option<String>, Option<uuid::Uuid>) {
    match atomcode_telemetry::pending_invite::load(&Config::config_dir()) {
        Some(invite) => (Some(invite.invite_code), Some(invite.install_uuid)),
        None => (None, None),
    }
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

/// Minimal, internally coherent credentials needed to authenticate one gateway request.
/// The refresh token and profile fields never leave the auth owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidAuthSession {
    pub access_token: String,
    pub user_id: String,
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
//
// On Windows `wait_for_esc_or_timeout` always returns `Timeout` (no
// poll(2) over stdin), so `Cancelled` and `OtherInput` are constructed
// only on Unix. The variants must still exist on Windows because
// `classify_input` and its tests reference them — `cargo test` runs on
// every platform. Suppress the dead-code warning rather than gate the
// type, so the test surface stays portable.
#[cfg_attr(target_os = "windows", allow(dead_code))]
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
//
// Only called from the Unix `wait_for_esc_or_timeout`. Kept callable on
// Windows because the unit-test module exercises it on every platform —
// the logic is byte-pattern matching, no platform deps. `dead_code`
// suppression scoped to Windows so Unix still gets the warning if a
// future change makes it genuinely unused there.
#[cfg_attr(target_os = "windows", allow(dead_code))]
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
    client: Option<reqwest::blocking::Client>,
}

impl Drop for LoginSession {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = std::thread::spawn(move || drop(client));
        }
    }
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
        let client = match &self.client {
            Some(c) => c.clone(),
            None => return Ok(PollOutcome::Pending),
        };
        let state = self.state.clone();
        std::thread::spawn(move || {
            let resp = with_login_context(
                client
                    .get(platform_check_url())
                    .query(&[("state", &state)])
                    .send(),
                "Failed to call /auth/check",
            )?;
            if resp.status().is_success() {
                if let Ok(check) = resp.json::<PlatformCheckResponse>() {
                    if check.valid {
                        return Ok(PollOutcome::Authorized);
                    }
                }
            }
            Ok(PollOutcome::Pending)
        })
        .join()
        .map_err(|_| anyhow::anyhow!("poll_once thread panicked"))?
    }

    /// Final step: `/auth/token` exchange + `LoginSuccess` telemetry.
    /// Consumes the session — only call after `poll_once` returned
    /// `Authorized`.
    pub fn finish(mut self, tel: Option<&Arc<Telemetry>>) -> Result<AuthInfo> {
        let client = self.client.take();
        let state = self.state.clone();
        let tel = tel.cloned();
        std::thread::spawn(move || {
            let client = match client {
                Some(c) => c,
                None => blocking_client()?,
            };
            let resp = with_login_context(
                client
                    .get(platform_token_url())
                    .query(&[("state", &state)])
                    .send(),
                "Failed to call /auth/token",
            )?;
            let token_resp: PlatformTokenResponse = resp
                .json()
                .context("Failed to parse /auth/token response")?;

            // `duration_since(UNIX_EPOCH)` only fails when the wall clock
            // is before 1970 — a misconfigured VM clock at boot is the
            // realistic trigger. Treat that as `created_at = 0`: the
            // expiry check downstream will see the token as immediately
            // stale and force a refresh / re-login rather than panicking
            // out of the OAuth callback (#45).
            let created_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

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

            if let Some(t) = tel.as_ref() {
                // Push account_id onto the telemetry handle BEFORE emitting
                // login_success so the event itself — and every subsequent event in
                // this process — carries the id. The handle-level setter outlives
                // any task-local scope, so events emitted outside the main scope
                // (e.g. before scope is entered, or from spawned tasks) inherit it.
                t.set_account_id(Some(auth_info.user.id.to_string()));
                let (invite_code, install_uuid) = pending_invite_for_login();
                let event = Event::LoginSuccess {
                    invite_code,
                    install_uuid,
                };
                if let Err(e) = t.track_durable_sync(event.clone()) {
                    tracing::warn!(
                        ?e,
                        "login_success durable enqueue failed; falling back to async telemetry"
                    );
                    t.track(event);
                }
            }

            Ok(auth_info)
        })
        .join()
        .map_err(|_| anyhow::anyhow!("finish thread panicked"))?
    }
}

/// Begin OAuth login: call `/auth/login`, return a session containing
/// the auth URL + state. Cheap (one HTTP round-trip), never blocks on
/// user action — separated from polling so callers can render the URL
/// before yielding control to the wait loop.
pub fn start_login() -> Result<LoginSession> {
    std::thread::spawn(move || {
        // First attempt uses the current TLS policy (TLS 1.3 by default). If the
        // connection is RST at the handshake — the signature of a middlebox that
        // resets TLS 1.3 to acs.atomgit.com (Windows `os error 10054`) — retry
        // once with a fresh TLS-1.2 client. Only a successful retry latches the
        // managed-endpoint policy for later auth/codingplan/provider clients.
        // Third-party endpoints remain unaffected.
        let login_url = platform_login_url();
        let was_capped = atomcode_config::tls::should_cap_url(&login_url);
        match attempt_login(was_capped) {
            Err(first)
                if atomcode_config::tls::should_try_fallback(
                    &login_url,
                    was_capped,
                    is_connect_error(&first),
                ) =>
            {
                match attempt_login(true) {
                    Ok(session) => {
                        atomcode_config::tls::latch_managed_tls12();
                        Ok(session)
                    }
                    Err(fallback) => Err(fallback.context(format!(
                        "TLS 1.2 fallback also failed after initial error: {first:#}"
                    ))),
                }
            }
            other => other,
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("start_login thread panicked"))?
}

/// One `/auth/login` round-trip with a freshly built client (so a retry picks up
/// a changed TLS policy). `Client::new()` panics on TLS/resolver init failure;
/// with `panic = "abort"` that aborts the process before the QR can even render,
/// so `blocking_client()` builds fallibly and we surface a recoverable error.
fn attempt_login(force_tls12: bool) -> Result<LoginSession> {
    let client = blocking_client_with_tls12(force_tls12)?;
    let sent = client
        .get(platform_login_url())
        .query(&[("provider", "atomgit")])
        .send();
    let resp = with_login_context(sent, "Failed to call /auth/login")?;
    let resp: PlatformLoginResponse = resp
        .json()
        .context("Failed to parse /auth/login response")?;
    Ok(LoginSession {
        state: resp.state,
        login_url: strip_force_login(&resp.login_url),
        client: Some(client),
    })
}

/// True iff the error chain carries a reqwest connect-level failure — the class
/// that includes a TLS-handshake RST (`os error 10054`). Used to decide whether a
/// TLS 1.2 downgrade retry is worth attempting; a status/parse error is not.
fn is_connect_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|e| e.is_connect())
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
    // Pre-1970 wall clock falls back to 0 instead of panicking. The
    // value is folded into a CSRF state string so a deterministic
    // 0-derived value is fine for the dead-code path (#45 audit).
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("atomcode_{}", timestamp)
}

/// Open browser with the authorization URL.
///
/// `pub` because TUI modals (e.g. the QR-login onboarding step) need to
/// invoke the same platform browser launch the CLI flow already does via
/// `LoginSession::open_browser_best_effort` — callers without a live
/// `LoginSession` only carry the URL string, so they go through this
/// free function directly.
#[cfg(target_os = "macos")]
pub fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "windows")]
pub fn open_browser(url: &str) -> Result<()> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    // NUL-terminated UTF-16 for the Win32 `W` API.
    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    // `ShellExecuteW(NULL, "open", url, …)` is exactly what clicking a hyperlink does:
    // the URL is a single opaque argument (no shell / command-line parsing at all), so
    // `?` and `&` in our `…/?token=…&sync=1` URL are pure data, and it reliably routes to
    // the default browser whether or not one is already running. The prior `explorer.exe
    // <url>` was unreliable — when it couldn't resolve the arg as a URL (notably on a cold
    // browser launch) it opened a File Explorer *folder* window (Documents / This PC)
    // instead, and `.spawn()` still reported success so the `cmd start` fallback never
    // fired. `cmd /C start "" "<url>"` in turn mishandles `&`. ShellExecuteW sidesteps all
    // three problems and avoids the cmd console flash.
    let verb = wide("open");
    let file = wide(url);
    // Docs: ShellExecuteW returns a value > 32 on success (it's an HINSTANCE-typed status).
    let rc = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if rc as isize > 32 {
        return Ok(());
    }

    // ShellExecute failed (rare). Fall back to the legacy launchers.
    if std::process::Command::new("explorer")
        .arg(url)
        .spawn()
        .is_ok()
    {
        return Ok(());
    }
    std::process::Command::new("cmd")
        .raw_arg(format!("/C start \"\" \"{}\"", url))
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn open_browser(_url: &str) -> Result<()> {
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

/// Refresh the currently-stored auth via Platform Broker and save it to disk.
///
/// `auth.access_token` is used only to detect a concurrent refresh: another
/// window may already have rotated the token, in which case the newer stored
/// credential is returned without a second broker call. An in-memory `AuthInfo`
/// that was never persisted is therefore NOT refreshed in isolation. Account
/// identity is not enforced here — see [`recover_auth_after_unauthorized`] for
/// the account-checked recovery entry point.
pub fn refresh_access_token(auth: &AuthInfo) -> Result<AuthInfo> {
    refresh_auth_if_current(&auth.access_token, None)
}

#[derive(Debug, thiserror::Error)]
#[error("No refresh_token available — please /login again")]
struct MissingRefreshToken;

#[derive(Debug, thiserror::Error)]
#[error("Token refresh failed ({status}): {body}")]
struct RefreshHttpStatus {
    status: u16,
    body: String,
}

/// The broker returned a success status but a body we couldn't parse. This is
/// deterministic (retrying re-parses the same bytes), so recovery treats it as a
/// re-authentication prompt rather than a transient decode failure that would
/// retry-loop forever against a bad response.
#[derive(Debug, thiserror::Error)]
#[error("Unexpected broker response — please /login again")]
struct UnexpectedBrokerResponse;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthRecoveryFailureKind {
    Transient,
    ReauthenticationRequired,
    Local,
}

/// Preserve the distinction used by provider recovery: transport/server
/// failures may succeed on a later OPEN retry; rejected refresh credentials
/// require `/login`; filesystem/account consistency errors are local failures.
pub fn classify_auth_recovery_error(error: &anyhow::Error) -> AuthRecoveryFailureKind {
    for cause in error.chain() {
        if cause.downcast_ref::<MissingRefreshToken>().is_some() {
            return AuthRecoveryFailureKind::ReauthenticationRequired;
        }
        if cause.downcast_ref::<UnexpectedBrokerResponse>().is_some() {
            return AuthRecoveryFailureKind::ReauthenticationRequired;
        }
        if let Some(status) = cause.downcast_ref::<RefreshHttpStatus>() {
            // Intentionally broader than the OPEN loop's `retry::is_retryable_status`
            // set: recovery only answers "retry vs re-login", so every 5xx is
            // transient and every other rejecting status maps to a `/login` prompt
            // rather than an opaque local error (a relocated broker returning 404,
            // a proxy 511, etc.). This deliberately does NOT mirror the retry
            // module — the two answer different questions, so no sync is required.
            return if matches!(status.status, 408 | 425 | 429) || status.status >= 500 {
                AuthRecoveryFailureKind::Transient
            } else if status.status >= 400 {
                AuthRecoveryFailureKind::ReauthenticationRequired
            } else {
                AuthRecoveryFailureKind::Local
            };
        }
        if let Some(request) = cause.downcast_ref::<reqwest::Error>() {
            if request.is_timeout()
                || request.is_connect()
                || request.is_request()
                || request.is_body()
                || request.is_decode()
            {
                return AuthRecoveryFailureKind::Transient;
            }
        }
    }
    AuthRecoveryFailureKind::Local
}

/// Caller must hold `auth-refresh.lock`.
fn refresh_access_token_unlocked(auth: &AuthInfo) -> Result<AuthInfo> {
    let auth = auth.clone();
    std::thread::spawn(move || {
        let refresh_token = auth
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow::Error::new(MissingRefreshToken))?;

        let client = blocking_client()?;

        // Call Platform Broker API for refresh
        let response = client
            .post(platform_refresh_url())
            .json(&serde_json::json!({ "refresh_token": refresh_token }))
            .send()
            .context("Failed to send refresh token request to broker")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(anyhow::Error::new(RefreshHttpStatus {
                status: status.as_u16(),
                body,
            }));
        }

        #[derive(Deserialize)]
        struct BrokerResponse {
            access_token: String,
            token_type: Option<String>,
            expires_in: Option<i64>,
            refresh_token: Option<String>,
            user: Option<PlatformUserInfo>,
        }

        // Read the body as text first so a mid-body transport failure surfaces as
        // a (transient) reqwest error, while a fully-received but unparseable 2xx
        // body becomes a terminal error — not a retryable decode error that would
        // loop against a deterministically-bad response.
        let body_text = response
            .text()
            .context("Failed to read broker response body")?;
        let broker_resp: BrokerResponse = serde_json::from_str(&body_text)
            .map_err(|_| anyhow::Error::new(UnexpectedBrokerResponse))?;

        // Pre-1970 wall clock would otherwise panic on `unwrap` and lose
        // the refresh result. Falling back to 0 forces the next token
        // check to refresh again — safer than crashing the broker path.
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

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

        save_auth_unlocked(&new_auth)?;
        Ok(new_auth)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("refresh_access_token thread panicked"))?
}

/// Recover from a server-side 401 for a token that the local expiry clock still
/// considered valid.
///
/// The lock is cross-process because refresh tokens may rotate: multiple AtomCode
/// windows must not consume the same refresh token concurrently. After acquiring
/// it, reload `auth.toml`; another process may already have refreshed, in which
/// case the newer credential is returned without another authority call.
pub fn recover_auth_after_unauthorized(
    rejected_access_token: &str,
    expected_user_id: &str,
) -> Result<ValidAuthSession> {
    let auth = refresh_auth_if_current(rejected_access_token, Some(expected_user_id))?;
    if auth.access_token.trim().is_empty() || auth.user.id.trim().is_empty() {
        anyhow::bail!("Invalid auth.toml — please use /login first");
    }
    Ok(ValidAuthSession {
        access_token: auth.access_token,
        user_id: auth.user.id,
    })
}

/// Serialize refresh-token consumption across threads and processes. The
/// rejected/current token is compared again after taking the lock so a waiter
/// observes credentials refreshed by the winner instead of refreshing twice.
fn refresh_auth_if_current(
    rejected_access_token: &str,
    expected_user_id: Option<&str>,
) -> Result<AuthInfo> {
    with_auth_lock(|| {
        let auth = get_stored_auth().context("Not logged in — please use /login first")?;
        // Only the account-checked recovery entry point enforces identity. The
        // proactive-refresh path passes `None`: it just needs any currently-valid
        // stored token, so a concurrent login as a different account should be
        // adopted, not turned into a spurious "Login account changed" hard failure.
        if let Some(expected) = expected_user_id {
            if auth.user.id != expected {
                anyhow::bail!("Login account changed — please retry the request");
            }
        }
        if auth.access_token != rejected_access_token {
            return Ok(auth);
        }
        refresh_access_token_unlocked(&auth)
    })
}

fn get_valid_auth_info() -> Result<AuthInfo> {
    let auth = get_stored_auth().context("Not logged in — please use /login first")?;

    // Check if token is expired (with 5-minute safety margin)
    if let Some(expires_in) = auth.expires_in {
        // A pre-1970 wall clock would otherwise panic here — and
        // get_valid_token runs on EVERY authenticated API call (atomgit /
        // coding_plan clients), not just /login. Treat that as expired
        // (now = i64::MAX) so it force-refreshes instead of crashing (#45).
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(i64::MAX);
        let expires_at = auth.created_at + expires_in;

        if now >= expires_at - 300 {
            // Token expired or about to expire — serialize refresh-token
            // consumption and re-check auth.toml after taking the lock.
            match refresh_auth_if_current(&auth.access_token, None) {
                Ok(new_auth) => return Ok(new_auth),
                Err(e) => anyhow::bail!("Token expired and refresh failed: {}", e),
            }
        }
    } else if auth.created_at == 0 {
        // Legacy auth.toml without created_at — no way to know if expired,
        // try refresh if refresh_token is available, otherwise use as-is.
        if auth.refresh_token.is_some() {
            if let Ok(new_auth) = refresh_auth_if_current(&auth.access_token, None) {
                return Ok(new_auth);
            }
        }
    }

    Ok(auth)
}

/// Get a valid token and its matching user identity from one auth snapshot.
/// Refresh, when needed, happens before either value is projected so callers cannot
/// accidentally combine a new token with a stale user id.
pub fn get_valid_auth_session() -> Result<ValidAuthSession> {
    let auth = get_valid_auth_info()?;
    if auth.access_token.trim().is_empty() || auth.user.id.trim().is_empty() {
        anyhow::bail!("Invalid auth.toml — please use /login first");
    }
    Ok(ValidAuthSession {
        access_token: auth.access_token,
        user_id: auth.user.id,
    })
}

/// Get a valid access token, refreshing automatically if expired.
/// Returns the access token string ready to use.
pub fn get_valid_token() -> Result<String> {
    let auth = get_valid_auth_info()?;
    if auth.access_token.trim().is_empty() {
        anyhow::bail!("Invalid auth.toml — please use /login first");
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
    // Absent file ⇒ already logged out. Return before touching the lock so a
    // never-logged-in user's /logout stays a pure no-op — no directory or lock
    // file created, and no failure on a read-only HOME.
    if !auth_path.exists() {
        return Ok(());
    }
    with_auth_lock(|| {
        if auth_path.exists() {
            std::fs::remove_file(&auth_path).context("Failed to remove auth file")?;
        }
        Ok(())
    })
}

/// Get stored auth info
pub fn get_stored_auth() -> Option<AuthInfo> {
    let auth_path = auth_file_path();
    read_stored_auth_at(&auth_path).ok().flatten()
}

/// Read credentials without collapsing transient I/O or parse failures into a
/// confirmed logout. Credential writers replace the file atomically, so this
/// remains non-blocking even while a refresh request holds the writer lock.
pub fn get_stored_auth_checked() -> Result<Option<AuthInfo>> {
    read_stored_auth_at(&auth_file_path())
}

fn read_stored_auth_at(auth_path: &std::path::Path) -> Result<Option<AuthInfo>> {
    let content = match std::fs::read_to_string(auth_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read auth file at {}", auth_path.display()));
        }
    };
    toml::from_str(&content)
        .map(Some)
        .with_context(|| format!("Failed to parse auth file at {}", auth_path.display()))
}

/// Save auth info to file
pub fn save_auth(auth: &AuthInfo) -> Result<()> {
    with_auth_lock(|| save_auth_unlocked(auth))
}

/// Execute one authentication-store transaction. Every writer uses this seam so
/// a refresh response cannot overwrite a concurrent login/logout from another
/// thread or process.
fn with_auth_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let auth_path = auth_file_path();
    let parent = auth_path
        .parent()
        .context("Invalid auth file path — please use /login again")?;
    std::fs::create_dir_all(parent).context("Failed to create auth directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort: a directory we can write to but can't chmod (unusual
        // mounts, or a dir owned by another user) must not block login / refresh /
        // logout. The file itself is still written 0600 by write_auth_file_secure.
        if let Err(error) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        {
            tracing::warn!(%error, "failed to tighten auth directory permissions");
        }
    }
    with_auth_lock_file(&parent.join("auth-refresh.lock"), operation)
}

fn with_auth_lock_file<T>(
    lock_path: &std::path::Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
        .context("Failed to open auth refresh lock")?;
    lock.lock_exclusive()
        .context("Failed to acquire auth refresh lock")?;
    operation()
}

/// Caller must hold `auth-refresh.lock`.
fn save_auth_unlocked(auth: &AuthInfo) -> Result<()> {
    let auth_path = auth_file_path();
    let content = toml::to_string_pretty(auth).context("Failed to serialize auth info")?;
    super::write_auth_file_secure(&auth_path, &content).context("Failed to write auth file")?;

    // Set file permissions to 0o600 (owner read/write only) on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
            .context("Failed to set auth file permissions")?;
    }

    // No stdout output here. `save_auth` is called from CLI flows, TUI
    // slash commands, the daemon, AND the silent in-chat 401 → refresh
    // path. Printing here would corrupt the TUI input box on the silent
    // refresh path (the cursor sits in the prompt and `println!` bypasses
    // the renderer). CLI callers print their own user-facing success
    // message right after calling this.
    Ok(())
}

/// Get path to auth file
pub fn auth_file_path() -> std::path::PathBuf {
    atomcode_config::config::Config::config_dir().join("auth.toml")
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
    fn auth_recovery_failure_classifies_statuses_and_local_errors() {
        let transient = anyhow::Error::new(RefreshHttpStatus {
            status: 503,
            body: "unavailable".to_string(),
        });
        assert_eq!(
            classify_auth_recovery_error(&transient),
            AuthRecoveryFailureKind::Transient
        );

        let rejected = anyhow::Error::new(RefreshHttpStatus {
            status: 401,
            body: "invalid refresh token".to_string(),
        });
        assert_eq!(
            classify_auth_recovery_error(&rejected),
            AuthRecoveryFailureKind::ReauthenticationRequired
        );

        // An unlisted 5xx (e.g. 501/505) is still transient; an unlisted 4xx
        // (e.g. a relocated broker returning 404) prompts re-login instead of
        // falling through to an opaque local error.
        let unlisted_server = anyhow::Error::new(RefreshHttpStatus {
            status: 501,
            body: "not implemented".to_string(),
        });
        assert_eq!(
            classify_auth_recovery_error(&unlisted_server),
            AuthRecoveryFailureKind::Transient
        );
        let unlisted_client = anyhow::Error::new(RefreshHttpStatus {
            status: 404,
            body: "not found".to_string(),
        });
        assert_eq!(
            classify_auth_recovery_error(&unlisted_client),
            AuthRecoveryFailureKind::ReauthenticationRequired
        );

        // A successful-but-unparseable broker body is deterministic → re-login,
        // not a retryable decode failure.
        let unparseable = anyhow::Error::new(UnexpectedBrokerResponse);
        assert_eq!(
            classify_auth_recovery_error(&unparseable),
            AuthRecoveryFailureKind::ReauthenticationRequired
        );

        let local = anyhow::anyhow!("failed to persist auth.toml");
        assert_eq!(
            classify_auth_recovery_error(&local),
            AuthRecoveryFailureKind::Local
        );
    }

    #[test]
    fn auth_store_lock_serializes_concurrent_writers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("auth-refresh.lock");
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();

        for _ in 0..2 {
            let lock_path = lock_path.clone();
            let active = active.clone();
            let max_active = max_active.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                with_auth_lock_file(&lock_path, || {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn checked_auth_read_distinguishes_invalid_data_from_logout() {
        let temp = tempfile::tempdir().unwrap();
        let auth_path = temp.path().join("auth.toml");
        assert!(read_stored_auth_at(&auth_path).unwrap().is_none());

        std::fs::write(&auth_path, "").unwrap();
        assert!(read_stored_auth_at(&auth_path).is_err());
        assert!(read_stored_auth_at(&auth_path).ok().flatten().is_none());
    }

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

    // ----- sanitize_base_url -----

    #[test]
    fn sanitize_adds_http_if_no_scheme() {
        assert_eq!(sanitize_base_url("127.0.0.1:8765"), "http://127.0.0.1:8765");
    }

    #[test]
    fn sanitize_preserves_http_scheme() {
        assert_eq!(
            sanitize_base_url("http://127.0.0.1:8765"),
            "http://127.0.0.1:8765"
        );
    }

    #[test]
    fn sanitize_preserves_https_scheme() {
        assert_eq!(
            sanitize_base_url("https://acs.example.com"),
            "https://acs.example.com"
        );
    }

    #[test]
    fn sanitize_strips_trailing_slash() {
        assert_eq!(
            sanitize_base_url("http://127.0.0.1:8765/"),
            "http://127.0.0.1:8765"
        );
        assert_eq!(
            sanitize_base_url("http://127.0.0.1:8765///"),
            "http://127.0.0.1:8765"
        );
    }

    #[test]
    fn sanitize_trims_whitespace() {
        assert_eq!(
            sanitize_base_url("  http://127.0.0.1:8765  "),
            "http://127.0.0.1:8765"
        );
    }

    #[test]
    fn sanitize_no_scheme_with_trailing_slash() {
        assert_eq!(
            sanitize_base_url("127.0.0.1:8765/"),
            "http://127.0.0.1:8765"
        );
    }

    #[test]
    fn connect_error_yields_hint_timeout_does_too() {
        // A builder timeout produces a timeout-class reqwest error.
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(1))
            .timeout(std::time::Duration::from_millis(1))
            .build()
            .unwrap();
        // 203.0.113.0/24 is TEST-NET-3 (RFC 5737) — guaranteed unroutable, so this
        // fails at connect/timeout without depending on any real host.
        let err = client
            .get("http://203.0.113.1:81/")
            .send()
            .expect_err("must fail");
        assert!(
            super::network_connect_hint(&err).is_some(),
            "connect/timeout error must yield a hint, got: {err:?}"
        );
    }

    #[test]
    fn non_network_error_yields_no_hint() {
        // A decode error is neither connect nor timeout → no hint.
        // Build any reqwest::Error that is not connect/timeout by parsing a bad URL.
        let err = reqwest::blocking::Client::new()
            .get("http://")
            .build()
            .expect_err("bad url builds an error");
        assert!(super::network_connect_hint(&err).is_none(), "got: {err:?}");
    }

    #[test]
    fn with_login_context_leads_with_ctx_and_adds_hint_on_connect_error() {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(1))
            .timeout(std::time::Duration::from_millis(1))
            .build()
            .unwrap();
        // TEST-NET-3 (RFC 5737) — unroutable, fails at connect/timeout.
        let res = client.get("http://203.0.113.1:81/").send();
        let err = super::with_login_context(res, "Failed to call /auth/test").unwrap_err();
        let chain = format!("{err:#}");
        assert!(
            chain.starts_with("Failed to call /auth/test"),
            "ctx must lead: {chain}"
        );
        assert!(
            chain.contains("/proxy") || chain.contains("HTTPS_PROXY"),
            "hint present: {chain}"
        );
    }
}
