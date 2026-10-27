//! HTTP retry / backoff helpers for OpenAI-compatible providers (L1).
//!
//! OPEN failures are retried per policy. A response-body read failure may also
//! transparently reopen the request, but only until replay-sensitive output (text,
//! reasoning, or tool data) reaches the consumer. Metadata alone is replay-safe.
//!
//! Direct consumers retain bounded adapter retries. Budgeted native calls
//! delegate transient OPEN and no-output stream retries to their request owner,
//! so every retry/continuation uses one allowance and cancellable wait policy.
//! HTTP 429 ownership is also selectable independently for older direct callers.

use std::time::Duration;

use atomcode_kernel::provider::RateLimitRetryOwner;
use atomcode_kernel::stream::StreamEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamReadRecovery {
    RetryExhausted { attempts: u32 },
    PartialResponse,
}

/// Wait for bytes within the remaining semantic progress budget. Heartbeats
/// neither switch phases nor restart the deadline; decoded events update it.
pub(crate) async fn next_chunk_phased<S, T, E>(
    byte_stream: &mut S,
    watchdog: &super::StreamWatchdog,
) -> Result<Option<Result<T, E>>, tokio::time::error::Elapsed>
where
    S: futures::Stream<Item = Result<T, E>> + Unpin,
{
    let result = watchdog.next(byte_stream).await;
    if result.is_err() {
        tracing::warn!(
            phase = watchdog.phase(),
            budget_ms = watchdog.budget().map(|d| d.as_millis() as u64),
            silent_ms = watchdog.silent_for().as_millis() as u64,
            "provider content deadline exceeded"
        );
    }
    result
}

/// Preserve a typed idle failure for the owner's progress-aware recovery.
/// The adapter must not replay a request after deltas reached its consumer.
pub(crate) fn stream_idle_error(
    watchdog: &super::StreamWatchdog,
) -> atomcode_kernel::stream::ProviderError {
    let limit = watchdog
        .budget()
        .map(|budget| format!("; limit {}ms", budget.as_millis()))
        .unwrap_or_default();
    atomcode_kernel::stream::ProviderError {
        code: Some(atomcode_kernel::stream::STREAM_IDLE_TIMEOUT_CODE.into()),
        message: format!(
            "stream idle timeout ({}): no content for {}ms{}",
            watchdog.phase(),
            watchdog.silent_for().as_millis(),
            limit
        ),
        retryable: false,
        ..Default::default()
    }
}

/// Whether replaying the whole provider request could duplicate user-visible output
/// or a tool side effect. Observational metadata is deliberately replay-safe.
pub(crate) fn is_replay_sensitive_event(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::TextDelta(_)
            | StreamEvent::Reasoning(_)
            | StreamEvent::ReasoningSignature { .. }
            | StreamEvent::ToolCall(_)
            | StreamEvent::ToolCallDelta { .. }
    )
}

/// Attempt-scoped metadata must not escape until the response commits by producing
/// replay-sensitive output or a terminal event. Otherwise a transparent reopen can
/// leave the kernel holding an id/model/usage value from the abandoned attempt.
pub(crate) fn is_attempt_metadata_event(event: &StreamEvent) -> bool {
    matches!(
        event,
        StreamEvent::ResponseId(_) | StreamEvent::ResponseModel(_) | StreamEvent::Usage(_)
    )
}

/// How long an idle keep-alive connection may sit in the pool before we drop
/// it. reqwest's default is 90s; gateway load balancers commonly close idle
/// connections sooner. 30s proved too generous against a real gateway — half-open
/// reuse there surfaced as hyper `IncompleteMessage` (see
/// [`chain_has_incomplete_message`]), a class the old classifier hard-failed.
/// 15s stays well under observed LB windows while keeping reuse for the
/// back-to-back requests of a tool loop (their gaps are far below 15s).
/// The broadened classifier + pool rebuild remain the correctness backstop.
/// Only affects *idle* connections — an active stream is never reaped.
pub(crate) const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// Retry configuration for the open call.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

/// Budgeted native calls have one retry owner. Standalone providers retain
/// their own policy; a caller's explicit zero still means one OPEN attempt.
pub(crate) fn policy_for_call(
    policy: &RetryPolicy,
    options: &atomcode_kernel::provider::ChatOptions,
) -> RetryPolicy {
    if options.request_recovery.is_some() {
        RetryPolicy::none()
    } else {
        policy.clone()
    }
}

pub(crate) fn recovery_exhausted(
    limit: atomcode_kernel::provider::RecoveryExhausted,
) -> atomcode_kernel::stream::ProviderError {
    atomcode_kernel::stream::ProviderError {
        retryable: true,
        code: Some(atomcode_kernel::provider::REQUEST_RECOVERY_EXHAUSTED_CODE.into()),
        message: limit.to_string(),
        ..Default::default()
    }
}

impl RetryPolicy {
    /// Default: 3 attempts, 500ms base, 8s cap.
    pub fn default_policy() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(8),
        }
    }

    /// Disable retries entirely (single attempt). Useful for tests / latency-sensitive callers.
    pub fn none() -> Self {
        Self {
            max_attempts: 1,
            base_delay: Duration::from_millis(0),
            max_delay: Duration::from_millis(0),
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// Transient server-side statuses worth retrying. Includes `529 Overloaded`
/// (Anthropic-style; some OpenAI-compatible gateways pass it through).
pub(crate) fn is_retryable_status(code: u16) -> bool {
    matches!(code, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
}

/// Whether a transient status should be retried inside the provider's OPEN loop.
/// HTTP 429 follows the per-call owner; all other transient statuses remain
/// provider-owned fast retries.
pub(crate) fn should_retry_open_status(code: u16, owner: RateLimitRetryOwner) -> bool {
    is_retryable_status(code) && (code != 429 || owner == RateLimitRetryOwner::Provider)
}

/// Transient transport errors worth retrying.
///
/// `is_timeout() || is_connect()` alone is too narrow: reqwest only reports
/// `is_connect()` for failures during connection *establishment*. The common
/// real-world case — a keep-alive connection that the gateway's load balancer
/// silently closed on idle-timeout, then we reuse it — surfaces as
/// "error sending request" with `is_connect() == false`, wrapping an
/// `io::Error(ConnectionReset)`. That used to be classified non-retryable and
/// hard-failed (the user-reported "open failed" that `/login` "fixed" by
/// rebuilding the client's pool). We now also walk the source chain for a
/// transient transport `io::Error` so the open loop reconnects transparently.
///
/// The same drop can also surface WITHOUT an io cause — as hyper's
/// `IncompleteMessage` ("connection closed before message completed"), which
/// [`chain_has_incomplete_message`] covers; both forms are unified under
/// [`is_stale_connection_error`].
pub(crate) fn is_retryable_reqwest_error(err: &reqwest::Error) -> bool {
    err.is_timeout()
        || err.is_connect()
        || is_stale_connection_error(err)
        || chain_has_tls_corruption(err)
}

/// True if any error in `err`'s `source()` chain is an `io::Error` whose kind
/// indicates a dropped/half-open connection (as opposed to a logical failure
/// like NotFound). Retrying these is safe **only on the OPEN path** (no
/// response bytes consumed yet) — mid-stream errors stay non-retryable.
///
/// `TimedOut` (Linux `ETIMEDOUT` / `os error 110`, Windows `10060`) is included:
/// the kernel gave up on a dead/stalled TCP connection while reading the streamed
/// body. It's a transport drop just like a reset — re-opening is safe, and it
/// earns the plain-language notice from [`stream_read_error_message`] instead of
/// the opaque raw chain.
pub(crate) fn chain_has_transient_io(err: &(dyn std::error::Error + 'static)) -> bool {
    use std::io::ErrorKind::*;
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                ConnectionReset
                    | ConnectionAborted
                    | BrokenPipe
                    | UnexpectedEof
                    | NotConnected
                    | TimedOut
            ) {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

/// True if any error in `err`'s `source()` chain is a TLS **record corruption**
/// failure — a data record whose AEAD tag / MAC did not verify. This covers
/// BOTH directions of a mangled record (verified against rustls 0.23 `error.rs`
/// Display, since we match the rendered chain, not a typed error — L1 avoids a
/// rustls dep):
///   - the PEER rejects a record WE sent → we receive its fatal alert, rendered
///     `received fatal alert: BadRecordMac` / `… DecryptError`;
///   - WE fail to authenticate a record the peer (or a middlebox) sent →
///     rustls `Error::DecryptError`, rendered `cannot decrypt peer's message`.
///
/// Both are the signature of a stale/long-lived pooled TLS-1.3 connection whose
/// state desynced, or a middlebox mangling TLS-1.3 records once the connection
/// has run for a while ("跑着跑着就 BadRecordMac").
///
/// rustls surfaces these as an `io::Error` of kind `InvalidData` — too broad to
/// fold into [`chain_has_transient_io`] (a malformed response *body* is also
/// `InvalidData` and must stay non-retryable). Handshake-negotiation alerts
/// (`HandshakeFailure`, …) are deliberately excluded: a fresh pool / TLS-1.2
/// *corruption* escalation is the wrong remedy for those — the `is_connect()`
/// fallback path owns handshake failures.
///
/// The recovery is scoped to the OPEN path (no response bytes consumed yet):
/// rebuild the client's pool and, on a managed endpoint, escalate to TLS 1.2.
pub(crate) fn chain_has_tls_corruption(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        let msg = e.to_string();
        if msg.contains("BadRecordMac")
            || msg.contains("DecryptError")
            || msg.contains("cannot decrypt peer's message")
        {
            return true;
        }
        cur = e.source();
    }
    false
}

/// True if any error in `err`'s `source()` chain is a hyper `IncompleteMessage` —
/// the server (or an LB in front of it) closed the connection before the response
/// message completed. This class does NOT wrap an `io::Error`, so
/// [`chain_has_transient_io`] cannot see it, and `is_timeout()`/`is_connect()` are
/// both false for it; it surfaced as a hard-failed
/// `open failed: … connection closed before message completed` with ZERO retries.
///
/// CAUTION: this does not prove the server never processed the request — see the
/// replay-risk note on [`is_stale_connection_error`].
pub(crate) fn chain_has_incomplete_message(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(h) = e.downcast_ref::<hyper::Error>() {
            if h.is_incomplete_message() {
                return true;
            }
        }
        cur = e.source();
    }
    false
}

/// A pooled connection the peer had already closed (half-open reuse). Rebuilding the
/// client's connection pool is the ONLY effective remedy — retrying without a rebuild
/// just grabs another dead socket from the same pool.
///
/// REPLAY RISK: neither form proves the server failed to receive or process the
/// request. Retrying may cause a duplicate inference (and duplicate billing). This is
/// accepted deliberately — the alternative is a hard failure after the user already
/// waited minutes — but callers must NOT describe it as a safe/idempotent replay.
pub(crate) fn is_stale_connection_error(err: &reqwest::Error) -> bool {
    chain_has_transient_io(err) || chain_has_incomplete_message(err)
}

/// Render an error plus its full `source()` chain as `top: cause: root`.
/// reqwest's Display for a transport failure is only the opaque shell
/// ("error sending request for url (…)"); the actionable cause
/// (`connection reset by peer (os error 54)`, `dns error`, …) lives in the
/// chain. Surfacing it turns the error line into a self-diagnosing probe.
///
/// **Never with a credential in it.** reqwest names the request's address in
/// full, and a base_url may carry one — `?key=…`, `user:password@`. This line
/// is shown, kept in the session log and written to `atomcode.log`, the file a
/// person is asked to send when something keeps failing. So every address a
/// reqwest error in the chain names is said without its userinfo, query and
/// fragment: the host and path, which are what the diagnosis needs, stay.
pub(crate) fn err_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        out.push_str(": ");
        out.push_str(&e.to_string());
        cur = e.source();
    }
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(url) = e
            .downcast_ref::<reqwest::Error>()
            .and_then(reqwest::Error::url)
        {
            let safe = url_without_credentials(url);
            if safe != url.as_str() {
                out = out.replace(url.as_str(), &safe);
            }
        }
        cur = e.source();
    }
    out
}

/// `url` as it may be said: no `user:password@`, no query, no fragment.
fn url_without_credentials(url: &reqwest::Url) -> String {
    let mut safe = url.clone();
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    safe.set_query(None);
    safe.set_fragment(None);
    safe.to_string()
}

pub(crate) fn stream_read_error_code(err: &(dyn std::error::Error + 'static)) -> Option<String> {
    (chain_has_transient_io(err) || chain_has_incomplete_message(err))
        .then(|| atomcode_kernel::stream::STREAM_TRANSPORT_INTERRUPTED_CODE.to_string())
}

/// Human-readable message for a mid-stream response-body read failure.
///
/// The raw reqwest chain for a dropped connection ("error decoding response
/// body: … 远程主机强迫关闭了一个现有的连接。 (os error 10054)") is opaque to
/// users. For the transient transport class (connection reset/abort/EOF — a
/// gateway dropping the connection under load) we LEAD with a Chinese
/// explanation and append the full cause chain for diagnosis. Logical failures
/// (e.g. a malformed body) keep the verbatim `stream read error: <chain>` form.
pub(crate) fn stream_read_error_message(
    err: &(dyn std::error::Error + 'static),
    recovery: StreamReadRecovery,
) -> String {
    // `IncompleteMessage` (connection closed before message completed) belongs to the
    // same transport class: it is a dropped connection, not a malformed body.
    if chain_has_transient_io(err) || chain_has_incomplete_message(err) {
        // Each recovery mode owns its full lead sentence — the PartialResponse
        // case is NOT a bare connection drop (we kept output), so it must not be
        // wrapped in the "网络连接中断" framing that fits RetryExhausted.
        let lead = match recovery {
            StreamReadRecovery::RetryExhausted { attempts } => {
                format!("网络连接中断:远端关闭或重置了连接,自动重连 {attempts} 次后仍失败,可重试。")
            }
            StreamReadRecovery::PartialResponse => {
                "响应中断:为避免重复输出或工具执行,未自动重放;已保留可安全保存的部分回复。无需重开会话——直接回复(例如「继续」)即可,模型会带着已保留的部分接着往下。"
                    .to_string()
            }
        };
        format!(
            "{lead}{}详情: {}",
            connection_reset_hint(err),
            err_chain(err)
        )
    } else {
        format!("stream read error: {}", err_chain(err))
    }
}

/// Windows WSAECONNRESET (`os error 10054`) — a *forced* connection reset — is
/// most often a corporate proxy/VPN/firewall killing the outbound stream, not a
/// server fault, so point the user at the actionable cause. Empty for other
/// transport drops (macOS `os error 54` / Linux `104`), where a proxy hint would
/// misdirect. Matches on the numeric code so it is locale-independent.
fn connection_reset_hint(err: &(dyn std::error::Error + 'static)) -> &'static str {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = cur {
        if e.to_string().contains("os error 10054") {
            return "此错误常见于公司网络或代理环境,请检查代理/VPN/防火墙设置后重试。";
        }
        cur = e.source();
    }
    ""
}

/// Strip any `user:pass@` userinfo from a proxy URL so credentials never land
/// in a user-facing error line or the log file. Best-effort string surgery (not
/// a full URL parse): drops the authority's userinfo, keeping scheme, host:port
/// and any path. Schemeless / userinfo-free input is returned trimmed, verbatim.
fn sanitize_proxy_url(raw: &str) -> String {
    let raw = raw.trim();
    let (scheme, rest) = match raw.find("://") {
        Some(i) => (&raw[..i + 3], &raw[i + 3..]),
        None => ("", raw),
    };
    // Split off only the query/fragment; the userinfo (and a host never carries
    // an unencoded '@') sits before it. Dropping up to the LAST '@' strips the
    // credential even when a raw '/' sits inside the password — bounding the
    // search at the first '/' would leave `user:pa/ss@` exposed. Over-stripping
    // a stray '@' in a path is harmless (host cosmetically wrong, no leak); a
    // proxy URL has no such path in practice.
    let tail_start = rest.find(['?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(tail_start);
    let host = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    format!("{scheme}{host}{tail}")
}

/// The sanitized proxy address reqwest most likely used for an HTTPS request, read
/// from the process proxy env published by `atomcode_config` policy. Best-effort:
/// the value is for a diagnostic hint, not a claim of exactly which proxy reqwest
/// resolved (scheme/`ALL_PROXY`/`NO_PROXY` precedence can differ). `None` when no
/// proxy env is set.
pub(crate) fn effective_proxy_env() -> Option<String> {
    for key in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim();
            if !v.is_empty() {
                return Some(sanitize_proxy_url(v));
            }
        }
    }
    None
}

/// When a connect failure is really a failure to reach the configured HTTP proxy
/// (reqwest's CONNECT tunnel could not open its underlying socket — e.g. a local
/// VPN/proxy client that has been shut down), lead with an actionable Chinese hint
/// naming the proxy and pointing at `/proxy`. Empty for every other connect
/// failure, where a proxy hint would misdirect.
///
/// Keyed on hyper-util's locale-independent `TunnelError::ConnectFailed` marker
/// ("failed to create underlying connection"), emitted ONLY on the proxy-tunnel
/// path — a direct connect failure never contains it, so this cannot false-fire.
///
/// NOTE: the marker is a transitive-dep Display string, not a stable contract.
/// A future hyper-util bump could reword it; the failure mode is fail-open (the
/// hint silently vanishes and the message reverts to the plain chain), never a
/// crash or misfire. If a `cargo update` bumps hyper-util, re-check this literal.
pub(crate) fn proxy_unreachable_hint(chain: &str, proxy: Option<&str>) -> String {
    if !chain.contains("failed to create underlying connection") {
        return String::new();
    }
    // Name the affected proxy when we can read it, else a generic reference; the
    // shared tail names the exact `/proxy` menu option (`no_proxy`) to pick —
    // the bare term "直连" is jargon users won't map to an action.
    let who = match proxy {
        Some(p) if !p.is_empty() => format!("代理 {p}"),
        _ => "配置的代理".to_string(),
    };
    format!(
        "无法连接到{who}(代理可能未运行或地址不可达)。若你不需要代理,请运行 /proxy 并选择 no_proxy(不使用代理)后重试。"
    )
}

/// The user-facing `open failed: …` message for a connection-open failure,
/// shared by every provider's `open_error`. Surfaces the full source chain so
/// the cause (connection reset / dns / proxy) is visible instead of reqwest's
/// opaque shell, and when the failure is really an unreachable HTTP proxy leads
/// with an actionable `/proxy` hint. Message-only — callers keep owning the
/// `retryable` decision via [`is_retryable_reqwest_error`].
pub(crate) fn open_failed_message(e: &reqwest::Error) -> String {
    let chain = err_chain(e);
    let hint = proxy_unreachable_hint(&chain, effective_proxy_env().as_deref());
    if hint.is_empty() {
        format!("open failed: {chain}")
    } else {
        format!("open failed: {hint}详情: {chain}")
    }
}

/// Parse `Retry-After` (RFC 7231 §7.1.3) into a wait duration. Handles BOTH forms:
/// delta-seconds (a bare integer) and an HTTP-date (e.g. some Anthropic 429s) — for the
/// date form the wait is `date - now`, clamped to zero for a past date. `None` only when
/// the header is absent or unparseable as either form.
pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let trimmed = value.trim();
    // delta-seconds form: a bare integer.
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    // HTTP-date form: wait until that instant (a past date → retry now = ZERO).
    let when = httpdate::parse_http_date(trimmed).ok()?;
    Some(
        when.duration_since(std::time::SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

/// Exponential backoff with real ±25% jitter, capped at `max_delay`. `attempt`
/// is 1-based.
///
/// Jitter exists to DECORRELATE retry timing across concurrent clients so a
/// shared upstream (the gateway) doesn't see a synchronized retry storm after
/// an outage — so the production jitter MUST vary per call. The pure math
/// lives in [`compute_backoff_jittered`] with the jitter position injected,
/// keeping unit tests reproducible WITHOUT making production deterministic (a
/// deterministic seed would defeat the anti-thundering-herd purpose).
pub(crate) fn compute_backoff(attempt: u32, policy: &RetryPolicy) -> Duration {
    compute_backoff_jittered(attempt, policy, random_jitter_fraction())
}

/// Pure backoff math. `jitter` is the position inside the ±25% window, in
/// `[0.0, 1.0)`: `0.0` → −25% (earliest), `0.5` → exactly `capped`, `~1.0` →
/// +25% (latest). Injected so production passes real randomness while tests
/// pass fixed fractions and assert exact bounds.
fn compute_backoff_jittered(attempt: u32, policy: &RetryPolicy, jitter: f64) -> Duration {
    let exp = policy
        .base_delay
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(16));
    let capped = exp.min(policy.max_delay);

    // ±25% window centered on `capped`: total span = 50% of `capped`.
    // Integer-ms math; for sub-2ms delays the window rounds to 0 (jitter is
    // meaningless at that scale) and we just return `capped` — never underflow.
    let capped_ms = capped.as_millis() as u64;
    let window_ms = capped_ms / 2;
    let jitter = jitter.clamp(0.0, 1.0 - f64::EPSILON);
    let offset_ms = (jitter * window_ms as f64) as u64;
    let floor_ms = capped_ms.saturating_sub(window_ms / 2);
    Duration::from_millis(floor_ms + offset_ms)
}

/// Real per-call jitter source for production. Wall-clock subsec nanos give
/// cross-process/cross-call decorrelation (the anti-thundering-herd property
/// jitter exists for) with no `rand` dependency. Returns a fraction in
/// `[0.0, 1.0)`. Tests bypass this entirely via [`compute_backoff_jittered`].
fn random_jitter_fraction() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos) / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_failure_is_typed_for_continuation_not_blind_replay() {
        let watchdog = super::super::StreamWatchdog::new(
            Some(Duration::from_secs(600)),
            Some(Duration::from_secs(300)),
        );
        let error = stream_idle_error(&watchdog);
        assert_eq!(
            error.code.as_deref(),
            Some(atomcode_kernel::stream::STREAM_IDLE_TIMEOUT_CODE)
        );
        assert!(!error.retryable);
        assert_eq!(error.http_status, None);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn idle_failure_reports_the_actual_content_phase_and_budget() {
        let mut watchdog = super::super::StreamWatchdog::new(
            Some(Duration::from_secs(600)),
            Some(Duration::from_secs(300)),
        );
        tokio::time::advance(Duration::from_secs(600)).await;
        watchdog.observe(&StreamEvent::Usage(Default::default()));
        let first = stream_idle_error(&watchdog);
        assert_eq!(
            first.message,
            "stream idle timeout (first_content): no content for 600000ms; limit 600000ms"
        );
        watchdog.observe(&StreamEvent::Reasoning("thinking".into()));
        tokio::time::advance(Duration::from_secs(300)).await;
        let idle = stream_idle_error(&watchdog);
        assert_eq!(
            idle.message,
            "stream idle timeout (stream_idle): no content for 300000ms; limit 300000ms"
        );
        assert_eq!(first.code, idle.code);
        assert!(!idle.retryable);
    }

    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn only_transport_causes_authorize_partial_text_continuation() {
        let reset = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset");
        assert_eq!(
            stream_read_error_code(&reset).as_deref(),
            Some(atomcode_kernel::stream::STREAM_TRANSPORT_INTERRUPTED_CODE)
        );
        let invalid = std::io::Error::new(std::io::ErrorKind::InvalidData, "connection reset");
        assert!(stream_read_error_code(&invalid).is_none());
    }

    #[test]
    fn retryable_status_table() {
        for c in [408, 425, 429, 500, 502, 503, 504, 529] {
            assert!(is_retryable_status(c), "{c} should be retryable");
        }
        for c in [400, 401, 403, 404, 422] {
            assert!(!is_retryable_status(c), "{c} should be fatal");
        }
    }

    #[test]
    fn open_retry_ownership_excludes_429_only() {
        assert!(
            is_retryable_status(429),
            "kernel must still receive a retryable 429"
        );
        assert!(should_retry_open_status(429, RateLimitRetryOwner::Provider));
        assert!(!should_retry_open_status(429, RateLimitRetryOwner::Kernel));
        for code in [408, 425, 500, 502, 503, 504, 529] {
            assert!(
                should_retry_open_status(code, RateLimitRetryOwner::Kernel),
                "{code} should keep provider fast retry"
            );
        }
    }

    #[test]
    fn parse_retry_after_seconds() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, HeaderValue::from_static("3"));
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(3)));
    }

    #[test]
    fn parse_retry_after_missing_is_none() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn parse_retry_after_http_date_future_is_delta_from_now() {
        // A future HTTP-date → wait ≈ (date − now). Build it from now so the assertion
        // is stable. HTTP-date has 1s resolution; allow slack for test execution time.
        let future = std::time::SystemTime::now() + Duration::from_secs(3600);
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(future)).unwrap(),
        );
        let got = parse_retry_after(&h).expect("future HTTP-date must parse");
        assert!(
            got.as_secs() >= 3590 && got.as_secs() <= 3600,
            "got {got:?}"
        );
    }

    #[test]
    fn parse_retry_after_http_date_past_is_zero() {
        // A past HTTP-date means "retry now" → ZERO (not None, not an underflow panic).
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&h), Some(Duration::ZERO));
    }

    #[test]
    fn parse_retry_after_garbage_is_none() {
        let mut h = HeaderMap::new();
        h.insert(
            RETRY_AFTER,
            HeaderValue::from_static("not-a-date-or-number"),
        );
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn backoff_respects_max_delay() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(1),
        };
        // capped at 1s, +25% jitter ⇒ never exceeds 1.5s.
        let d = compute_backoff(10, &policy);
        assert!(d <= Duration::from_millis(1500), "got {d:?}");
    }

    #[test]
    fn backoff_grows_then_caps() {
        let policy = RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(10),
        };
        // attempt 1 ≈ 100ms base (±25%); attempt 5 ≈ 1600ms base — strictly larger floor.
        let a1 = compute_backoff(1, &policy);
        let a5 = compute_backoff(5, &policy);
        assert!(a5 > a1, "backoff should grow: a1={a1:?} a5={a5:?}");
    }

    // Jitter-math tests against the pure `compute_backoff_jittered` (jitter
    // position injected) — reproducible WITHOUT making production jitter
    // deterministic, which would defeat the anti-thundering-herd purpose.

    #[test]
    fn backoff_jitter_spans_plus_minus_25_percent() {
        // attempt 1 → capped = base = 1000ms. floor = 750ms, center = 1000ms,
        // max < 1250ms (±25% centered on capped).
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(10),
        };
        assert_eq!(
            compute_backoff_jittered(1, &policy, 0.0),
            Duration::from_millis(750)
        );
        assert_eq!(
            compute_backoff_jittered(1, &policy, 0.5),
            Duration::from_millis(1000)
        );
        let hi = compute_backoff_jittered(1, &policy, 0.999);
        assert!(
            (Duration::from_millis(1240)..Duration::from_millis(1250)).contains(&hi),
            "near-1.0 jitter must approach +25% without reaching it: {hi:?}"
        );
    }

    #[test]
    fn backoff_jittered_is_pure() {
        let p = RetryPolicy::default_policy();
        assert_eq!(
            compute_backoff_jittered(2, &p, 0.3),
            compute_backoff_jittered(2, &p, 0.3),
            "same inputs + same jitter must be reproducible"
        );
    }

    #[test]
    fn backoff_grows_with_attempts_at_fixed_jitter() {
        // Hold jitter fixed so the EXPONENTIAL base — not jitter — drives
        // monotonicity, regardless of saturation.
        let p = RetryPolicy::default_policy();
        let d1 = compute_backoff_jittered(1, &p, 0.5);
        let d2 = compute_backoff_jittered(2, &p, 0.5);
        let d3 = compute_backoff_jittered(3, &p, 0.5);
        assert!(
            d1 < d2 && d2 < d3,
            "backoff must grow: {d1:?} {d2:?} {d3:?}"
        );
    }

    #[test]
    fn backoff_small_delay_is_safe() {
        // base 1ms → capped 1ms; ±25% window rounds to 0 but must never
        // underflow, panic, or yield a zero delay regardless of jitter.
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        };
        for jitter in [0.0, 0.5, 0.999] {
            let d = compute_backoff_jittered(1, &policy, jitter);
            assert!(
                (Duration::from_millis(1)..=Duration::from_millis(2)).contains(&d),
                "small delay out of range at jitter={jitter}: {d:?}"
            );
        }
    }

    #[test]
    fn backoff_random_source_stays_within_window() {
        // Production wrapper draws real randomness; the result must always
        // stay inside the ±25% window (bounds hold → not flaky).
        let policy = RetryPolicy {
            max_attempts: 5,
            base_delay: Duration::from_millis(1000),
            max_delay: Duration::from_secs(10),
        };
        for _ in 0..1000 {
            let d = compute_backoff(1, &policy);
            assert!(
                (Duration::from_millis(750)..Duration::from_millis(1250)).contains(&d),
                "random jitter escaped ±25% window: {d:?}"
            );
        }
    }

    // A two-level error chain `outer -> io::Error(kind)`, mirroring how a
    // reqwest "error sending request" wraps a hyper error wrapping the
    // underlying io error.
    #[derive(Debug)]
    struct Wrap(std::io::Error);
    impl std::fmt::Display for Wrap {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "error sending request")
        }
    }
    impl std::error::Error for Wrap {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn chain_has_transient_io_detects_connection_drops() {
        use std::io::{Error, ErrorKind};
        // The exact class we were missing: a connection reset surfaced
        // *through* a wrapper (is_connect() == false), so the old
        // `is_timeout() || is_connect()` check would have said "not
        // retryable" and hard-failed instead of reconnecting.
        for kind in [
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::UnexpectedEof,
            ErrorKind::NotConnected,
        ] {
            let e = Wrap(Error::new(kind, "boom"));
            assert!(
                chain_has_transient_io(&e),
                "{kind:?} buried in the chain must be treated as transient"
            );
        }
        // A bare io error (no wrapper) is detected too.
        assert!(chain_has_transient_io(&Error::new(
            ErrorKind::BrokenPipe,
            "bp"
        )));
    }

    #[test]
    fn chain_has_transient_io_detects_connection_timeout() {
        use std::io::{Error, ErrorKind};
        // The reported case: a streamed body read that dies with
        // `Connection timed out (os error 110)` (Linux ETIMEDOUT → ErrorKind::TimedOut)
        // must be treated as a transport drop — both for the plain-language message
        // and for open-path retry.
        let e = Wrap(Error::new(
            ErrorKind::TimedOut,
            "Connection timed out (os error 110)",
        ));
        assert!(
            chain_has_transient_io(&e),
            "ETIMEDOUT buried in the chain is a transport drop"
        );
        let msg = stream_read_error_message(&e, StreamReadRecovery::RetryExhausted { attempts: 2 });
        assert!(
            msg.contains("网络连接中断"),
            "leads with a plain-language notice: {msg}"
        );
        assert!(
            msg.contains("os error 110"),
            "still appends the raw cause: {msg}"
        );
    }

    #[test]
    fn chain_has_transient_io_ignores_non_transport_errors() {
        use std::io::{Error, ErrorKind};
        // NotFound / PermissionDenied are not transport hiccups — must NOT
        // be retried (re-sending won't help and could mask a real fault).
        assert!(!chain_has_transient_io(&Wrap(Error::new(
            ErrorKind::NotFound,
            "nf"
        ))));
        assert!(!chain_has_transient_io(&Wrap(Error::new(
            ErrorKind::PermissionDenied,
            "pd"
        ))));
        // An error chain with no io::Error at all → not classified transient.
        #[derive(Debug)]
        struct Plain;
        impl std::fmt::Display for Plain {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "plain")
            }
        }
        impl std::error::Error for Plain {}
        assert!(!chain_has_transient_io(&Plain));
    }

    #[test]
    fn chain_has_tls_corruption_detects_record_mac_and_decrypt_alerts() {
        use std::io::{Error, ErrorKind};
        // A gateway/middlebox mangling a TLS 1.3 data record surfaces as the
        // peer's fatal alert. rustls maps it to ErrorKind::InvalidData — too
        // broad to add to the transient list (a malformed *body* is also
        // InvalidData) — so we detect the corruption class by its alert
        // signature instead. Both the AEAD-tag failure (BadRecordMac) and its
        // sibling DecryptError count.
        assert!(chain_has_tls_corruption(&Wrap(Error::new(
            ErrorKind::InvalidData,
            "received fatal alert: BadRecordMac",
        ))));
        assert!(chain_has_tls_corruption(&Wrap(Error::new(
            ErrorKind::InvalidData,
            "received fatal alert: DecryptError",
        ))));
    }

    #[test]
    fn chain_has_tls_corruption_detects_local_inbound_decrypt_failure() {
        use std::io::{Error, ErrorKind};
        // The SYMMETRIC direction: a middlebox mangles a record the peer sent
        // US, so OUR rustls fails to authenticate it and returns
        // Error::DecryptError — which renders "cannot decrypt peer's message"
        // (rustls 0.23 error.rs:991), containing NEITHER "BadRecordMac" nor the
        // token "DecryptError". Must still be classified as corruption.
        assert!(chain_has_tls_corruption(&Wrap(Error::new(
            ErrorKind::InvalidData,
            "cannot decrypt peer's message",
        ))));
    }

    #[test]
    fn chain_has_tls_corruption_ignores_logical_and_handshake_failures() {
        use std::io::{Error, ErrorKind};
        // A malformed response body is ALSO InvalidData but is a logical
        // failure — a fresh connection won't help, so it must NOT be read as
        // TLS corruption (mirrors chain_has_transient_io's InvalidData carve-out).
        assert!(!chain_has_tls_corruption(&Wrap(Error::new(
            ErrorKind::InvalidData,
            "bad frame",
        ))));
        // A handshake-negotiation failure is fatal in a different way: a fresh
        // pool / TLS-1.2 *corruption* escalation is the wrong remedy — the
        // existing is_connect() fallback path owns handshake failures.
        assert!(!chain_has_tls_corruption(&Wrap(Error::new(
            ErrorKind::InvalidData,
            "received fatal alert: HandshakeFailure",
        ))));
    }

    #[test]
    fn tls_corruption_and_transient_io_are_disjoint_classes() {
        use std::io::{Error, ErrorKind};
        // The two recovery classes must not overlap: corruption earns a fresh
        // pool + managed TLS-1.2 escalation; a half-open socket drop just
        // reconnects. A BadRecordMac must not be misread as ConnectionReset,
        // and a reset must not be misread as corruption.
        let corruption = Wrap(Error::new(
            ErrorKind::InvalidData,
            "received fatal alert: BadRecordMac",
        ));
        assert!(chain_has_tls_corruption(&corruption));
        assert!(!chain_has_transient_io(&corruption));

        let reset = Wrap(Error::new(
            ErrorKind::ConnectionReset,
            "connection reset by peer",
        ));
        assert!(chain_has_transient_io(&reset));
        assert!(!chain_has_tls_corruption(&reset));
    }

    #[test]
    fn err_chain_appends_the_underlying_cause() {
        use std::io::{Error, ErrorKind};
        // The whole point of the probe: the top-level reqwest Display
        // ("error sending request") hides the cause; err_chain must surface
        // the buried "connection reset by peer" so the next failure is
        // diagnosable at a glance.
        let e = Wrap(Error::new(
            ErrorKind::ConnectionReset,
            "connection reset by peer (os error 54)",
        ));
        let s = err_chain(&e);
        assert!(
            s.contains("error sending request"),
            "keeps the top message: {s}"
        );
        assert!(
            s.contains("connection reset by peer (os error 54)"),
            "appends the cause: {s}"
        );
    }

    #[test]
    fn stream_read_error_message_explains_a_connection_reset_in_plain_language() {
        use std::io::{Error, ErrorKind};
        // The reported Windows case: a gateway forcibly closing the connection
        // mid-body surfaces as the opaque "os error 10054 / 远程主机强迫关闭了一个
        // 现有的连接". Lead with a human-readable Chinese explanation, but still
        // append the raw cause chain so the failure stays diagnosable.
        let e = Wrap(Error::new(
            ErrorKind::ConnectionReset,
            "远程主机强迫关闭了一个现有的连接。 (os error 10054)",
        ));
        let msg = stream_read_error_message(&e, StreamReadRecovery::RetryExhausted { attempts: 2 });
        assert!(
            msg.contains("网络连接中断"),
            "leads with a plain-language notice: {msg}"
        );
        assert!(
            msg.contains("os error 10054"),
            "still appends the raw cause for diagnosis: {msg}"
        );
        assert!(msg.contains("自动重连 2 次后仍失败"));
        // os error 10054 (Windows WSAECONNRESET) appends the corporate-network hint.
        assert!(
            msg.contains("公司网络或代理环境"),
            "10054 leads the user to the proxy/VPN cause: {msg}"
        );

        let partial = stream_read_error_message(&e, StreamReadRecovery::PartialResponse);
        assert!(partial.contains("为避免重复输出或工具执行"));
        assert!(partial.contains("已保留可安全保存的部分回复"));
        // Recovery is now ACTIONABLE (the report's gap: "可继续" told users nothing,
        // so they reopened the session). It must point at replying to continue, and
        // that no session reopen is needed.
        assert!(
            partial.contains("直接回复") && partial.contains("无需重开会话"),
            "PartialResponse must give an actionable recovery entry: {partial}"
        );
        assert!(!partial.contains("自动重连仍失败"));
        // The PartialResponse lead is self-contained — no "网络连接中断" double-中断.
        assert!(!partial.contains("网络连接中断"));
        // The 10054 hint is cause-scoped, so it rides both recovery modes.
        assert!(partial.contains("公司网络或代理环境"));
    }

    #[test]
    fn non_10054_transport_drop_omits_the_corporate_network_hint() {
        use std::io::{Error, ErrorKind};
        // macOS ECONNRESET (os error 54) is a generic transport drop — the
        // proxy/VPN hint would misdirect, so it must NOT appear.
        let e = Wrap(Error::new(
            ErrorKind::ConnectionReset,
            "Connection reset by peer (os error 54)",
        ));
        let msg = stream_read_error_message(&e, StreamReadRecovery::RetryExhausted { attempts: 1 });
        assert!(
            msg.contains("网络连接中断"),
            "still a plain-language notice: {msg}"
        );
        assert!(
            !msg.contains("公司网络或代理环境"),
            "generic reset must not claim a proxy cause: {msg}"
        );
    }

    #[test]
    fn stream_read_error_message_keeps_verbatim_form_for_logical_errors() {
        use std::io::{Error, ErrorKind};
        // A non-transport failure (e.g. malformed body) is NOT a network drop —
        // it must keep the verbatim `stream read error:` form, not be mislabeled
        // a connection interruption.
        let e = Wrap(Error::new(ErrorKind::InvalidData, "bad frame"));
        let msg = stream_read_error_message(&e, StreamReadRecovery::PartialResponse);
        assert!(
            msg.starts_with("stream read error:"),
            "verbatim form for logical errors: {msg}"
        );
        assert!(
            !msg.contains("网络连接中断"),
            "must not mislabel a logical error: {msg}"
        );
    }

    #[test]
    fn only_content_and_tool_events_are_replay_sensitive() {
        use atomcode_kernel::stream::TokenUsage;

        assert!(!is_replay_sensitive_event(&StreamEvent::ResponseId(
            "r".into()
        )));
        assert!(!is_replay_sensitive_event(&StreamEvent::ResponseModel(
            "m".into()
        )));
        assert!(!is_replay_sensitive_event(&StreamEvent::Usage(
            TokenUsage::default()
        )));
        assert!(!is_replay_sensitive_event(&StreamEvent::Malformed));
        assert!(is_attempt_metadata_event(&StreamEvent::ResponseId(
            "r".into()
        )));
        assert!(is_attempt_metadata_event(&StreamEvent::ResponseModel(
            "m".into()
        )));
        assert!(is_attempt_metadata_event(&StreamEvent::Usage(
            TokenUsage::default()
        )));
        assert!(!is_attempt_metadata_event(&StreamEvent::Malformed));
        assert!(is_replay_sensitive_event(&StreamEvent::TextDelta(
            "x".into()
        )));
        assert!(is_replay_sensitive_event(&StreamEvent::Reasoning(
            "x".into()
        )));
        assert!(is_replay_sensitive_event(&StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: "{".into(),
        }));
    }

    /// Spin up a server that reads the request then closes the connection immediately,
    /// reproducing the real shape of `connection closed before message completed`.
    /// A hand-built `reqwest::Error` cannot reproduce hyper's internal kind.
    async fn incomplete_message_error() -> reqwest::Error {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
            }
        });
        reqwest::Client::new()
            .post(format!("http://{addr}/v1/chat/completions"))
            .body("{}")
            .send()
            .await
            .expect_err("the server closes the connection, so this must be an Err")
    }

    #[tokio::test]
    async fn incomplete_message_is_retryable_and_stale() {
        let e = incomplete_message_error().await;
        // Precondition — the ROOT CAUSE of the bug: none of the three existing
        // predicates can see this error.
        assert!(!e.is_timeout());
        assert!(!e.is_connect());
        assert!(!chain_has_transient_io(&e));
        // The new predicates must.
        assert!(chain_has_incomplete_message(&e));
        assert!(is_retryable_reqwest_error(&e));
        assert!(is_stale_connection_error(&e));
    }

    #[tokio::test]
    async fn connection_refused_is_not_classified_as_incomplete_message() {
        let e = reqwest::Client::new()
            .post("http://127.0.0.1:1/nothing")
            .body("{}")
            .send()
            .await
            .expect_err("connection refused");
        // A refused connection also has is_request() == true — which is exactly why
        // is_request() cannot be used as the predicate.
        assert!(e.is_request());
        assert!(!chain_has_incomplete_message(&e));
        // It stays covered by is_connect(); behavior unchanged.
        assert!(e.is_connect());
        assert!(is_retryable_reqwest_error(&e));
    }

    #[test]
    fn sanitize_proxy_url_strips_embedded_credentials() {
        // A proxy URL can carry `user:pass@` userinfo; it must never reach a
        // user-facing error line or the log file.
        assert_eq!(
            sanitize_proxy_url("http://user:pass@10.0.0.1:8080"),
            "http://10.0.0.1:8080"
        );
    }

    #[test]
    fn sanitize_proxy_url_leaves_a_plain_proxy_untouched() {
        assert_eq!(
            sanitize_proxy_url("http://127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
        // Schemeless authority is preserved verbatim (trimmed).
        assert_eq!(sanitize_proxy_url("  127.0.0.1:7890 "), "127.0.0.1:7890");
    }

    #[test]
    fn sanitize_proxy_url_strips_userinfo_but_keeps_path() {
        // The `@` we strip is the authority's userinfo, not a later path char.
        assert_eq!(
            sanitize_proxy_url("http://u:p@proxy.corp:3128/pac"),
            "http://proxy.corp:3128/pac"
        );
    }

    #[test]
    fn sanitize_proxy_url_strips_userinfo_even_with_a_slash_in_the_password() {
        // A malformed-but-possible env value with an unencoded `/` inside the
        // password must NOT leak the credential (no `user:...@` may survive).
        assert_eq!(
            sanitize_proxy_url("http://user:pa/ss@host:8080"),
            "http://host:8080"
        );
    }

    #[test]
    fn proxy_hint_is_empty_for_a_non_tunnel_connect_failure() {
        // A direct connect failure never contains hyper-util's ConnectFailed
        // marker, so the proxy hint must not fire and misdirect.
        let chain = "error sending request for url (https://x): \
                     tcp connect error: connection refused (os error 61)";
        assert!(proxy_unreachable_hint(chain, Some("http://127.0.0.1:7890")).is_empty());
    }

    #[test]
    fn proxy_hint_names_the_proxy_and_points_at_slash_proxy() {
        // hyper-util emits "tunnel error: failed to create underlying connection"
        // ONLY on the proxy CONNECT path — the definitive "we couldn't reach the
        // proxy" signal.
        let chain = "error sending request for url (https://llm-api.atomgit.com): \
                     tunnel error: failed to create underlying connection: \
                     tcp connect error: connection refused (os error 10061)";
        let hint = proxy_unreachable_hint(chain, Some("http://127.0.0.1:7890"));
        assert!(
            hint.contains("http://127.0.0.1:7890"),
            "names the proxy: {hint}"
        );
        // Name the concrete `/proxy` menu option the user must pick, not the
        // jargon "直连" — users don't know what that means.
        assert!(hint.contains("/proxy"), "points at the command: {hint}");
        assert!(
            hint.contains("no_proxy"),
            "names the exact menu option: {hint}"
        );
    }

    #[test]
    fn proxy_hint_still_fires_without_a_known_proxy_address() {
        // The env var may be unreadable (reqwest resolved the proxy some other
        // way); the hint must still lead the user to `/proxy` without naming an
        // address it cannot vouch for.
        let chain = "tunnel error: failed to create underlying connection: \
                     tcp connect error: connection refused (os error 10061)";
        let hint = proxy_unreachable_hint(chain, None);
        assert!(!hint.is_empty());
        assert!(hint.contains("/proxy"), "points at the command: {hint}");
        assert!(
            hint.contains("no_proxy"),
            "names the exact menu option: {hint}"
        );
    }

    /// A base_url may carry a credential — a `?key=` or a `user:password@` —
    /// and reqwest names the address in full. The message is shown, kept in
    /// the session log and written to `atomcode.log`: it names the host and
    /// path, never the credential. No proxy, so the refusal is the machine's
    /// own and not a proxy's answer.
    #[tokio::test]
    async fn an_open_failure_names_the_address_without_its_credential() {
        let e = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post("http://me:hunter2@127.0.0.1:1/v1/chat/completions?key=topsecret#frag")
            .body("{}")
            .send()
            .await
            .expect_err("connection refused");
        assert!(
            e.to_string().contains("topsecret"),
            "reqwest names it in full, which is the point: {e}"
        );
        for msg in [
            open_failed_message(&e),
            stream_read_error_message(&e, StreamReadRecovery::RetryExhausted { attempts: 1 }),
        ] {
            assert!(!msg.contains("topsecret"), "{msg}");
            assert!(!msg.contains("hunter2"), "{msg}");
            assert!(
                msg.contains("127.0.0.1:1/v1/chat/completions"),
                "the host and path stay: {msg}"
            );
        }
    }

    #[tokio::test]
    async fn open_failed_message_keeps_the_plain_form_for_a_direct_connect_failure() {
        // A direct (non-proxy) connection refusal never carries the tunnel
        // marker, so the shared builder must NOT inject the proxy hint — it
        // stays byte-for-byte `open failed: <chain>`.
        let e = reqwest::Client::new()
            .post("http://127.0.0.1:1/nothing")
            .body("{}")
            .send()
            .await
            .expect_err("connection refused");
        let msg = open_failed_message(&e);
        assert!(msg.starts_with("open failed: "), "{msg}");
        assert!(
            !msg.contains("无法连接到"),
            "no proxy hint on a direct failure: {msg}"
        );
    }
}

/// Bound all OPEN attempts and backoffs together; an exhausted budget must not
/// be retried again by a runtime row. Streaming bodies are outside this scope.
pub(crate) async fn bounded_open<T>(
    per_attempt: Duration,
    open: impl std::future::Future<Output = Result<T, atomcode_kernel::stream::ProviderError>>,
) -> Result<T, atomcode_kernel::stream::ProviderError> {
    let budget = per_attempt.saturating_mul(2);
    match tokio::time::timeout(budget, open).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                phase = "response_headers",
                budget_ms = budget.as_millis() as u64,
                "provider open retry budget exhausted"
            );
            Err(atomcode_kernel::stream::ProviderError {
                retryable: false,
                message: format!(
                    "等待响应头超时：连接恢复与重试总等待已达到 {}s，请稍后重试",
                    budget.as_secs()
                ),
                code: Some(atomcode_kernel::stream::OPEN_TIMEOUT_CODE.into()),
                ..Default::default()
            })
        }
    }
}

#[cfg(test)]
mod open_budget_tests {
    use super::*;
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn open_budget_preserves_original_errors_and_success() {
        let error = bounded_open::<()>(Duration::from_secs(90), async {
            Err(atomcode_kernel::stream::ProviderError {
                retryable: false,
                http_status: Some(401),
                message: "unauthorized".into(),
                ..Default::default()
            })
        })
        .await
        .unwrap_err();
        assert_eq!(error.http_status, Some(401));
        assert_eq!(error.message, "unauthorized");
        assert_eq!(
            bounded_open(Duration::from_secs(90), async { Ok(42) })
                .await
                .unwrap(),
            42
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn open_budget_bounds_attempts_and_backoff_and_is_terminal() {
        let started = tokio::time::Instant::now();
        let error = bounded_open::<()>(Duration::from_secs(90), async {
            tokio::time::sleep(Duration::from_secs(90)).await;
            tokio::time::sleep(Duration::from_secs(3)).await;
            tokio::time::sleep(Duration::from_secs(90)).await;
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(started.elapsed(), Duration::from_secs(180));
        assert!(!error.retryable);
        assert_eq!(
            error.code.as_deref(),
            Some(atomcode_kernel::stream::OPEN_TIMEOUT_CODE)
        );
    }
}

#[cfg(test)]
mod content_deadline_tests {
    use super::*;
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn heartbeat_bytes_do_not_shorten_or_extend_prefill() {
        let watchdog = super::super::StreamWatchdog::new(
            Some(Duration::from_secs(600)),
            Some(Duration::from_secs(120)),
        );
        let bytes = futures::stream::unfold((), |_| async {
            tokio::time::sleep(Duration::from_secs(70)).await;
            Some((Ok::<_, ()>(b": ping\n\n"), ()))
        });
        futures::pin_mut!(bytes);
        let started = tokio::time::Instant::now();
        for _ in 0..8 {
            assert!(next_chunk_phased(&mut bytes, &watchdog)
                .await
                .unwrap()
                .is_some());
        }
        assert!(next_chunk_phased(&mut bytes, &watchdog).await.is_err());
        assert_eq!(started.elapsed(), Duration::from_secs(600));
    }
}
