//! Spike-only test doubles. NOT part of the kernel's real API.

use crate::hook::{LifecycleHooks, TurnCtx};
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

/// Returns scripted stream events (one Vec per call) and RECORDS the messages it
/// received on each call, so tests can assert exactly what the LLM saw.
pub struct MockProvider {
    turns: Mutex<VecDeque<Vec<StreamEvent>>>,
    /// Per chat_stream call: the received messages as (role_debug, text).
    pub received: Arc<Mutex<Vec<Vec<(String, String)>>>>,
    ctx_window: u32,
}

impl MockProvider {
    pub fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
            received: Arc::new(Mutex::new(Vec::new())),
            ctx_window: 0,
        }
    }
    pub fn with_ctx_window(mut self, w: u32) -> Self {
        self.ctx_window = w;
        self
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn model_name(&self) -> &str {
        "mock"
    }
    fn context_window(&self) -> u32 {
        self.ctx_window
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
    ) -> BoxStream<'static, StreamEvent> {
        let snapshot: Vec<(String, String)> =
            messages.iter().map(|m| (format!("{:?}", m.role), m.text.clone())).collect();
        self.received.lock().unwrap().push(snapshot);
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

/// Records every lifecycle callback it receives — used to prove the kernel wires
/// the FULL LifecycleHooks surface (no dead methods).
pub struct RecorderHook {
    pub log: Arc<Mutex<Vec<String>>>,
}

impl RecorderHook {
    pub fn new() -> Self {
        Self { log: Arc::new(Mutex::new(Vec::new())) }
    }
    fn record(&self, name: &str) {
        self.log.lock().unwrap().push(name.to_string());
    }
}

impl Default for RecorderHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LifecycleHooks for RecorderHook {
    async fn session_start(&self, _convo: &mut Conversation) { self.record("session_start"); }
    async fn user_prompt_submit(&self, _text: &mut String) { self.record("user_prompt_submit"); }
    async fn turn_start(&self, _convo: &mut Conversation) { self.record("turn_start"); }
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) { self.record("pre_request"); }
    async fn on_model_response(&self, _response: &mut Message) { self.record("on_model_response"); }
    async fn pre_tool(&self, _call: &ToolCall) -> Result<(), String> { self.record("pre_tool"); Ok(()) }
    async fn post_tool(&self, _result: &mut ToolResult) { self.record("post_tool"); }
    async fn turn_end(&self, _convo: &Conversation) -> Option<String> { self.record("turn_end"); None }
    async fn on_error(&self, _error: &str) { self.record("on_error"); }
    async fn session_end(&self, _convo: &Conversation) { self.record("session_end"); }
}

/// Projects current context-utilization back into the request as a TAIL reminder
/// so the LLM perceives its own budget pressure. Reads the latest meta from
/// history and appends ONE synthetic message at the END — never mutating history
/// → prefix-cache safe.
pub struct BudgetReminderHook;

#[async_trait]
impl LifecycleHooks for BudgetReminderHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        let util = messages.iter().rev().find_map(|m| m.meta.as_ref().map(|x| x.utilization));
        if let Some(u) = util {
            messages.push(Message::user(format!("[ctx {:.0}%]", u * 100.0)));
        }
    }
}

/// Projects the current round budget back into the request as a TAIL reminder so
/// the LLM can wrap up before the hard cap. Appends one synthetic message at the
/// END — never mutates history → prefix-cache safe.
pub struct RoundBudgetHook;

#[async_trait]
impl LifecycleHooks for RoundBudgetHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, ctx: &TurnCtx) {
        if let Some(max) = ctx.max_rounds {
            let note = if ctx.round >= max {
                format!("[round {}/{} - final round, wrap up now]", ctx.round, max)
            } else {
                format!("[round {}/{}]", ctx.round, max)
            };
            messages.push(Message::user(note));
        }
    }
}

/// Transforms the model response in on_model_response: redacts a secret from the
/// assistant text before it is stored. Proves `&mut Message` lets a hook rewrite
/// the response, and that the rewrite lands in storage.
pub struct RedactHook;

#[async_trait]
impl LifecycleHooks for RedactHook {
    async fn on_model_response(&self, response: &mut Message) {
        if response.text.contains("SECRET") {
            response.text = response.text.replace("SECRET", "[redacted]");
        }
    }
}

/// Probes concurrency: a shared (active, max) counter tracks the max number of
/// simultaneous executions. Declares its `parallel_safe` so tests can prove the
/// kernel parallelizes safe tools and serializes others.
pub struct ConcurrencyProbeTool {
    name: String,
    parallel: bool,
    tracker: Arc<Mutex<(u32, u32)>>,
}

impl ConcurrencyProbeTool {
    pub fn new(name: &str, parallel: bool, tracker: Arc<Mutex<(u32, u32)>>) -> Self {
        Self { name: name.to_string(), parallel, tracker }
    }
}

#[async_trait]
impl Tool for ConcurrencyProbeTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "concurrency probe"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn parallel_safe(&self) -> bool {
        self.parallel
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        {
            let mut t = self.tracker.lock().unwrap();
            t.0 += 1;
            if t.0 > t.1 {
                t.1 = t.0;
            }
        }
        // yield several times so a concurrent sibling can overlap before we exit.
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        {
            let mut t = self.tracker.lock().unwrap();
            t.0 -= 1;
        }
        ToolResult { call_id: String::new(), content: format!("ran {}", self.name), is_error: false }
    }
}
