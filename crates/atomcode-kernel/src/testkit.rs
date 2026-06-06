//! Spike-only test doubles. NOT part of the kernel's real API.

use crate::hook::{LifecycleHooks, TurnCtx};
use crate::message::{Conversation, Message};
use crate::middleware::ToolMiddleware;
use crate::provider::LlmProvider;
use crate::request::RequestCtx;
use crate::stream::{ProviderError, StreamEvent};
use crate::tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolDef, ToolResult};
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::HashSet;
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
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let snapshot: Vec<(String, String)> =
            messages.iter().map(|m| (format!("{:?}", m.role), m.text.clone())).collect();
        self.received.lock().unwrap().push(snapshot);
        let events = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![StreamEvent::Done { truncated: false }]);
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// One-shot scripted provider for the FALLIBLE-stream claims: it either fails to
/// OPEN (returns `Err`) or yields a fixed event script ONCE (a single turn). A
/// richer adversarial mock is a separate later task — this stays minimal.
pub struct ScriptedProvider {
    open_error: Option<ProviderError>,
    events: Mutex<Option<Vec<StreamEvent>>>,
}

impl ScriptedProvider {
    /// `chat_stream` returns `Err(e)` — a failed open.
    pub fn open_error(e: ProviderError) -> Self {
        Self { open_error: Some(e), events: Mutex::new(None) }
    }
    /// `chat_stream` opens OK and yields `events` once.
    pub fn events(events: Vec<StreamEvent>) -> Self {
        Self { open_error: None, events: Mutex::new(Some(events)) }
    }
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn model_name(&self) -> &str {
        "scripted"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        if let Some(e) = &self.open_error {
            return Err(e.clone());
        }
        let events = self.events.lock().unwrap().take().unwrap_or_default();
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// Scripted, RICH-recording provider for prefix-cache regression tests. Unlike
/// `MockProvider` (which snapshots only `(role, text)` and discards tools /
/// tool_calls), this records the FULL `(Vec<Message>, Vec<ToolDef>)` it received
/// on every `chat_stream` call — so a test can byte-compare the exact wire prefix
/// (history + tool block) the provider saw across rounds and turns.
///
/// Each call pops the next scripted `Vec<StreamEvent>`; an empty queue yields a
/// bare `Done { truncated: false }`. This drives multi-round turns (call 1 → a
/// ToolCall then Done; call 2 → TextDelta then Done → no calls → turn ends) and
/// multi-turn sessions.
pub struct RecordingProvider {
    turns: Mutex<VecDeque<Vec<StreamEvent>>>,
    calls: Arc<Mutex<Vec<(Vec<Message>, Vec<ToolDef>)>>>,
    ctx_window: u32,
}

impl RecordingProvider {
    pub fn new(turns: Vec<Vec<StreamEvent>>) -> Self {
        Self {
            turns: Mutex::new(turns.into_iter().collect()),
            calls: Arc::new(Mutex::new(Vec::new())),
            ctx_window: 0,
        }
    }
    pub fn with_ctx_window(mut self, w: u32) -> Self {
        self.ctx_window = w;
        self
    }
    /// Shared handle to the recorded calls; clone before moving the provider into
    /// the builder so the test can inspect what the LLM saw afterwards.
    pub fn calls(&self) -> Arc<Mutex<Vec<(Vec<Message>, Vec<ToolDef>)>>> {
        self.calls.clone()
    }
    /// A point-in-time snapshot of every recorded `(messages, tools)` call.
    pub fn recorded(&self) -> Vec<(Vec<Message>, Vec<ToolDef>)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    fn model_name(&self) -> &str {
        "recording"
    }
    fn context_window(&self) -> u32 {
        self.ctx_window
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.calls.lock().unwrap().push((messages.to_vec(), tools.to_vec()));
        let events = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![StreamEvent::Done { truncated: false }]);
        Ok(Box::pin(futures::stream::iter(events)))
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
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky
    }
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

/// Specialization-side approval gate for risky tool calls. Reads the tool's
/// ARG-AWARE risk; for a risky call it checks a session-grant cache ("remember")
/// and, if not yet granted, round-trips an "approval" request to the driver over
/// the kernel's generic RequestCtx seam. The kernel knows none of this.
pub struct ApprovalMiddleware {
    granted: Arc<Mutex<HashSet<String>>>,
}

impl ApprovalMiddleware {
    pub fn new() -> Self {
        Self { granted: Arc::new(Mutex::new(HashSet::new())) }
    }
    fn grant_key(call: &ToolCall) -> String {
        format!("{}::{}", call.name, call.arguments)
    }
}

impl Default for ApprovalMiddleware {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolMiddleware for ApprovalMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> Result<(), String> {
        // Safe call (arg-aware) → no approval.
        if tool.risk(&call.arguments) == RiskLevel::Safe {
            return Ok(());
        }
        // Session grant cache: an identical risky call already approved-always.
        let key = Self::grant_key(call);
        if self.granted.lock().unwrap().contains(&key) {
            return Ok(());
        }
        // Round-trip to the driver for a decision.
        let decision = rt
            .request(
                "approval",
                serde_json::json!({ "tool": tool.name(), "args": call.arguments }),
            )
            .await;
        if decision.get("decision").and_then(|d| d.as_str()) != Some("allow") {
            return Err("denied".to_string());
        }
        // "remember" → cache the grant so the same command is not asked again.
        if decision.get("remember").and_then(|r| r.as_bool()) == Some(true) {
            self.granted.lock().unwrap().insert(key);
        }
        Ok(())
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
    async fn user_prompt_submit(&self, _text: &mut String) -> Result<(), String> { self.record("user_prompt_submit"); Ok(()) }
    async fn turn_start(&self, _convo: &mut Conversation) { self.record("turn_start"); }
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) { self.record("pre_request"); }
    async fn on_model_response(&self, _response: &mut Message) { self.record("on_model_response"); }
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

/// A bash-like tool whose danger depends on the command (arg-aware risk).
pub struct DangerousBashTool;

#[async_trait]
impl Tool for DangerousBashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a shell command"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string"}}})
    }
    fn risk(&self, args: &str) -> RiskLevel {
        const DANGEROUS: &[&str] = &["rm -rf", "sudo", "mkfs", "dd if=", ":(){"];
        if DANGEROUS.iter().any(|p| args.contains(p)) {
            RiskLevel::Risky
        } else {
            RiskLevel::Safe
        }
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult { call_id: String::new(), content: format!("ran: {args}"), is_error: false }
    }
}

/// Drops all tool calls from the response — proves the kernel HONORS
/// on_model_response edits to tool_calls (a dropped call will not execute).
pub struct DropToolsHook;

#[async_trait]
impl LifecycleHooks for DropToolsHook {
    async fn on_model_response(&self, response: &mut Message) {
        response.tool_calls.clear();
    }
}

/// Rewrites a tool call's args in `before` — proves ToolMiddleware can mutate the call.
pub struct ArgRewriteMiddleware;

#[async_trait]
impl ToolMiddleware for ArgRewriteMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> Result<(), String> {
        call.arguments = "{\"rewritten\":true}".to_string();
        Ok(())
    }
}

/// Blocks every tool in `before` — proves a blocked tool emits no ghost ToolStarted.
pub struct BlockToolMiddleware;

#[async_trait]
impl ToolMiddleware for BlockToolMiddleware {
    async fn before(
        &self,
        _call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> Result<(), String> {
        Err("blocked by policy".to_string())
    }
}

/// Blocks every prompt in user_prompt_submit — proves a prompt can be rejected.
pub struct RejectPromptHook;

#[async_trait]
impl LifecycleHooks for RejectPromptHook {
    async fn user_prompt_submit(&self, _text: &mut String) -> Result<(), String> {
        Err("policy violation".to_string())
    }
}

/// Transforms the tool result in `after` — proves the after-chain (absorbs post_tool).
pub struct TruncateMiddleware;

#[async_trait]
impl ToolMiddleware for TruncateMiddleware {
    async fn after(&self, result: &mut ToolResult) {
        result.content = format!("[truncated] {}", result.content);
    }
}
