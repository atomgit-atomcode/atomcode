//! 本地 webui 一次性 token 存储与鉴权中间件。
//!
//! Phase 1：token 随 `/webui` 启动生成，仅存内存、随进程退出失效。
//! Phase 2（官方中转隧道）会把账号 token 接入同一条 `is_valid` 校验链——
//! 故鉴权统一收口在本模块，路由层只调中间件。

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

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

#[cfg(test)]
mod tests {
    use super::*;

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
