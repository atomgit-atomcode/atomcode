//! Spike-only test doubles. NOT part of the kernel's real API.

use crate::hook::LifecycleHooks;
use crate::message::{Conversation, Message};
use crate::middleware::ToolMiddleware;
use crate::provider::LlmProvider;
use crate::request::RequestCtx;
use crate::stream::StreamEvent;
use crate::tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolDef, ToolResult};
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Returns scripted stream events, one Vec per successive turn.
pub struct MockProvider {
    turns: Mutex<VecDeque<Vec<StreamEvent>>>,
}

impl MockProvider {
    pub fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
        Self { turns: Mutex::new(turns.into_iter().collect()) }
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn model_name(&self) -> &str {
        "mock"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> BoxStream<'static, StreamEvent> {
        let events = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![StreamEvent::Done]);
        Box::pin(futures::stream::iter(events))
    }
}

/// A safe tool.
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str { "echo" }
    fn description(&self) -> &str { "Echo the arguments back" }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult { call_id: String::new(), content: format!("echo: {args}"), is_error: false }
    }
}

/// A risky tool — declares RiskLevel::Risky. (Pretends to write; does nothing.)
pub struct RiskyWriteTool;

#[async_trait]
impl Tool for RiskyWriteTool {
    fn name(&self) -> &str { "risky_write" }
    fn description(&self) -> &str { "Pretend to write a file (risky)" }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}})
    }
    fn risk(&self) -> RiskLevel { RiskLevel::Risky }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult { call_id: String::new(), content: format!("wrote: {args}"), is_error: false }
    }
}

/// Injects one "keep going" reminder the first time the model tries to stop,
/// then lets it complete. Proves turn-level injection changes the loop.
pub struct ContinueOnceHook {
    used: AtomicBool,
}

impl ContinueOnceHook {
    pub fn new() -> Self {
        Self { used: AtomicBool::new(false) }
    }
}

impl Default for ContinueOnceHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LifecycleHooks for ContinueOnceHook {
    async fn turn_end(&self, _convo: &Conversation) -> Option<String> {
        if self.used.swap(true, Ordering::Relaxed) {
            None
        } else {
            Some("keep going".to_string())
        }
    }
}

/// Specialization-side middleware: gates Risky tools by round-tripping an
/// "approval" request to the driver via the kernel's GENERIC RequestCtx seam.
/// The kernel itself has no idea this is "approval".
pub struct ApprovalMiddleware;

#[async_trait]
impl ToolMiddleware for ApprovalMiddleware {
    async fn before(
        &self,
        call: &ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> Result<(), String> {
        if tool.risk() == RiskLevel::Safe {
            return Ok(());
        }
        let decision = rt
            .request(
                "approval",
                serde_json::json!({ "tool": tool.name(), "call_id": call.id, "args": call.arguments }),
            )
            .await;
        if decision.get("decision").and_then(|d| d.as_str()) == Some("allow") {
            Ok(())
        } else {
            Err("denied".into())
        }
    }
}
