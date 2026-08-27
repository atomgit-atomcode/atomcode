//! Spike-only test doubles. NOT part of the kernel's real API.

use crate::agent::{Agent, AutoRespond};
use crate::event::AgentCommand;
use crate::event::StopReason;
use crate::hook::{LifecycleHooks, RateLimitDecision, RateLimitHint, TurnCtx};
use crate::message::{CompactionPlan, CompactionStrategy, CompactionView, Conversation, Message};
use crate::middleware::{AfterOutcome, BeforeOutcome, ToolMiddleware};
use crate::provider::{ChatOptions, LlmProvider};
use crate::request::RequestCtx;
use crate::stream::{ProviderError, StreamEvent};
use crate::tool::MountedTools;
use crate::tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolDef, ToolResult};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

/// Recorded `(role_debug, text)` for each received message, per `chat_stream` call.
type ReceivedLog = Arc<Mutex<Vec<Vec<(String, String)>>>>;
/// Recorded `(messages, tools, options)` per `chat_stream` call.
type CallLog = Arc<Mutex<Vec<(Vec<Message>, Vec<ToolDef>, ChatOptions)>>>;

/// Returns scripted stream events (one Vec per call) and RECORDS the messages it
/// received on each call, so tests can assert exactly what the LLM saw.
pub struct MockProvider {
    turns: Mutex<VecDeque<Vec<StreamEvent>>>,
    /// Per chat_stream call: the received messages as (role_debug, text).
    pub received: ReceivedLog,
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
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let snapshot: Vec<(String, String)> = messages
            .iter()
            .map(|m| (format!("{:?}", m.role), m.text.clone()))
            .collect();
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
        Self {
            open_error: Some(e),
            events: Mutex::new(None),
        }
    }
    /// `chat_stream` opens OK and yields `events` once.
    pub fn events(events: Vec<StreamEvent>) -> Self {
        Self {
            open_error: None,
            events: Mutex::new(Some(events)),
        }
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
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        if let Some(e) = &self.open_error {
            return Err(e.clone());
        }
        let events = self.events.lock().unwrap().take().unwrap_or_default();
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// Opens OK but its stream PENDS FOREVER on the first `stall_calls` `chat_stream`
/// calls (driving the stream idle-timeout), then returns `events` on the next
/// call. Drives the RECONNECT path: with `stall_calls = 1` the first attempt
/// times out and the reconnect succeeds; a large `stall_calls` exhausts the
/// reconnect budget and the turn clean-fails.
pub struct StallThenProvider {
    stall_calls: usize,
    calls: AtomicUsize,
    events: Mutex<Option<Vec<StreamEvent>>>,
}

/// First call emits a replay-unsafe prefix and then stalls; later calls return a
/// normal scripted response. Used to verify partial-stream continuation without
/// replaying the original request.
pub struct PartialStallThenProvider {
    calls: AtomicUsize,
    requests: Mutex<Vec<Vec<Message>>>,
    prefix: Mutex<Option<Vec<StreamEvent>>>,
    recovered: Mutex<Option<Vec<StreamEvent>>>,
    recovered_stalls: bool,
}

impl PartialStallThenProvider {
    pub fn new(prefix: Vec<StreamEvent>, recovered: Vec<StreamEvent>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            prefix: Mutex::new(Some(prefix)),
            recovered: Mutex::new(Some(recovered)),
            recovered_stalls: false,
        }
    }

    pub fn with_recovered_stall(mut self) -> Self {
        self.recovered_stalls = true;
        self
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl LlmProvider for PartialStallThenProvider {
    fn model_name(&self) -> &str {
        "partial-stall-then"
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.requests.lock().unwrap().push(messages.to_vec());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let prefix = self.prefix.lock().unwrap().take().unwrap_or_default();
            return Ok(Box::pin(
                futures::stream::iter(prefix).chain(futures::stream::pending()),
            ));
        }
        let recovered = self.recovered.lock().unwrap().take().unwrap_or_default();
        if self.recovered_stalls {
            Ok(Box::pin(
                futures::stream::iter(recovered).chain(futures::stream::pending()),
            ))
        } else {
            Ok(Box::pin(futures::stream::iter(recovered)))
        }
    }
}

impl StallThenProvider {
    pub fn new(stall_calls: usize, events: Vec<StreamEvent>) -> Self {
        Self {
            stall_calls,
            calls: AtomicUsize::new(0),
            events: Mutex::new(Some(events)),
        }
    }
}

#[async_trait]
impl LlmProvider for StallThenProvider {
    fn model_name(&self) -> &str {
        "stall-then"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.stall_calls {
            // Open OK, then never yield → the kernel's stream idle-timeout fires.
            Ok(Box::pin(futures::stream::pending::<StreamEvent>()))
        } else {
            let events = self.events.lock().unwrap().take().unwrap_or_default();
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }
}

/// Scripted, RICH-recording provider for prefix-cache regression tests. Unlike
/// `MockProvider` (which snapshots only `(role, text)` and discards tools /
/// tool_calls), this records the FULL `(Vec<Message>, Vec<ToolDef>, ChatOptions)`
/// it received on every `chat_stream` call — so a test can byte-compare the exact
/// wire prefix (history + tool block) the provider saw across rounds and turns,
/// AND assert which neutral `ChatOptions` reached the provider on each call.
///
/// Each call pops the next scripted `Vec<StreamEvent>`; an empty queue yields a
/// bare `Done { truncated: false }`. This drives multi-round turns (call 1 → a
/// ToolCall then Done; call 2 → TextDelta then Done → no calls → turn ends) and
/// multi-turn sessions.
pub struct RecordingProvider {
    turns: Mutex<VecDeque<Vec<StreamEvent>>>,
    calls: CallLog,
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
    /// the builder so the test can inspect what the LLM saw afterwards. Each entry
    /// is `(messages, tools, options)` — the `.0`/`.1` history/tool-block view is
    /// unchanged; `.2` is the neutral `ChatOptions` that reached the provider.
    pub fn calls(&self) -> CallLog {
        self.calls.clone()
    }
    /// A point-in-time snapshot of every recorded `(messages, tools, options)` call.
    pub fn recorded(&self) -> Vec<(Vec<Message>, Vec<ToolDef>, ChatOptions)> {
        self.calls.lock().unwrap().clone()
    }
    /// Just the `ChatOptions` received on each call, in call order — convenience
    /// for tests that only assert which options reached the provider.
    pub fn recorded_options(&self) -> Vec<ChatOptions> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, o)| o.clone())
            .collect()
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
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.calls
            .lock()
            .unwrap()
            .push((messages.to_vec(), tools.to_vec(), options.clone()));
        let events = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![StreamEvent::Done { truncated: false }]);
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// A provider whose stream OPENS OK, optionally yields a FIXED prefix of events,
/// then PENDS FOREVER (never resolves, never ends) — modelling a TCP half-open /
/// model stall where the connection is alive but no further tokens arrive. Used
/// by the stream-timeout liveness test: without a `stream_timeout` the kernel's
/// `stream.next().await` parks the turn forever; with one it cleanly fails.
///
/// Implemented by chaining the scripted prefix with `futures::stream::pending()`,
/// which is a stream that NEVER yields (its `poll_next` is always `Poll::Pending`)
/// and never terminates — so the consumer's `next()` await never completes.
pub struct SilentStreamProvider {
    prefix: Mutex<Option<Vec<StreamEvent>>>,
}

impl SilentStreamProvider {
    /// Open OK, emit `prefix` once, then pend forever. Pass `vec![]` for a stream
    /// that is silent from the very first token (bounds first-token latency); pass
    /// e.g. `vec![StreamEvent::TextDelta("hi".into())]` to go silent AFTER some
    /// output (bounds inter-token latency).
    pub fn new(prefix: Vec<StreamEvent>) -> Self {
        Self {
            prefix: Mutex::new(Some(prefix)),
        }
    }
}

#[async_trait]
impl LlmProvider for SilentStreamProvider {
    fn model_name(&self) -> &str {
        "silent-stream"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let prefix = self.prefix.lock().unwrap().take().unwrap_or_default();
        // The scripted prefix, then a stream that is forever Pending and never
        // ends → the consumer's next await after the prefix parks forever.
        let stream = futures::stream::iter(prefix).chain(futures::stream::pending());
        Ok(Box::pin(stream))
    }
}

/// Always opens OK and yields the SAME non-empty stop response (`TextDelta(text)`
/// then `Done`) on EVERY call — a model that produces a normal, CONTENT-BEARING
/// stop (no tool calls) every round, forever. Distinct from `MockProvider::new(
/// vec![])` (a CONTENT-FREE `Done`, which the kernel now treats as a transient
/// empty-200 and RETRIES): this is a legitimate completion, so it exercises the
/// `offer_continuation` / continuation-fuse paths over many rounds without
/// tripping the empty-response retry.
pub struct AlwaysStopProvider {
    text: String,
}

impl AlwaysStopProvider {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[async_trait]
impl LlmProvider for AlwaysStopProvider {
    fn model_name(&self) -> &str {
        "always-stop"
    }
    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta(self.text.clone()),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

/// A safe tool.
pub struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "Echo the arguments back"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: format!("echo: {args}"),
            is_error: false,
            images: vec![],
        }
    }
}

/// A tool that COUNTS how many times its `execute` was actually invoked, into a
/// shared `AtomicUsize`. Used by the dedup-gate tests to prove a suppressed
/// duplicate call does NOT execute (the counter stays at the number of calls the
/// gate let through, not the number the model emitted). Each execution also bumps
/// the counter into its result content so the driver can see ordering.
pub struct CountingTool {
    pub count: Arc<AtomicUsize>,
}

impl CountingTool {
    pub fn new(count: Arc<AtomicUsize>) -> Self {
        Self { count }
    }
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "count"
    }
    fn description(&self) -> &str {
        "Increments a shared counter each time it executes"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"k": {"type": "string"}}})
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        ToolResult {
            call_id: String::new(),
            content: format!("count#{n} args={args}"),
            is_error: false,
            images: vec![],
        }
    }
}

/// A shared, LATE-BOUND handle to the session's `commands` sender. The session's
/// `cmd_tx` only exists AFTER `spawn()`, but a tool must be mounted BEFORE — this
/// slot bridges that: a test creates it, builds an [`InjectCommandTool`] over a
/// clone, spawns, then fills the slot with `handle.commands.clone()`.
pub type DeferredCommands = Arc<Mutex<Option<UnboundedSender<AgentCommand>>>>;

/// A tool that, the FIRST time it executes, INJECTS a pre-configured
/// `AgentCommand` back into the session over a LATE-BOUND `commands` handle — a
/// DETERMINISTIC mid-turn injection point. Because the kernel runs `execute`
/// between the assistant's tool_call and the round completing, the injected
/// command is delivered while the turn is still in flight (the mid-turn select),
/// proving the kernel QUEUES rather than drops it. Fires AT MOST ONCE (guarded by
/// an AtomicBool) so a multi-round turn does not re-inject.
pub struct InjectCommandTool {
    commands: DeferredCommands,
    to_send: Mutex<Option<AgentCommand>>,
    fired: AtomicBool,
}

impl InjectCommandTool {
    pub fn new(commands: DeferredCommands, to_send: AgentCommand) -> Self {
        Self {
            commands,
            to_send: Mutex::new(Some(to_send)),
            fired: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl Tool for InjectCommandTool {
    fn name(&self) -> &str {
        "inject"
    }
    fn description(&self) -> &str {
        "Injects a pre-configured AgentCommand mid-turn (test only)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        if !self.fired.swap(true, Ordering::SeqCst) {
            let cmd = self.to_send.lock().unwrap().take();
            if let (Some(cmd), Some(tx)) = (cmd, self.commands.lock().unwrap().clone()) {
                let _ = tx.send(cmd);
            }
            // Yield repeatedly so the turn future stays PENDING for a window after
            // the command is enqueued — this guarantees the session's mid-turn
            // `tokio::select!` gets polled while the turn is still in flight and
            // drains the just-injected command (the deterministic mid-turn proof).
            // Without this, an all-ready mock turn can run to completion in a single
            // poll and the command would only be seen at idle, masking the bug.
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
        }
        ToolResult {
            call_id: String::new(),
            content: "injected".into(),
            is_error: false,
            images: vec![],
        }
    }
}

/// A risky tool — declares RiskLevel::Risky. (Pretends to write; does nothing.)
pub struct RiskyWriteTool;

#[async_trait]
impl Tool for RiskyWriteTool {
    fn name(&self) -> &str {
        "risky_write"
    }
    fn description(&self) -> &str {
        "Pretend to write a file (risky)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}})
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: format!("wrote: {args}"),
            is_error: false,
            images: vec![],
        }
    }
}

/// Injects one "keep going" reminder the first time the model tries to stop,
/// then lets it complete. Proves turn-level injection changes the loop.
pub struct ContinueOnceHook {
    used: AtomicBool,
}

impl ContinueOnceHook {
    pub fn new() -> Self {
        Self {
            used: AtomicBool::new(false),
        }
    }
}

impl Default for ContinueOnceHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LifecycleHooks for ContinueOnceHook {
    async fn offer_continuation(&self, _convo: &Conversation) -> Option<String> {
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
        Self {
            granted: Arc::new(Mutex::new(HashSet::new())),
        }
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
    ) -> BeforeOutcome {
        // Safe call (arg-aware) → no approval.
        if tool.risk(&call.arguments) == RiskLevel::Safe {
            return BeforeOutcome::Proceed;
        }
        // Session grant cache: an identical risky call already approved-always.
        let key = Self::grant_key(call);
        if self.granted.lock().unwrap().contains(&key) {
            return BeforeOutcome::Proceed;
        }
        // Round-trip to the driver for a decision.
        let decision = rt
            .request(
                "approval",
                serde_json::json!({ "tool": tool.name(), "args": call.arguments }),
            )
            .await;
        if decision.get("decision").and_then(|d| d.as_str()) != Some("allow") {
            return BeforeOutcome::deny("denied");
        }
        // "remember" → cache the grant so the same command is not asked again.
        if decision.get("remember").and_then(|r| r.as_bool()) == Some(true) {
            self.granted.lock().unwrap().insert(key);
        }
        BeforeOutcome::Proceed
    }
}

/// Records every lifecycle callback it receives — used to prove the kernel wires
/// the FULL LifecycleHooks surface (no dead methods).
pub struct RecorderHook {
    pub log: Arc<Mutex<Vec<String>>>,
}

impl RecorderHook {
    pub fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
        }
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
    async fn session_start(&self, _convo: &mut Conversation, _resumed: bool) {
        self.record("session_start");
    }
    async fn user_prompt_submit(&self, _text: &mut String) -> Result<(), String> {
        self.record("user_prompt_submit");
        Ok(())
    }
    async fn turn_start(&self, _convo: &mut Conversation) {
        self.record("turn_start");
    }
    async fn pre_request(&self, _messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        self.record("pre_request");
    }
    async fn on_request(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
        _ctx: &TurnCtx,
    ) {
        self.record("on_request");
    }
    async fn on_text_delta(&self, _delta: &mut String) {
        self.record("on_text_delta");
    }
    async fn on_reasoning_delta(&self, _delta: &mut String) {
        self.record("on_reasoning_delta");
    }
    async fn on_model_response(&self, _response: &mut Message) {
        self.record("on_model_response");
    }
    async fn offer_continuation(&self, _convo: &Conversation) -> Option<String> {
        self.record("offer_continuation");
        None
    }
    async fn turn_complete(&self, _convo: &Conversation, _reason: &StopReason, _ctx: &TurnCtx) {
        self.record("turn_complete");
    }
    async fn on_error(&self, _error: &str) {
        self.record("on_error");
    }
    async fn session_end(&self, _convo: &Conversation) {
        self.record("session_end");
    }
}

/// Projects current context-utilization back into the request as a TAIL reminder
/// so the LLM perceives its own budget pressure. Reads the latest meta from
/// history and appends ONE synthetic message at the END — never mutating history
/// → prefix-cache safe.
pub struct BudgetReminderHook;

#[async_trait]
impl LifecycleHooks for BudgetReminderHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        let util = messages
            .iter()
            .rev()
            .find_map(|m| m.meta.as_ref().map(|x| x.utilization));
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
        ToolResult {
            call_id: String::new(),
            content: format!("ran: {args}"),
            is_error: false,
            images: vec![],
        }
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
    ) -> BeforeOutcome {
        call.arguments = "{\"rewritten\":true}".to_string();
        BeforeOutcome::Proceed
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
    ) -> BeforeOutcome {
        BeforeOutcome::deny("blocked by policy")
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
    async fn after(
        &self,
        result: &mut ToolResult,
        _tool: Option<&std::sync::Arc<dyn crate::tool::Tool>>,
    ) -> AfterOutcome {
        result.content = format!("[truncated] {}", result.content);
        AfterOutcome::Proceed
    }
}

/// On `session_start` AND `turn_start`, appends `marker` to a SHARED log and pushes
/// a `[marker]` system message into the conversation. Two of these (sharing a log)
/// prove a `HookChain` fans out to BOTH hooks in registration order — the case that
/// was structurally impossible when the Agent held a single hook.
pub struct MarkerHook {
    marker: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl MarkerHook {
    pub fn new(marker: impl Into<String>, log: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            marker: marker.into(),
            log,
        }
    }
}

#[async_trait]
impl LifecycleHooks for MarkerHook {
    async fn session_start(&self, convo: &mut Conversation, _resumed: bool) {
        self.log
            .lock()
            .unwrap()
            .push(format!("session_start:{}", self.marker));
        convo.push(Message::system(format!("[{}]", self.marker)));
    }
    async fn turn_start(&self, convo: &mut Conversation) {
        self.log
            .lock()
            .unwrap()
            .push(format!("turn_start:{}", self.marker));
        convo.push(Message::system(format!("[{}]", self.marker)));
    }
}

/// `pre_request` appends ONE distinct tail reminder (`[tail]`) to the ephemeral
/// outgoing messages. Two of these prove `pre_request` composes (both tails reach
/// the provider, in registration order) while never mutating stored history.
pub struct TailReminderHook {
    tail: String,
}

impl TailReminderHook {
    pub fn new(tail: impl Into<String>) -> Self {
        Self { tail: tail.into() }
    }
}

#[async_trait]
impl LifecycleHooks for TailReminderHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        messages.push(Message::user(format!("[{}]", self.tail)));
    }
}

/// `user_prompt_submit` APPENDS `suffix` to the prompt text and records that it
/// ran into a shared log — proving text mutations chain into later hooks and that
/// a hook AFTER a blocker is never reached (its name is absent from the log).
pub struct RewritePromptHook {
    name: String,
    suffix: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl RewritePromptHook {
    pub fn new(
        name: impl Into<String>,
        suffix: impl Into<String>,
        log: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            name: name.into(),
            suffix: suffix.into(),
            log,
        }
    }
}

#[async_trait]
impl LifecycleHooks for RewritePromptHook {
    async fn user_prompt_submit(&self, text: &mut String) -> Result<(), String> {
        self.log.lock().unwrap().push(self.name.clone());
        text.push_str(&self.suffix);
        Ok(())
    }
}

/// `user_prompt_submit` records it ran then BLOCKS with `Err(reason)`. Placed
/// between a rewrite hook and a would-run hook, it proves the chain short-circuits.
pub struct BlockingPromptHook {
    name: String,
    reason: String,
    log: Arc<Mutex<Vec<String>>>,
}

impl BlockingPromptHook {
    pub fn new(
        name: impl Into<String>,
        reason: impl Into<String>,
        log: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            name: name.into(),
            reason: reason.into(),
            log,
        }
    }
}

#[async_trait]
impl LifecycleHooks for BlockingPromptHook {
    async fn user_prompt_submit(&self, _text: &mut String) -> Result<(), String> {
        self.log.lock().unwrap().push(self.name.clone());
        Err(self.reason.clone())
    }
}

/// On `offer_continuation`, RECORDS it observed (into a shared log) and returns the
/// configured continuation (`Some(text)` or `None`). Three of these prove every
/// hook OBSERVES offer_continuation while only the FIRST `Some` (in registration order)
/// provides the continuation.
pub struct ObservingTurnEndHook {
    name: String,
    reply: Option<String>,
    log: Arc<Mutex<Vec<String>>>,
}

impl ObservingTurnEndHook {
    pub fn new(
        name: impl Into<String>,
        reply: Option<String>,
        log: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            name: name.into(),
            reply,
            log,
        }
    }
}

#[async_trait]
impl LifecycleHooks for ObservingTurnEndHook {
    async fn offer_continuation(&self, _convo: &Conversation) -> Option<String> {
        let mut log = self.log.lock().unwrap();
        log.push(self.name.clone());
        // Reply only the FIRST time this hook observes, so the loop terminates after
        // one continuation round (otherwise it would re-inject forever). "First" =
        // this hook's name now appears exactly once in the log.
        let first_time = log.iter().filter(|n| *n == &self.name).count() == 1;
        if first_time {
            self.reply.clone()
        } else {
            None
        }
    }
}

// ── REPLACEABLE compaction strategy doubles (claim 23) ───────────────────────
//
// Two SAME-trait, DIFFERENT-behavior `CompactionStrategy` impls — the explicit
// proof that the compaction seam is replaceable. Both only PROPOSE a
// `CompactionPlan` from a read-only `CompactionView`; the kernel remains the sole
// writer (`Conversation::apply_plan`), so neither can corrupt the sacred floor,
// net-loss, or cache-epoch invariants. A third double (`NeverShrinks…`) proves
// the net-loss guard refuses a non-shrinking plan.

/// "Summarize old turns" shape: drain `[sacred_floor .. len - keep_recent)` into a
/// single synthetic summary message. Carries the cold-summary compaction shape —
/// it removes whole old messages and replaces them with one short summary, so a
/// committed plan reduces the MESSAGE COUNT (and bytes).
pub struct SummarizeOldestStrategy {
    pub keep_recent: usize,
}

#[async_trait]
impl CompactionStrategy for SummarizeOldestStrategy {
    async fn plan(&self, view: &CompactionView<'_>) -> CompactionPlan {
        let len = view.messages.len();
        let floor = view.sacred_floor;
        // Drain everything between the protected prefix and the most-recent
        // `keep_recent` messages.
        let drain_to = len.saturating_sub(self.keep_recent);
        if drain_to <= floor {
            // Nothing eligible to drain → noop.
            return CompactionPlan::noop();
        }
        let n = drain_to - floor;
        CompactionPlan {
            drain_from: floor,
            drain_to,
            summary: Some(format!("[summary of {n} earlier messages]")),
            rewrites: vec![],
            resume_note: None,
        }
    }

    /// Mirrors `plan`: this strategy announces (does slow summary work) iff there is
    /// something between the protected prefix and the kept tail to drain.
    fn will_summarize(&self, view: &CompactionView<'_>) -> bool {
        view.messages.len().saturating_sub(self.keep_recent) > view.sacred_floor
    }
}

/// "Microcompact tool results" shape: rewrite the text of OLDER tool-result
/// messages (`tool_call_id.is_some()`) — every tool result EXCEPT the most recent
/// one — to a short `[elided]` stub, IN PLACE (no drain). Carries the in-place
/// microcompact shape — it keeps the MESSAGE COUNT unchanged but shrinks bytes by
/// stubbing stale tool output.
pub struct StubToolResultsStrategy;

#[async_trait]
impl CompactionStrategy for StubToolResultsStrategy {
    async fn plan(&self, view: &CompactionView<'_>) -> CompactionPlan {
        // Indices of all tool-result messages, in order.
        let tool_idxs: Vec<usize> = view
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.tool_call_id.is_some())
            .map(|(i, _)| i)
            .collect();
        if tool_idxs.len() <= 1 {
            // 0 or 1 tool result → nothing "older than the most recent" to stub.
            return CompactionPlan::noop();
        }
        // Rewrite every tool result EXCEPT the last (most recent) one to a stub.
        // `apply_plan` interprets rewrite indices in the ORIGINAL `messages` space
        // (the same space `view.messages` is enumerated in here), so these indices
        // are correct as-is. This strategy never drains, so the original and the
        // post-drain candidate indices happen to coincide too.
        let rewrites = tool_idxs[..tool_idxs.len() - 1]
            .iter()
            .map(|&i| (i, "[elided]".to_string()))
            .collect();
        CompactionPlan {
            drain_from: 0,
            drain_to: 0,
            summary: None,
            rewrites,
            resume_note: None,
        }
    }
}

/// A strategy whose plan NEVER shrinks the conversation: it proposes a drain whose
/// inserted summary is LONGER than the bytes it removes (so `apply_plan`'s net-loss
/// guard must REFUSE it → committed=false, epoch unchanged). Proves a bad/ineffective
/// strategy cannot burn a cache epoch or mutate history.
pub struct NeverShrinksStrategy;

#[async_trait]
impl CompactionStrategy for NeverShrinksStrategy {
    async fn plan(&self, view: &CompactionView<'_>) -> CompactionPlan {
        let len = view.messages.len();
        let floor = view.sacred_floor;
        if len <= floor {
            return CompactionPlan::noop();
        }
        // Drain exactly ONE message past the floor, but insert a summary far larger
        // than any plausible drained message → net BIGGER → refused.
        CompactionPlan {
            drain_from: floor,
            drain_to: floor + 1,
            summary: Some("X".repeat(100_000)),
            rewrites: vec![],
            resume_note: None,
        }
    }
}

// ── SUBAGENT-BY-COMPOSITION doubles (claim 31) ───────────────────────────────
//
// A SUBAGENT is NOT a kernel concept. It is an L2 PATTERN: a parent agent spawns a
// child agent for an isolated sub-task by mounting a `Tool` whose `execute` BUILDS
// and RUNS a child `Agent`. These doubles prove the kernel supports that purely BY
// COMPOSITION — using ONLY `Agent` + `Tool` + `run_to_completion` and the two new
// builder seams (`working_dir`, `cancel_token`). The kernel gains NO "subagent"
// type. See `tests/subagent.rs`.

/// A tool that, in `execute`, BUILDS a fresh child `Agent` and runs it to
/// completion on the parent's tool args (the "subtask"). It is the WHOLE subagent
/// pattern: a Tool running a child Agent via the existing one-shot adapter — no new
/// kernel concept.
///
/// It carries no shared mutable provider state of its own — instead it holds two
/// FACTORIES (`Fn() -> ...`) so each `execute` mints a FRESH child provider and a
/// FRESH `MountedTools` (neither is `Clone`, and a child session consumes its
/// provider/tools). The child is built with:
/// * `.working_dir(child_dir)` when `child_dir` is `Some` (SEAM 1 — dir isolation),
/// * `.cancel_token(ctx.cancel.child_token())` ALWAYS (SEAM 2 — the parent's
///   per-turn cancel propagates into the DETACHED child task), then
/// * `run_to_completion(args, AllowAll)`.
///
/// The child's `Outcome.text` becomes this tool's result content; the result is an
/// error iff the child did not stop cleanly (`stop != Stopped`).
pub struct SubAgentTool {
    name: String,
    /// Fresh child provider per execution (a session consumes its provider).
    make_provider: Box<dyn Fn() -> Arc<dyn LlmProvider> + Send + Sync>,
    /// Fresh mounted tool subset per execution (`MountedTools` is not `Clone`).
    make_tools: Box<dyn Fn() -> MountedTools + Send + Sync>,
    persona: String,
    /// SEAM 1: when `Some`, the child runs dir-scoped to this path (distinct from
    /// the parent's). `None` = the child inherits the default (process cwd) behavior.
    child_dir: Option<std::path::PathBuf>,
}

impl SubAgentTool {
    pub fn new(
        name: impl Into<String>,
        make_provider: impl Fn() -> Arc<dyn LlmProvider> + Send + Sync + 'static,
        make_tools: impl Fn() -> MountedTools + Send + Sync + 'static,
        persona: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            make_provider: Box::new(make_provider),
            make_tools: Box::new(make_tools),
            persona: persona.into(),
            child_dir: None,
        }
    }
    /// SEAM 1: scope the child agent to a distinct working dir.
    pub fn child_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.child_dir = Some(dir);
        self
    }
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "Spawn a child agent for an isolated sub-task (subagent-by-composition)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {"task": {"type": "string"}}})
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        // Build the CHILD agent purely from kernel primitives. SEAM 2: derive the
        // child's cancel source from the PARENT's per-turn token (a child token) so
        // a parent cancel reaches the DETACHED child session. SEAM 1: optionally pin
        // the child's tool working_dir.
        let mut builder = Agent::builder()
            .provider((self.make_provider)())
            .tools((self.make_tools)())
            .persona(self.persona.clone())
            .cancel_token(ctx.cancel.child_token());
        if let Some(dir) = &self.child_dir {
            builder = builder.working_dir(dir.clone());
        }
        let child = builder.build();
        let task = args.to_string();
        // DETACH the child run onto its own `tokio::spawn` task, then await its
        // JoinHandle. This is the load-bearing shape for the cancel-propagation
        // claim: if the PARENT's tool future is dropped on cancel, this awaited
        // JoinHandle is dropped too — but the spawned run task KEEPS RUNNING
        // (a dropped JoinHandle detaches, it does NOT abort). So the child's command
        // channel stays open (no future-drop teardown), and the ONLY thing that can
        // stop the still-running child is the cancel TOKEN cascading in via the
        // `.cancel_token(ctx.cancel.child_token())` wired above. (`run_to_completion`
        // already spawns the child SESSION detached; spawning the DRIVER here too
        // makes the whole child genuinely independent of the parent tool future.)
        let outcome =
            tokio::spawn(async move { child.run_to_completion(task, AutoRespond::AllowAll).await })
                .await
                // A child run task should not itself panic; if it did (or was aborted),
                // surface a failure outcome rather than unwrapping.
                .unwrap_or_default();
        // Surface the child's work as this tool's content. Prefer the child's
        // assistant text; if it produced none (a tool-only child, e.g. a probe),
        // fall back to its tool-result contents so the child's actual output still
        // flows back up. (`content: outcome.text` per the spec, with this fallback
        // so a tool-only subagent is not silently empty.)
        let is_error = outcome.stop != StopReason::Stopped;
        let content = if is_error {
            // A failed child must surface its CAUSE, not an empty errored result.
            // `Outcome.error` carries the last error message (kernel populates it);
            // include it + the StopReason so the parent/model sees why the subagent
            // failed instead of a blank is_error=true blob.
            format!(
                "subagent failed ({:?}): {}",
                outcome.stop,
                outcome.error.unwrap_or_else(|| "<no error message>".into())
            )
        } else if !outcome.text.is_empty() {
            outcome.text
        } else {
            // Tool-only child (e.g. a probe): fall back to its tool-result contents
            // so the child's actual output still flows back up.
            outcome
                .tool_results
                .iter()
                .map(|r| r.content.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        ToolResult {
            // call_id is set by the kernel after execute returns.
            call_id: String::new(),
            content,
            is_error,
            images: vec![],
        }
    }
}

/// A tool whose result content is exactly the `working_dir` it saw in its
/// `ToolContext` — so a test can assert WHICH dir the (child) agent's tool context
/// reported. Proves SEAM 1 is per-agent, not process-global.
pub struct WorkingDirProbeTool;

#[async_trait]
impl Tool for WorkingDirProbeTool {
    fn name(&self) -> &str {
        "working_dir_probe"
    }
    fn description(&self) -> &str {
        "Returns the ToolContext working_dir it saw"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &str, ctx: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: ctx.working_dir.display().to_string(),
            is_error: false,
            images: vec![],
        }
    }
}

/// A cooperative tool that BLOCKS until its `ctx.cancel` fires, then flips a shared
/// `observed` flag and returns. Used by the subagent cancel-propagation test: a
/// CHILD mounts this and its provider calls it, so the child session parks inside
/// `execute` on the child's own cancel token. When the parent is cancelled, the
/// parent's per-turn token (whose child the child-agent's cancel_token is) fires,
/// the child's per-turn token (a grandchild) fires, this tool observes it, and
/// `observed` flips — proving cancel propagated into the DETACHED child task.
pub struct BlockUntilCancelTool {
    pub observed: Arc<AtomicBool>,
}

impl BlockUntilCancelTool {
    pub fn new(observed: Arc<AtomicBool>) -> Self {
        Self { observed }
    }
}

#[async_trait]
impl Tool for BlockUntilCancelTool {
    fn name(&self) -> &str {
        "block_until_cancel"
    }
    fn description(&self) -> &str {
        "Blocks until ctx.cancel fires, then flips a shared observed flag"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &str, ctx: &ToolContext) -> ToolResult {
        ctx.cancel.cancelled().await;
        self.observed.store(true, Ordering::SeqCst);
        ToolResult {
            call_id: String::new(),
            content: "observed cancel".into(),
            is_error: false,
            images: vec![],
        }
    }
}

/// A `LifecycleHooks` that returns a FIXED `on_rate_limit` verdict — lets tests
/// drive the kernel's 429 branch (wait-and-retry vs pause) without any network
/// or usage data.
pub struct ScriptedRateLimitHook {
    decision: RateLimitDecision,
}

impl ScriptedRateLimitHook {
    pub fn new(decision: RateLimitDecision) -> Self {
        Self { decision }
    }
}

#[async_trait]
impl LifecycleHooks for ScriptedRateLimitHook {
    async fn on_rate_limit(&self, _hint: &RateLimitHint) -> Option<RateLimitDecision> {
        Some(self.decision.clone())
    }
}

/// A MockProvider variant that, on a specified `inject_on_call` (1-based), sends a
/// `SendMessage` command into a DEFERRED command sender (filled after spawn) and
/// then yields between events so the driver's select-loop has a guaranteed
/// opportunity to process the steer before the next model round begins.
///
/// This makes steer-buffer tests deterministic without relying on task-scheduler
/// timing: the steer is in cmd_rx before the stream's first yield, the driver
/// processes it during that yield, and it is in steer.lock() by the time the
/// next round's drain runs.
pub struct DeferredSteerProvider {
    inner: MockProvider,
    /// Which chat_stream call (1-based) triggers the steer injection.
    inject_on_call: usize,
    /// The steer text to inject.
    inject_text: String,
    /// Deferred command sender — filled by the test after `handle = agent.spawn()`.
    inject_cmd: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::event::AgentCommand>>>>,
    call_count: std::sync::atomic::AtomicUsize,
}

impl DeferredSteerProvider {
    pub fn new(
        turns: Vec<Vec<StreamEvent>>,
        inject_on_call: usize,
        inject_text: impl Into<String>,
        inject_cmd: Arc<
            Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::event::AgentCommand>>>,
        >,
    ) -> Self {
        Self {
            inner: MockProvider::new(turns),
            inject_on_call,
            inject_text: inject_text.into(),
            inject_cmd,
            call_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    /// Expose the received log from the inner MockProvider for assertions.
    pub fn received(&self) -> ReceivedLog {
        self.inner.received.clone()
    }
}

#[async_trait]
impl LlmProvider for DeferredSteerProvider {
    fn model_name(&self) -> &str {
        "deferred-steer"
    }
    fn context_window(&self) -> u32 {
        0
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let call_n = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if call_n == self.inject_on_call {
            // Inject steer into the driver's cmd_rx. The steer is now in the channel
            // and will be processed into steer.lock() at the next driver select! poll.
            if let Some(tx) = self.inject_cmd.lock().unwrap().as_ref() {
                let _ = tx.send(crate::event::AgentCommand::SendMessage {
                    text: self.inject_text.clone(),
                    images: vec![],
                });
            }
        }
        // Delegate recording + scripted events to inner provider.
        let inner_stream = self.inner.chat_stream(messages, tools, options).await?;
        // Wrap the stream with a real async pause at the START so the driver's
        // select! has an EXCLUSIVE window to process the steer from cmd_rx into
        // steer.lock() before the stream emits any events.
        //
        // `sleep(1ms)` is used rather than `yield_now` because yield_now just
        // re-schedules the current task; the outer select! might pick `turn` again
        // (it is immediately re-ready after yield_now), causing a race. A real sleep
        // means `turn` is genuinely NOT ready for at least 1ms, so the outer select!
        // MUST pick cmd_rx if data is present — eliminating the race.
        let yielding = futures::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            None::<StreamEvent>
        })
        .filter_map(futures::future::ready)
        .chain(inner_stream);
        Ok(Box::pin(yielding))
    }
}

/// A trivial tool that always succeeds immediately (name `"noop"`, returns Ok).
/// Used by steer-buffer tests that need a real tool call but don't care about output.
pub struct NoopTool;

#[async_trait]
impl Tool for NoopTool {
    fn name(&self) -> &str {
        "noop"
    }
    fn description(&self) -> &str {
        "Does nothing and returns Ok"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: "noop".into(),
            is_error: false,
            images: vec![],
        }
    }
}

/// A read-only tool that (a) bumps a shared "in-flight" counter on entry and
/// records the peak, (b) awaits a short tokio sleep so concurrent instances
/// overlap under `tokio::time`, (c) decrements on exit. Proves real overlap and
/// enforces the cap. `read_only_hint` = true so it is `parallel_safe`.
pub struct ConcurrencyProbeTool {
    pub name: &'static str,
    pub inflight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub delay_ms: u64,
}

#[async_trait]
impl Tool for ConcurrencyProbeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        ""
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        ToolResult {
            call_id: String::new(),
            content: self.name.into(),
            is_error: false,
            images: vec![],
        }
    }
}

/// A probe whose `parallel_safe` is decided by its ARGS (mirrors BashTool: the
/// command string decides). If args contain `"ro":true` it is parallel-safe and
/// overlaps; else it serializes. Shares inflight/peak counters like
/// `ConcurrencyProbeTool` so a test can assert overlap vs. barrier.
///
/// The `name` field is used in `execute` output so tests can check which tool ran.
/// Fields are `pub` to allow construction from integration tests.
pub struct ArgGatedProbeTool {
    pub name: &'static str,
    pub inflight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub peak: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub delay_ms: u64,
}

#[async_trait]
impl Tool for ArgGatedProbeTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        ""
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn parallel_safe(&self, args: &str) -> bool {
        args.contains("\"ro\":true")
    }
    async fn execute(&self, _args: &str, _c: &ToolContext) -> ToolResult {
        let now = self.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        self.inflight.fetch_sub(1, Ordering::SeqCst);
        ToolResult {
            call_id: String::new(),
            content: self.name.into(),
            is_error: false,
            images: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::{RateLimitDecision, RateLimitHint};

    #[tokio::test]
    async fn scripted_rate_limit_hook_returns_programmed_decision() {
        let hook = ScriptedRateLimitHook::new(RateLimitDecision::WaitAndRetry { secs: 7 });
        let got = hook
            .on_rate_limit(&RateLimitHint {
                http_status: Some(429),
                retry_after_secs: None,
                terminal: false,
                attempt: 1,
            })
            .await;
        assert_eq!(got, Some(RateLimitDecision::WaitAndRetry { secs: 7 }));
    }
}
