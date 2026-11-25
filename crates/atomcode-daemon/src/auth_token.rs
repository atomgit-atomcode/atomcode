//! 本地 webui 一次性 token 存储与鉴权中间件。
//!
//! Phase 1：token 随 `/webui` 启动生成，仅存内存、随进程退出失效。
//! Phase 2（官方中转隧道）会把账号 token 接入同一条 `is_valid` 校验链——
//! 故鉴权统一收口在本模块，路由层只调中间件。

use axum::{
    extract::State,
    http::{
        header::{AUTHORIZATION, COOKIE, RETRY_AFTER},
        StatusCode,
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// `X-Atom-User-Id` 请求头名，App 端通过中继透传桌面端进行双向校验。
const APP_USER_ID_HEADER: &str = "x-atom-user-id";

/// Name of the HttpOnly cookie that carries the webui token after the
/// `/?token=` handoff (see `serve_webui_index` in lib.rs). Keeping the
/// credential in an HttpOnly cookie — rather than the URL — prevents a
/// malicious browser extension from reading it off `location.search`
/// (CWE-598 / CWE-522).
/// Base cookie name. Intentionally `pub(crate)` and NOT used directly as a
/// cookie name anywhere — the real per-instance name is [`webui_cookie_name`]
/// (port-scoped). Reading/writing the bare base name would 401 against a
/// port-scoped instance, so it's kept crate-private to prevent that footgun.
pub(crate) const WEBUI_COOKIE: &str = "atomcode_webui";

/// Per-instance webui cookie name: `atomcode_webui_<port>`.
///
/// Cookies on `localhost` ignore the PORT (RFC 6265 — port is not part of a
/// cookie's identity), so the bare `atomcode_webui` name was SHARED across every
/// localhost port. A second `/webui` instance's `/?token=` handoff therefore
/// overwrote the first page's cookie in the shared jar, and the first page's
/// (now-wrong-token) requests all 401'd. Scoping the name by the instance's
/// actual bound port keeps each instance's cookie distinct, so they coexist.
pub fn webui_cookie_name(port: u16) -> String {
    format!("{WEBUI_COOKIE}_{port}")
}

/// 同一来源在一个窗口内允许的失败认证次数。
pub(crate) const AUTH_FAILURE_LIMIT: u32 = 10;
/// 失败计数窗口。窗口过期后计数从零开始。
pub(crate) const AUTH_FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// 达到失败上限后的封锁时长。
pub(crate) const AUTH_BLOCK_DURATION: Duration = Duration::from_secs(30);
/// 最多跟踪多少个来源。默认只监听 loopback；这个上限防止异常来源无限占用内存。
const AUTH_TRACKED_SOURCES_MAX: usize = 1_024;

/// 一次失败计数。`blocked_until` 有值时窗口已转为封锁。
#[derive(Clone, Copy)]
struct FailureWindow {
    window_start: Instant,
    failures: u32,
    blocked_until: Option<Instant>,
}

/// 进程内有效 webui token 集合。线程安全，可放进 `AppState`。
#[derive(Clone, Default)]
pub struct WebuiTokenStore {
    inner: Arc<RwLock<HashSet<String>>>,
    /// 按来源地址记录失败认证，用于限制暴力猜测。
    failures: Arc<Mutex<HashMap<String, FailureWindow>>>,
}

impl WebuiTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 生成并登记一个新 token，返回其字符串。
    pub fn mint(&self) -> String {
        let token = Uuid::new_v4().simple().to_string();
        self.inner.write().unwrap().insert(token.clone());
        token
    }

    /// 登记一个已知 token 为有效（用于从 env/文件注入固定 token）。
    pub fn insert(&self, token: String) {
        self.inner.write().unwrap().insert(token);
    }

    /// 校验 token 是否有效。空串始终无效。
    ///
    /// 扫描全部已登记 token 而不是 `HashSet::contains`，避免命中时提前返回
    /// 泄露比较前缀。token 由 [`Self::mint`] 固定为 32 个十六进制字符，
    /// 长度不是秘密。
    pub fn is_valid(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let guard = self.inner.read().unwrap();
        let mut found = 0usize;
        for known in guard.iter() {
            found |= constant_time_eq(token.as_bytes(), known.as_bytes()) as usize;
        }
        found != 0
    }

    /// 来源已被封锁时返回剩余等待时间。过期的窗口或封锁会在这里清理。
    pub(crate) fn auth_retry_after(&self, source: &str, now: Instant) -> Option<Duration> {
        let mut failures = self.failures.lock().unwrap();
        let entry = failures.get_mut(source)?;
        if let Some(until) = entry.blocked_until {
            if now < until {
                return Some(until - now);
            }
            failures.remove(source);
            return None;
        }
        if now.saturating_duration_since(entry.window_start) >= AUTH_FAILURE_WINDOW {
            failures.remove(source);
        }
        None
    }

    /// 记录一次失败认证。达到阈值时开始封锁并返回封锁时长。
    pub(crate) fn note_auth_failure(&self, source: &str, now: Instant) -> Option<Duration> {
        let mut failures = self.failures.lock().unwrap();
        if !failures.contains_key(source) && failures.len() >= AUTH_TRACKED_SOURCES_MAX {
            // Drop expired windows first, then the oldest remaining source.
            failures.retain(|_, window| match window.blocked_until {
                Some(until) => now < until,
                None => now.saturating_duration_since(window.window_start) < AUTH_FAILURE_WINDOW,
            });
            if failures.len() >= AUTH_TRACKED_SOURCES_MAX {
                // Keep active blocks: evicting one would let that source guess
                // again. Prefer the oldest source that is not blocked; if every
                // tracked source is blocked, fall back to the oldest so the map
                // stays bounded.
                let victim = failures
                    .iter()
                    .filter(|(_, window)| window.blocked_until.is_none_or(|until| now >= until))
                    .min_by_key(|(_, window)| window.window_start)
                    .or_else(|| {
                        failures
                            .iter()
                            .min_by_key(|(_, window)| window.window_start)
                    })
                    .map(|(source, _)| source.clone());
                if let Some(oldest) = victim {
                    failures.remove(&oldest);
                }
            }
        }
        let entry = failures.entry(source.to_owned()).or_insert(FailureWindow {
            window_start: now,
            failures: 0,
            blocked_until: None,
        });
        // A concurrent request may have passed the pre-check before another
        // one set the block. Keep the remaining wait instead of counting again.
        if let Some(until) = entry.blocked_until {
            if now < until {
                return Some(until - now);
            }
            entry.window_start = now;
            entry.failures = 0;
            entry.blocked_until = None;
        }
        if now.saturating_duration_since(entry.window_start) >= AUTH_FAILURE_WINDOW {
            entry.window_start = now;
            entry.failures = 0;
            entry.blocked_until = None;
        }
        entry.failures = entry.failures.saturating_add(1);
        if entry.failures < AUTH_FAILURE_LIMIT {
            return None;
        }
        entry.failures = 0;
        entry.blocked_until = Some(now + AUTH_BLOCK_DURATION);
        Some(AUTH_BLOCK_DURATION)
    }

    /// 成功认证清除该来源的失败记录。
    pub(crate) fn note_auth_success(&self, source: &str) {
        self.failures.lock().unwrap().remove(source);
    }
}

/// 常量时间字节比较。长度不同一定返回 false；比较次数只取决于较长的一方。
///
/// `black_box` 是编译器屏障：阻止 LLVM 把「xor 累积判零」折叠成会提前返回的
/// `memcmp`/`bcmp`，否则会重新打开 CWE-208 时序侧信道。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let longest = a.len().max(b.len());
    for index in 0..longest {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        diff |= (left ^ right) as usize;
    }
    std::hint::black_box(diff) == 0
}

/// 从 `Authorization` 头解析 Bearer token（大小写不敏感前缀）。
pub fn token_from_header(value: Option<&str>) -> Option<String> {
    let v = value?;
    let rest = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))?;
    let rest = rest.trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// 从 `Cookie` 头解析 `atomcode_webui=<token>`。Cookie 是 `/?token=` 交接后
/// 凭证的主要载体（HttpOnly，前端 JS / 浏览器插件读不到），EventSource/同源
/// fetch 会自动携带。空值视为无。
pub fn token_from_cookie(value: Option<&str>, cookie_name: &str) -> Option<String> {
    let v = value?;
    let prefix = format!("{cookie_name}=");
    for pair in v.split(';') {
        let pair = pair.trim();
        if let Some(tok) = pair.strip_prefix(&prefix) {
            let tok = tok.trim();
            if !tok.is_empty() {
                return Some(tok.to_string());
            }
        }
    }
    None
}

/// Per-source key for failed authentication. `serve_http` installs
/// `ConnectInfo`; routers built without it (tests, embedded hosts) share one
/// bucket.
pub(crate) fn request_source(req: &axum::extract::Request) -> String {
    req.extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|connect| connect.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Axum 中间件：校验 `Authorization: Bearer <token>` 是否为有效的 webui token。
///
/// 签名与 `activity_tracker_middleware`（同 crate）保持一致——
/// `axum::extract::Request` + `axum::middleware::Next`，适用于 axum 0.7。
/// 路由层通过 `axum::middleware::from_fn_with_state` 挂载（Task 8）。
pub async fn require_webui_token(
    State(state): State<crate::AppState>,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, Response> {
    if !state.enforce_token {
        // 只有 `--no-auth` 或 App 中继路径（webui_tokens=None）才走到这里；默认的
        // 独立 daemon / IDE 守护进程 enforce_token=true，会继续往下做 token 校验。
        return Ok(next.run(req).await);
    }
    // Per-source failure tracking. `serve_http` installs `ConnectInfo`; routers
    // built without it (tests, embedded hosts) share one bucket.
    let source = request_source(&req);
    let now = Instant::now();
    // Accept the token from either the `Authorization: Bearer` header
    // (programmatic clients, legacy URL-token front-ends) OR the HttpOnly
    // `atomcode_webui` cookie set by the `/?token=` handoff. Header wins
    // when both are present.
    let header = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok());
    let cookie = req.headers().get(COOKIE).and_then(|h| h.to_str().ok());
    // Read THIS instance's port-scoped cookie name so a sibling `/webui` on a
    // different localhost port (which shares the cookie jar) can't shadow us.
    // Use THIS instance's pre-resolved port-scoped cookie name (computed once on
    // AppState) so a sibling `/webui` on another localhost port can't shadow us.
    let token =
        token_from_header(header).or_else(|| token_from_cookie(cookie, &state.webui_cookie_name));
    match token {
        Some(tok) if state.webui_tokens.is_valid(&tok) => {
            state.webui_tokens.note_auth_success(&source);
            Ok(next.run(req).await)
        }
        // Only a presented token consumes the failure budget. A missing
        // credential is not a guess, so a broken client cannot lock itself out.
        // A valid token is checked before the block, so recovery is always
        // possible; the block only rejects further guesses.
        Some(_) => {
            if let Some(wait) = state.webui_tokens.auth_retry_after(&source, now) {
                return Err(too_many_requests(wait));
            }
            match state.webui_tokens.note_auth_failure(&source, now) {
                Some(wait) => Err(too_many_requests(wait)),
                None => Err(StatusCode::UNAUTHORIZED.into_response()),
            }
        }
        None => Err(StatusCode::UNAUTHORIZED.into_response()),
    }
}

/// 429 with the remaining block time. The body stays empty so the response
/// does not say whether a token exists.
pub(crate) fn too_many_requests(wait: Duration) -> Response {
    let mut response = StatusCode::TOO_MANY_REQUESTS.into_response();
    let seconds = wait.as_secs().max(1).to_string();
    if let Ok(value) = axum::http::HeaderValue::from_str(&seconds) {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

/// Axum 中间件：校验 `X-Atom-User-Id` 请求头是否与桌面端当前登录账号一致。
///
/// 仅 `/app` 模式下启用（`state.app_user_id` 非空），webui / 独立 daemon 不受影响。
/// App 端经中继透传此头（中继只过滤 `x-atom-token`，`x-atom-user-id` 原样通过），
/// 桌面 daemon 借此验证请求确实来自同一账号的手机客户端。
pub async fn require_app_user_id(
    State(state): State<crate::AppState>,
    req: axum::extract::Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = &state.app_user_id;
    if expected.is_empty() {
        // app_user_id 未设置（非 /app 模式 / 桌面未登录）：不校验，放行。
        return Ok(next.run(req).await);
    }

    // 从请求头读取 App 端传来的 user_id
    let actual = req
        .headers()
        .get(APP_USER_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if actual == *expected {
        Ok(next.run(req).await)
    } else {
        tracing::warn!(
            "app user_id mismatch: expected={:?}, actual={:?}",
            expected,
            actual,
        );
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[cfg(test)]
mod token_store_tests {
    use super::*;

    #[test]
    fn insert_makes_token_valid() {
        let store = WebuiTokenStore::new();
        assert!(!store.is_valid("known-token"));
        store.insert("known-token".to_string());
        assert!(store.is_valid("known-token"));
    }

    #[test]
    fn constant_time_equality_is_a_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn repeated_failures_block_only_that_source() {
        let store = WebuiTokenStore::new();
        let now = Instant::now();
        for _ in 0..AUTH_FAILURE_LIMIT - 1 {
            assert_eq!(store.note_auth_failure("10.0.0.1", now), None);
        }
        assert_eq!(
            store.note_auth_failure("10.0.0.1", now),
            Some(AUTH_BLOCK_DURATION)
        );
        assert!(store.auth_retry_after("10.0.0.1", now).is_some());
        assert_eq!(store.auth_retry_after("10.0.0.2", now), None);
    }

    #[test]
    fn expired_sources_are_pruned_before_eviction() {
        let store = WebuiTokenStore::new();
        let start = Instant::now();
        for index in 0..AUTH_TRACKED_SOURCES_MAX {
            store.note_auth_failure(&format!("expired-{index}"), start);
        }
        assert_eq!(
            store.failures.lock().unwrap().len(),
            AUTH_TRACKED_SOURCES_MAX
        );

        let later = start + AUTH_FAILURE_WINDOW + Duration::from_millis(1);
        store.note_auth_failure("fresh", later);
        let failures = store.failures.lock().unwrap();
        assert_eq!(
            failures.len(),
            1,
            "expired windows are dropped, not evicted"
        );
        assert!(failures.contains_key("fresh"));
    }

    #[test]
    fn eviction_keeps_an_active_block_and_bounds_the_map() {
        let store = WebuiTokenStore::new();
        let start = Instant::now();
        for _ in 0..AUTH_FAILURE_LIMIT - 1 {
            store.note_auth_failure("blocked", start);
        }
        assert!(store.note_auth_failure("blocked", start).is_some());
        for index in 1..AUTH_TRACKED_SOURCES_MAX {
            store.note_auth_failure(
                &format!("source-{index}"),
                start + Duration::from_micros(index as u64),
            );
        }
        assert_eq!(
            store.failures.lock().unwrap().len(),
            AUTH_TRACKED_SOURCES_MAX
        );

        store.note_auth_failure("new-source", start + Duration::from_secs(1));
        let failures = store.failures.lock().unwrap();
        assert_eq!(failures.len(), AUTH_TRACKED_SOURCES_MAX);
        assert!(
            failures.contains_key("blocked"),
            "an active block must not be evicted"
        );
        assert!(failures.contains_key("new-source"));
        assert!(
            !failures.contains_key("source-1"),
            "the oldest non-blocked source is evicted"
        );
    }

    #[test]
    fn a_failure_during_the_block_reports_the_remaining_wait() {
        let store = WebuiTokenStore::new();
        let now = Instant::now();
        for _ in 0..AUTH_FAILURE_LIMIT - 1 {
            store.note_auth_failure("10.0.0.1", now);
        }
        assert!(store.note_auth_failure("10.0.0.1", now).is_some());

        let one_second_in = now + Duration::from_secs(1);
        assert_eq!(
            store.note_auth_failure("10.0.0.1", one_second_in),
            Some(AUTH_BLOCK_DURATION - Duration::from_secs(1))
        );
    }

    #[test]
    fn block_expires_and_gives_a_fresh_budget() {
        let store = WebuiTokenStore::new();
        let now = Instant::now();
        for _ in 0..AUTH_FAILURE_LIMIT - 1 {
            store.note_auth_failure("10.0.0.1", now);
        }
        assert!(store.note_auth_failure("10.0.0.1", now).is_some());

        let after_block = now + AUTH_BLOCK_DURATION + Duration::from_millis(1);
        assert_eq!(store.auth_retry_after("10.0.0.1", after_block), None);
        assert_eq!(
            store.note_auth_failure("10.0.0.1", after_block),
            None,
            "the first failure after the block must not block again"
        );
    }

    #[test]
    fn failure_window_expires() {
        let store = WebuiTokenStore::new();
        let now = Instant::now();
        for _ in 0..AUTH_FAILURE_LIMIT - 1 {
            store.note_auth_failure("10.0.0.1", now);
        }

        let after_window = now + AUTH_FAILURE_WINDOW + Duration::from_millis(1);
        assert_eq!(store.note_auth_failure("10.0.0.1", after_window), None);
    }

    #[test]
    fn success_clears_the_failure_count() {
        let store = WebuiTokenStore::new();
        let now = Instant::now();
        for _ in 0..AUTH_FAILURE_LIMIT - 1 {
            store.note_auth_failure("10.0.0.1", now);
        }
        store.note_auth_success("10.0.0.1");
        assert_eq!(
            store.note_auth_failure("10.0.0.1", now),
            None,
            "a successful auth resets the failures before the limit"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bearer_token() {
        assert_eq!(
            token_from_header(Some("Bearer abc123")),
            Some("abc123".to_string())
        );
        assert_eq!(
            token_from_header(Some("bearer abc123")),
            Some("abc123".to_string())
        );
        assert_eq!(token_from_header(Some("abc123")), None);
        assert_eq!(token_from_header(None), None);
    }

    #[test]
    fn mints_and_validates_token() {
        let store = WebuiTokenStore::new();
        let tok = store.mint();
        assert!(store.is_valid(&tok), "freshly minted token must validate");
    }

    #[test]
    fn rejects_unknown_token() {
        let store = WebuiTokenStore::new();
        store.mint();
        assert!(!store.is_valid("not-a-real-token"));
        assert!(!store.is_valid(""));
    }

    #[test]
    fn mint_is_unique() {
        let store = WebuiTokenStore::new();
        assert_ne!(store.mint(), store.mint());
    }

    #[test]
    fn extracts_token_from_cookie() {
        assert_eq!(
            token_from_cookie(Some("atomcode_webui=abc123"), "atomcode_webui"),
            Some("abc123".to_string())
        );
        // Among other cookies, any order.
        assert_eq!(
            token_from_cookie(
                Some("foo=1; atomcode_webui=abc123; bar=2"),
                "atomcode_webui"
            ),
            Some("abc123".to_string())
        );
        assert_eq!(
            token_from_cookie(Some("bar=2; atomcode_webui=xyz"), "atomcode_webui"),
            Some("xyz".to_string())
        );
    }

    #[test]
    fn cookie_without_token_is_none() {
        assert_eq!(token_from_cookie(None, "atomcode_webui"), None);
        assert_eq!(token_from_cookie(Some(""), "atomcode_webui"), None);
        assert_eq!(
            token_from_cookie(Some("foo=1; bar=2"), "atomcode_webui"),
            None
        );
        assert_eq!(
            token_from_cookie(Some("atomcode_webui="), "atomcode_webui"),
            None
        );
        assert_eq!(
            token_from_cookie(Some("atomcode_webui= "), "atomcode_webui"),
            None
        );
        // Must not match a different cookie that merely ends with the name.
        assert_eq!(
            token_from_cookie(Some("x_atomcode_webui=nope"), "atomcode_webui"),
            None
        );
    }

    #[test]
    fn cookie_name_is_port_scoped() {
        assert_eq!(webui_cookie_name(54321), "atomcode_webui_54321");
        assert_ne!(
            webui_cookie_name(1111),
            webui_cookie_name(2222),
            "different ports must yield different cookie names"
        );
    }

    #[test]
    fn two_instances_on_different_ports_do_not_collide() {
        // The bug: the browser's shared localhost cookie jar holds BOTH instances'
        // cookies (cookies ignore the port). Each server must read ONLY its own
        // port-scoped name, so instance A still sees token A even after B set B.
        let jar = "atomcode_webui_1111=tokenA; atomcode_webui_2222=tokenB";
        assert_eq!(
            token_from_cookie(Some(jar), &webui_cookie_name(1111)),
            Some("tokenA".to_string()),
            "instance on port 1111 reads its own cookie, not the other's"
        );
        assert_eq!(
            token_from_cookie(Some(jar), &webui_cookie_name(2222)),
            Some("tokenB".to_string()),
            "instance on port 2222 reads its own cookie"
        );
        // A third instance whose cookie was never set sees nothing (→ 401, correct).
        assert_eq!(token_from_cookie(Some(jar), &webui_cookie_name(3333)), None);
    }
}
