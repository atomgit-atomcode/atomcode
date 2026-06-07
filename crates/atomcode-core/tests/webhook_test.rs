//! Webhook 测试

use std::collections::HashMap;


use atomcode_core::hook::Hook;
use atomcode_core::hook::webhook::{WebhookHook, WebhookConfig};

/// 测试用的 Mock HTTP 服务器（使用 wiremock）
mod webhook_test {
    use super::*;

    #[tokio::test]
    async fn test_webhook_config_defaults() {
        let config = WebhookConfig {
            name: "test".to_string(),
            trigger: "post_tool".to_string(),
            url: "http://example.com/hook".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
            timeout_secs: 10,
            retries: 2,
            enabled: true,
            description: "Test webhook".to_string(),
        };

        assert_eq!(config.method, "POST");
        assert_eq!(config.timeout_secs, 10);
        assert_eq!(config.retries, 2);
        assert!(config.enabled);
    }

    #[tokio::test]
    async fn test_webhook_disabled() {
        let config = WebhookConfig {
            name: "test".to_string(),
            trigger: "post_tool".to_string(),
            url: "http://example.com/hook".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
            timeout_secs: 10,
            retries: 2,
            enabled: false,
            description: "Test webhook".to_string(),
        };

        let hook = WebhookHook::new(config);
        assert!(!hook.is_enabled());
    }

    #[tokio::test]
    async fn test_webhook_name_and_description() {
        let config = WebhookConfig {
            name: "my-webhook".to_string(),
            trigger: "on_tool_call_start".to_string(),
            url: "http://example.com/hook".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
            timeout_secs: 10,
            retries: 2,
            enabled: true,
            description: "My custom webhook".to_string(),
        };

        let hook = WebhookHook::new(config);
        assert_eq!(hook.name(), "my-webhook");
        assert_eq!(hook.description(), "My custom webhook");
    }
}
