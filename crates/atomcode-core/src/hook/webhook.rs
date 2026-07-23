//! Webhook Hook 实现
//!
//! 允许通过 HTTP 远程调用 Hook，支持：
//! - 所有 Hook 时机的 Webhook 触发
//! - 超时控制
//! - 自动重试
//! - 自定义 Header（如认证 Token）

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::async_batcher::{AsyncWebhookBatcher, WebhookEvent};
use crate::hook::{
    ErrorContext, Hook, HookCtx, HookResult, OnErrorHook, OnMessageReceivedHook,
    OnModelResponseHook, OnSessionEndHook, OnSessionStartHook, OnToolCallStartHook,
    OnTurnCompleteHook, OnTurnStartHook, PostToolExecutionHook, PostTurnHook, PreToolExecutionHook,
    SessionContext, SystemPromptHook, ToolCallStartContext, ToolResultContext, TurnCompleteContext,
    TurnStartContext, UserMessageContext,
};

/// Webhook 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Webhook 名称
    pub name: String,
    /// 触发时机
    pub trigger: String,
    /// Webhook URL
    pub url: String,
    /// HTTP 方法（默认 POST）
    #[serde(default = "default_method")]
    pub method: String,
    /// 自定义 Header（如认证信息）
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    /// 超时时间（秒）
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 重试次数
    #[serde(default = "default_retries")]
    pub retries: u32,
    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 描述
    #[serde(default)]
    pub description: String,
}

fn default_method() -> String {
    "POST".to_string()
}

fn default_timeout() -> u64 {
    10
}

fn default_retries() -> u32 {
    2
}

fn default_true() -> bool {
    true
}

/// Webhook Hook 实现
pub struct WebhookHook {
    config: WebhookConfig,
    client: Client,
    /// 异步批处理器（可选）
    async_batcher: Option<Arc<AsyncWebhookBatcher>>,
}

impl WebhookHook {
    pub fn new(config: WebhookConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(false)
            .build()
            .unwrap_or_else(|_| Client::new());

        // 如果配置了批量处理，创建异步批处理器
        let async_batcher = None; // 默认使用同步模式

        Self {
            config,
            client,
            async_batcher,
        }
    }

    /// 创建带异步批处理器的 WebhookHook
    pub fn new_with_async(config: WebhookConfig, batcher: Arc<AsyncWebhookBatcher>) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .danger_accept_invalid_certs(false)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            config,
            client,
            async_batcher: Some(batcher),
        }
    }

    /// 发送 Webhook 请求
    async fn send_webhook(&self, payload: &serde_json::Value) -> Result<WebhookResponse, String> {
        // 如果启用了异步批处理，使用异步模式
        if let Some(ref batcher) = self.async_batcher {
            let event = WebhookEvent {
                event: payload
                    .get("event")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                hook_name: self.config.name.clone(),
                trigger: self.config.trigger.clone(),
                context: payload.clone(),
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
            };

            return match batcher.add_event(event).await {
                HookResult::Ok => Ok(WebhookResponse {
                    result: "ok".to_string(),
                    message: Some("Queued for async sending".to_string()),
                    modified_content: None,
                }),
                HookResult::Warning(msg) => Err(format!("Async queue failed: {}", msg)),
                _ => Err("Async queue denied".to_string()),
            };
        }

        // 否则使用同步模式
        let url = &self.config.url;
        let method = &self.config.method;

        // 构建请求
        let mut request = self.client.request(
            method
                .parse()
                .map_err(|e| format!("Invalid HTTP method: {}", e))?,
            url,
        );

        // 添加自定义 Header
        for (key, value) in &self.config.headers {
            request = request.header(key, value);
        }

        // 添加 Content-Type
        request = request.header("Content-Type", "application/json");

        // 添加 AtomCode 标识
        request = request.header("X-AtomCode-Version", env!("CARGO_PKG_VERSION"));
        request = request.header("X-AtomCode-Hook", &self.config.name);

        // 发送请求（带重试）
        let mut last_error = None;
        for attempt in 0..=self.config.retries {
            let req = request
                .try_clone()
                .ok_or_else(|| "Failed to clone request".to_string())?;

            match req.json(payload).send().await {
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();

                    if status.is_success() {
                        // 解析响应
                        let webhook_response: WebhookResponse = serde_json::from_str(&body)
                            .unwrap_or_else(|_| WebhookResponse {
                                result: "ok".to_string(),
                                message: None,
                                modified_content: None,
                            });

                        return Ok(webhook_response);
                    } else {
                        last_error = Some(format!(
                            "HTTP {} at attempt {}: {}",
                            status,
                            attempt + 1,
                            body
                        ));
                    }
                }
                Err(e) => {
                    last_error = Some(format!("Request failed at attempt {}: {}", attempt + 1, e));
                    // 指数退避重试
                    tokio::time::sleep(Duration::from_millis(100 * 2u64.pow(attempt))).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| "Unknown error".to_string()))
    }
}

impl Hook for WebhookHook {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

/// Webhook 响应结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookResponse {
    /// 结果: ok, warning, deny, modify
    pub result: String,
    /// 消息（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 修改后的内容（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified_content: Option<String>,
}

// ============================================================================
// Webhook Hook 实现 - 所有 Hook 时机
// ============================================================================

#[async_trait]
impl OnMessageReceivedHook for WebhookHook {
    async fn on_message_received(&self, ctx: &UserMessageContext) -> HookResult {
        if !self.config.trigger.to_lowercase().contains("message") {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "on_message_received",
            "context": ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl OnTurnStartHook for WebhookHook {
    async fn on_turn_start(&self, ctx: &TurnStartContext) -> HookResult {
        if !self.config.trigger.to_lowercase().contains("turn_start") {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "on_turn_start",
            "context": ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl OnToolCallStartHook for WebhookHook {
    async fn on_tool_call_start(&self, ctx: &ToolCallStartContext) -> HookResult {
        if !self
            .config
            .trigger
            .to_lowercase()
            .contains("tool_call_start")
        {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "on_tool_call_start",
            "context": ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl PreToolExecutionHook for WebhookHook {
    async fn on_pre_execute(&self, ctx: &HookCtx) -> HookResult {
        let t = self.config.trigger.to_lowercase();
        if !t.contains("pre_tool") && !t.contains("before_tool") {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "pre_tool_execution",
            "context": ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl PostToolExecutionHook for WebhookHook {
    async fn on_post_execute(&self, ctx: &HookCtx, result_ctx: &ToolResultContext) -> HookResult {
        let t = self.config.trigger.to_lowercase();
        if !t.contains("post_tool") && !t.contains("after_tool") {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "post_tool_execution",
            "hook_context": ctx,
            "result_context": result_ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl OnTurnCompleteHook for WebhookHook {
    async fn on_turn_complete(&self, ctx: &TurnCompleteContext) -> HookResult {
        let t = self.config.trigger.to_lowercase();
        if !t.contains("turn_complete") && !t.contains("after_turn") {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "on_turn_complete",
            "context": ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl PostTurnHook for WebhookHook {
    async fn on_post_turn(&self, ctx: &HookCtx, turn_result: &str) -> HookResult {
        if !self.config.trigger.to_lowercase().contains("post_turn") {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "post_turn",
            "context": ctx,
            "turn_result": turn_result,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl OnSessionStartHook for WebhookHook {
    async fn on_session_start(&self, ctx: &SessionContext) -> HookResult {
        if !self.config.trigger.to_lowercase().contains("session_start") {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "on_session_start",
            "context": ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl OnSessionEndHook for WebhookHook {
    async fn on_session_end(&self, ctx: &SessionContext) -> HookResult {
        if !self.config.trigger.to_lowercase().contains("session_end") {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "on_session_end",
            "context": ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl OnErrorHook for WebhookHook {
    async fn on_error(&self, ctx: &ErrorContext) -> HookResult {
        if !self.config.trigger.to_lowercase().contains("error") {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "on_error",
            "context": ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl OnModelResponseHook for WebhookHook {
    async fn on_model_response(&self, response: &str, turn_ctx: &TurnStartContext) -> HookResult {
        if !self
            .config
            .trigger
            .to_lowercase()
            .contains("model_response")
        {
            return HookResult::Ok;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "on_model_response",
            "response": response,
            "turn_context": turn_ctx,
        });

        match self.send_webhook(&payload).await {
            Ok(response) => parse_webhook_response(&response),
            Err(e) => HookResult::Warning(format!("Webhook error: {}", e)),
        }
    }
}

#[async_trait]
impl SystemPromptHook for WebhookHook {
    async fn extend_system_prompt(&self) -> Option<String> {
        if !self.config.trigger.to_lowercase().contains("system_prompt") {
            return None;
        }

        let payload = serde_json::json!({
            "hook_name": self.config.name,
            "trigger": self.config.trigger,
            "event": "system_prompt",
        });

        match self.send_webhook(&payload).await {
            Ok(response) => {
                if response.result == "ok" || response.result == "modify" {
                    response.modified_content.or(response.message)
                } else {
                    None
                }
            }
            Err(e) => {
                tracing::warn!("[Webhook] {} error: {}", self.config.name, e);
                None
            }
        }
    }
}

/// 解析 Webhook 响应为 HookResult
fn parse_webhook_response(response: &WebhookResponse) -> HookResult {
    match response.result.as_str() {
        "ok" => HookResult::Ok,
        "warning" => HookResult::Warning(response.message.clone().unwrap_or_default()),
        "deny" => HookResult::Denied(response.message.clone().unwrap_or_default()),
        "modify" => HookResult::Modified(response.modified_content.clone().unwrap_or_default()),
        _ => HookResult::Warning(format!("Unknown webhook result: {}", response.result)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    // -----------------------------------------------------------------------
    // WebhookConfig defaults
    // -----------------------------------------------------------------------
    #[test]
    fn test_webhook_config_defaults() {
        let json = r#"{
            "name": "test-hook",
            "trigger": "message",
            "url": "https://example.com/hook"
        }"#;

        let config: WebhookConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.name, "test-hook");
        assert_eq!(config.trigger, "message");
        assert_eq!(config.url, "https://example.com/hook");
        assert_eq!(config.method, "POST");
        assert_eq!(config.timeout_secs, 10);
        assert_eq!(config.retries, 2);
        assert!(config.enabled);
        assert!(config.description.is_empty());
        assert!(config.headers.is_empty());
    }

    // -----------------------------------------------------------------------
    // WebhookConfig serialization roundtrip
    // -----------------------------------------------------------------------
    #[test]
    fn test_webhook_config_roundtrip() {
        let config = WebhookConfig {
            name: "roundtrip-hook".to_string(),
            trigger: "error".to_string(),
            url: "http://localhost:9999/hook".to_string(),
            method: "PUT".to_string(),
            headers: [("X-Auth".to_string(), "token123".to_string())].into(),
            timeout_secs: 30,
            retries: 5,
            enabled: false,
            description: "My hook".to_string(),
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: WebhookConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.name, config.name);
        assert_eq!(deserialized.trigger, config.trigger);
        assert_eq!(deserialized.url, config.url);
        assert_eq!(deserialized.method, config.method);
        assert_eq!(deserialized.headers.get("X-Auth").unwrap(), "token123");
        assert_eq!(deserialized.timeout_secs, 30);
        assert_eq!(deserialized.retries, 5);
        assert!(!deserialized.enabled);
        assert_eq!(deserialized.description, "My hook");
    }

    // -----------------------------------------------------------------------
    // WebhookResponse serialization roundtrip
    // -----------------------------------------------------------------------
    #[test]
    fn test_webhook_response_roundtrip_ok() {
        let resp = WebhookResponse {
            result: "ok".to_string(),
            message: None,
            modified_content: None,
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: WebhookResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.result, "ok");
        assert!(deserialized.message.is_none());
        assert!(deserialized.modified_content.is_none());
    }

    #[test]
    fn test_webhook_response_roundtrip_warning() {
        let resp = WebhookResponse {
            result: "warning".to_string(),
            message: Some("be careful".to_string()),
            modified_content: None,
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: WebhookResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.result, "warning");
        assert_eq!(deserialized.message.unwrap(), "be careful");
    }

    #[test]
    fn test_webhook_response_roundtrip_deny() {
        let resp = WebhookResponse {
            result: "deny".to_string(),
            message: Some("blocked".to_string()),
            modified_content: None,
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: WebhookResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.result, "deny");
        assert_eq!(deserialized.message.unwrap(), "blocked");
    }

    #[test]
    fn test_webhook_response_roundtrip_modify() {
        let resp = WebhookResponse {
            result: "modify".to_string(),
            message: Some("updated".to_string()),
            modified_content: Some("new content".to_string()),
        };

        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: WebhookResponse = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.result, "modify");
        assert_eq!(deserialized.message.unwrap(), "updated");
        assert_eq!(deserialized.modified_content.unwrap(), "new content");
    }

    // -----------------------------------------------------------------------
    // parse_webhook_response
    // -----------------------------------------------------------------------
    #[test]
    fn test_parse_webhook_response_ok() {
        let resp = WebhookResponse {
            result: "ok".to_string(),
            message: None,
            modified_content: None,
        };
        let result = parse_webhook_response(&resp);
        assert!(matches!(result, HookResult::Ok));
    }

    #[test]
    fn test_parse_webhook_response_warning() {
        let resp = WebhookResponse {
            result: "warning".to_string(),
            message: Some("caution".to_string()),
            modified_content: None,
        };
        let result = parse_webhook_response(&resp);
        assert!(matches!(result, HookResult::Warning(_)));
        if let HookResult::Warning(msg) = result {
            assert_eq!(msg, "caution");
        }
    }

    #[test]
    fn test_parse_webhook_response_deny() {
        let resp = WebhookResponse {
            result: "deny".to_string(),
            message: Some("forbidden".to_string()),
            modified_content: None,
        };
        let result = parse_webhook_response(&resp);
        assert!(matches!(result, HookResult::Denied(_)));
        if let HookResult::Denied(msg) = result {
            assert_eq!(msg, "forbidden");
        }
    }

    #[test]
    fn test_parse_webhook_response_modify() {
        let resp = WebhookResponse {
            result: "modify".to_string(),
            message: None,
            modified_content: Some("altered".to_string()),
        };
        let result = parse_webhook_response(&resp);
        assert!(matches!(result, HookResult::Modified(_)));
        if let HookResult::Modified(content) = result {
            assert_eq!(content, "altered");
        }
    }

    #[test]
    fn test_parse_webhook_response_unknown() {
        let resp = WebhookResponse {
            result: "unknown_value".to_string(),
            message: None,
            modified_content: None,
        };
        let result = parse_webhook_response(&resp);
        assert!(matches!(result, HookResult::Warning(_)));
        if let HookResult::Warning(msg) = result {
            assert!(msg.contains("unknown_value"));
        }
    }

    // -----------------------------------------------------------------------
    // WebhookHook::new
    // -----------------------------------------------------------------------
    #[test]
    fn test_webhook_hook_new() {
        let config = WebhookConfig {
            name: "my-hook".to_string(),
            trigger: "message".to_string(),
            url: "http://localhost:1234/hook".to_string(),
            method: "POST".to_string(),
            headers: std::collections::HashMap::new(),
            timeout_secs: 5,
            retries: 1,
            enabled: true,
            description: "Test hook".to_string(),
        };

        let hook = WebhookHook::new(config);

        assert_eq!(hook.name(), "my-hook");
        assert_eq!(hook.description(), "Test hook");
        assert!(hook.is_enabled());
    }

    // -----------------------------------------------------------------------
    // Hook trait
    // -----------------------------------------------------------------------
    #[test]
    fn test_webhook_hook_trait() {
        let config = WebhookConfig {
            name: "trait-test".to_string(),
            trigger: "error".to_string(),
            url: "http://localhost:5678/hook".to_string(),
            method: "POST".to_string(),
            headers: std::collections::HashMap::new(),
            timeout_secs: 3,
            retries: 0,
            enabled: false,
            description: "Disabled hook".to_string(),
        };

        let hook = WebhookHook::new(config);

        // Hook trait methods
        assert_eq!(hook.name(), "trait-test");
        assert_eq!(hook.description(), "Disabled hook");
        assert!(!hook.is_enabled());
    }
}
