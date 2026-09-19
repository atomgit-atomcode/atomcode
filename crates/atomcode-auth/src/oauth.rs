use std::io;
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use atomcode_telemetry::{Event, Telemetry};

use atomcode_config::config::Config;
// The store half. One-way: this crate puts away what it got and reads what it
// has to refresh; nothing over there knows a server exists.
use atomcode_credentials::{
    get_stored_auth, save_auth_unlocked, with_auth_lock, AuthInfo, UserInfo,
};

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

pub(crate) fn blocking_client_with_tls12(force_tls12: bool) -> Result<reqwest::blocking::Client> {
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
// Windows has no `poll(2)` over stdin, so the same cancellation problem is
// unsolvable there. We follow the same pattern: `CbreakGuard` is a
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

/// Map an `/auth/check` HTTP result to a [`PollOutcome`]. `Authorized`
/// only when the request succeeded AND the body parsed as `valid=true`;
/// every other success shape ("not yet") is `Pending`. Pure decision logic,
/// split out so it's unit-testable without a live server.
fn interpret_check(status_success: bool, valid: bool) -> PollOutcome {
    if status_success && valid {
        PollOutcome::Authorized
    } else {
        PollOutcome::Pending
    }
}

/// One blocking `/auth/check` round-trip. Shared by [`LoginSession::poll_once`]
/// and the background poller [`LoginSession::spawn_poller`]. Errors only on
/// transport failure; a "not yet" answer is `Ok(Pending)`.
fn check_once(client: &reqwest::blocking::Client, state: &str) -> Result<PollOutcome> {
    let resp = with_login_context(
        client
            .get(platform_check_url())
            .query(&[("state", state)])
            .send(),
        "Failed to call /auth/check",
    )?;
    let status_success = resp.status().is_success();
    let valid = status_success
        && resp
            .json::<PlatformCheckResponse>()
            .map(|c| c.valid)
            .unwrap_or(false);
    Ok(interpret_check(status_success, valid))
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
    ///
    /// NOTE: this BLOCKS the calling thread for the duration of the HTTP
    /// request (`.join()` on the worker). Interactive wait loops must
    /// prefer [`spawn_poller`](Self::spawn_poller) so a wedged request
    /// (hung DNS/connect that reqwest's timeout can't interrupt — the
    /// Windows "console frozen on cancel" failure mode) can't block the
    /// thread that reads the ESC keystroke. Kept for one-shot callers.
    pub fn poll_once(&self) -> Result<PollOutcome> {
        let client = match &self.client {
            Some(c) => c.clone(),
            None => return Ok(PollOutcome::Pending),
        };
        let state = self.state.clone();
        std::thread::spawn(move || check_once(&client, &state))
            .join()
            .map_err(|_| anyhow::anyhow!("poll_once thread panicked"))?
    }

    /// Spawn a DETACHED background thread that polls `/auth/check` every
    /// `interval` and streams each outcome over the returned channel until
    /// it reports `Authorized`, hits an error, or the receiver is dropped.
    ///
    /// This is the cancellation-safe counterpart to [`poll_once`](Self::poll_once).
    /// The caller's wait loop stays on a tight, non-blocking cadence (drain
    /// the channel, check for ESC, sleep briefly) so a request that wedges
    /// at the socket layer — where reqwest's `connect_timeout`/`timeout`
    /// can't interrupt a blocking `getaddrinfo` — leaks at most this one
    /// background thread instead of freezing the UI thread. That freeze is
    /// the root cause of the Windows "pressing ESC to cancel /login hangs
    /// the console" bug: `poll_once().join()` blocked the only thread that
    /// could observe the ESC keypress.
    ///
    /// The first check fires immediately (no leading `interval` sleep), so
    /// an already-authorized session resolves on the first tick, matching
    /// the old `poll_once`-at-top-of-loop cadence.
    pub fn spawn_poller(&self, interval: Duration) -> mpsc::Receiver<Result<PollOutcome>> {
        let (tx, rx) = mpsc::channel();
        let client = self.client.clone();
        let state = self.state.clone();
        std::thread::spawn(move || {
            let client = match client {
                // No client (test/degraded session): report Pending once and
                // stop — mirrors `poll_once`'s `None` arm.
                None => {
                    let _ = tx.send(Ok(PollOutcome::Pending));
                    return;
                }
                Some(c) => c,
            };
            loop {
                let outcome = check_once(&client, &state);
                let stop = !matches!(outcome, Ok(PollOutcome::Pending));
                // A send error means the caller dropped the receiver
                // (cancelled or finished) — stop polling and let the thread
                // exit. If the last request wedged, this thread is already
                // detached, so no one is blocked on it.
                if tx.send(outcome).is_err() {
                    return;
                }
                if stop {
                    return;
                }
                std::thread::sleep(interval);
            }
        });
        rx
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

    // Poll on a detached background thread so a wedged `/auth/check` request
    // can't block the ESC-detection loop below (see `spawn_poller`). The
    // foreground stays on a tight cadence: drain the channel, then give ESC
    // a short window, repeat.
    let poll_rx = session.spawn_poller(Duration::from_secs(2));
    loop {
        match poll_rx.try_recv() {
            Ok(Ok(PollOutcome::Authorized)) => break,
            Ok(Ok(PollOutcome::Pending)) => {}
            Ok(Err(e)) => return Err(e),
            Err(mpsc::TryRecvError::Disconnected) => {
                anyhow::bail!("login poller stopped unexpectedly")
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        match wait_for_esc_or_timeout(&cbreak, Duration::from_millis(100)) {
            EscOutcome::Cancelled => anyhow::bail!("login cancelled by user"),
            EscOutcome::Timeout | EscOutcome::OtherInput => {}
        }
    }
    // Stop the poller before the token exchange: dropping the receiver makes
    // the next `tx.send` fail so the background thread exits.
    drop(poll_rx);

    session.finish(tel)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpret_check_only_authorizes_on_success_and_valid() {
        assert_eq!(interpret_check(true, true), PollOutcome::Authorized);
        assert_eq!(
            interpret_check(true, false),
            PollOutcome::Pending,
            "2xx but valid=false is 'not yet', not authorized"
        );
        assert_eq!(
            interpret_check(false, true),
            PollOutcome::Pending,
            "non-2xx never authorizes even if a stale body said valid"
        );
        assert_eq!(interpret_check(false, false), PollOutcome::Pending);
    }

    /// A session with no client (degraded/test) must not spin: the poller
    /// reports `Pending` exactly once and then the thread exits, closing the
    /// channel. This exercises the send-then-stop control flow without a
    /// live server.
    #[test]
    fn spawn_poller_without_client_reports_pending_once_then_exits() {
        let session = LoginSession {
            state: "s".to_string(),
            login_url: "https://example/login".to_string(),
            client: None,
        };
        let rx = session.spawn_poller(Duration::from_millis(10));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap().unwrap(),
            PollOutcome::Pending
        );
        // Thread has exited → sender dropped → channel disconnected.
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_secs(2)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            ),
            "poller thread must exit after the no-client Pending report"
        );
    }

    /// Dropping the receiver must let the poller thread stop rather than
    /// leaking or panicking. With no client the thread returns immediately;
    /// the drop-before-recv path must simply be a clean no-op.
    #[test]
    fn spawn_poller_receiver_drop_is_clean() {
        let session = LoginSession {
            state: "s".to_string(),
            login_url: "https://example/login".to_string(),
            client: None,
        };
        let rx = session.spawn_poller(Duration::from_millis(10));
        drop(rx); // caller cancelled before reading — must not panic.
    }

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
