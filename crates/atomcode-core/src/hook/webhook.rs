//! Webhook Hook 实现
//!
//! 允许通过 HTTP 远程调用 Hook，支持：
//! - 所有 Hook 时机的 Webhook 触发
//! - 超时控制
//! - 自动重试
//! - 自定义 Header（如认证 Token）

use std::time::Duration;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::hook::{
    Hook, HookResult,
    OnMessageReceivedHook, OnTurnStartHook, OnTurnCompleteHook,
    OnToolCallStartHook, OnModelResponseHook,
    OnSessionStartHook, OnSessionEndHook, OnErrorHook,
    PreToolExecutionHook, PostToolExecutionHook, PostTurnHook, SystemPromptHook,
    UserMessageContext, TurnStartContext, TurnCompleteContext,
    ToolCallStartContext, ToolResultContext, HookCtx,
    ErrorContext, SessionContext,
};
use super::async_batcher::{AsyncWebhookBatcher, AsyncWebhookConfig, WebhookEvent};

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
        let async_batcher = None;  // 默认使用同步模式

        Self { config, client, async_batcher }
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
                event: payload.get("event").and_then(|v| v.as_str()).unwrap_or("unknown").to_string(),
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
            method.parse().map_err(|e| format!("Invalid HTTP method: {}", e))?,
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
            let req = request.try_clone().ok_or_else(|| "Failed to clone request".to_string())?;

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
                            status, attempt + 1, body
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
        if !self.config.trigger.contains("message") {
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
        if !self.config.trigger.contains("turn_start") {
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
        if !self.config.trigger.contains("tool_call_start") {
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
        if !self.config.trigger.contains("pre_tool") && !self.config.trigger.contains("before_tool") {
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
        if !self.config.trigger.contains("post_tool") && !self.config.trigger.contains("after_tool") {
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
        if !self.config.trigger.contains("turn_complete") && !self.config.trigger.contains("after_turn") {
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
        if !self.config.trigger.contains("post_turn") {
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
        if !self.config.trigger.contains("session_start") {
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
        if !self.config.trigger.contains("session_end") {
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
        if !self.config.trigger.contains("error") {
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
        if !self.config.trigger.contains("model_response") {
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
        if !self.config.trigger.contains("system_prompt") {
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
                eprintln!("[Webhook] {} error: {}", self.config.name, e);
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
