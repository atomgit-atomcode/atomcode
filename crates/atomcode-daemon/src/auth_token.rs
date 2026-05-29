//! 本地 webui 一次性 token 存储与鉴权中间件。
//!
//! Phase 1：token 随 `/webui` 启动生成，仅存内存、随进程退出失效。
//! Phase 2（官方中转隧道）会把账号 token 接入同一条 `is_valid` 校验链——
//! 故鉴权统一收口在本模块，路由层只调中间件。

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use uuid::Uuid;
use axum::{
    extract::State,
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::Response,
};

/// 进程内有效 webui token 集合。线程安全，可放进 `AppState`。
#[derive(Clone, Default)]
pub struct WebuiTokenStore {
    inner: Arc<RwLock<HashSet<String>>>,
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

    /// 校验 token 是否有效。空串始终无效。
    pub fn is_valid(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        self.inner.read().unwrap().contains(token)
    }
}

/// 从 `Authorization` 头解析 Bearer token（大小写不敏感前缀）。
pub fn token_from_header(value: Option<&str>) -> Option<String> {
    let v = value?;
    let rest = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?;
    let rest = rest.trim();
    if rest.is_empty() { None } else { Some(rest.to_string()) }
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
) -> Result<Response, StatusCode> {
    let header = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok());
    match token_from_header(header) {
        Some(tok) if state.webui_tokens.is_valid(&tok) => Ok(next.run(req).await),
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_bearer_token() {
        assert_eq!(token_from_header(Some("Bearer abc123")), Some("abc123".to_string()));
        assert_eq!(token_from_header(Some("bearer abc123")), Some("abc123".to_string()));
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
}
