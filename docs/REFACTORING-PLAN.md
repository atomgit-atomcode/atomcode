# AtomCode Refactoring Plan: Claude Code Architecture Alignment

**Version:** 1.0
**Date:** 2026-03-19
**Author:** Chief Architect
**Status:** Draft / Proposal

---

## Motivation

AtomCode's current architecture has drifted from its stated principles. The ARCHITECTURE.md document honestly catalogs 14 known weaknesses. The most critical are:

1. **`app.rs` is a 1450-line God Object** — agent loop, tool dispatch, UI state, history management, provider switching, clipboard, input, and file attachment all live in one struct with 30+ fields.
2. **Claude and Ollama providers cannot participate in the agent loop** — 2 of 3 providers ignore the `tools` parameter entirely.
3. **No streaming cancellation** — cancelled tasks continue running in the background.
4. **No token budget management** — fixed message-count windowing with no awareness of actual token counts.
5. **No parallel tool calls** — hardcoded `parallel_tool_calls: false`.
6. **BashTool working directory is fragile** — re-instantiated on every call as a workaround.

This plan restructures AtomCode to follow Claude Code's architecture patterns: a standalone agent loop decoupled from the UI, a fully pluggable tool system with dynamic registration, proper cancellation propagation, token-aware context management, and a permission system that goes beyond binary approve/deny.

---

## Part 1: Architecture Restructuring

### 1.1 Extract AgentLoop from App (God Object decomposition)

**Problem:** `App` currently holds both UI state (scroll position, render cache, input state, slash menu) and agent state (conversation, tool registry, provider, working directory, tool call count, retry count). The agent loop is implicit — scattered across `send_message()`, `handle_tool_call()`, `execute_tool()`, `handle_tool_result()`, and `continue_agent_loop()` as methods on `App`.

**Target:** Two distinct structs with clear ownership boundaries. Communication via an async channel pair.

#### New struct: `AgentLoop` (lives in `atomcode-core`)

```rust
// crates/atomcode-core/src/agent/mod.rs

pub struct AgentLoop {
    // --- Core state ---
    conversation: Conversation,
    tool_registry: ToolRegistry,
    provider: Box<dyn LlmProvider>,
    working_dir: PathBuf,

    // --- Execution state ---
    mode: AgentMode,
    cancellation: CancellationToken,
    tool_call_count: usize,
    retry_count: usize,
    turn_start: Option<Instant>,
    tool_start: Option<Instant>,

    // --- Token management ---
    token_budget: TokenBudget,

    // --- Permissions ---
    permissions: PermissionStore,

    // --- Communication ---
    /// Events flowing TO the UI (deltas, tool calls, completions, errors)
    ui_tx: mpsc::UnboundedSender<AgentEvent>,
    /// Commands flowing FROM the UI (send message, approve tool, cancel, etc.)
    cmd_rx: mpsc::UnboundedReceiver<AgentCommand>,
}

pub enum AgentMode {
    Idle,
    Streaming,
    WaitingApproval(ToolCall),
    ToolExecuting { name: String, call_id: String },
}

/// Commands the UI sends to the agent loop.
pub enum AgentCommand {
    SendMessage { text: String, attachments: Vec<Attachment> },
    ApproveToolCall { call_id: String },
    DenyToolCall { call_id: String, reason: String },
    Cancel,
    ChangeDirectory(PathBuf),
    SwitchProvider(Box<dyn LlmProvider>),
    Shutdown,
}

/// Events the agent loop sends to the UI.
pub enum AgentEvent {
    StreamDelta(String),
    ToolCallStart { id: String, name: String, args: String },
    ToolCallDelta(String),
    ToolCallComplete { id: String, name: String, result: ToolResult },
    ApprovalRequired(ToolCall, String),   // tool_call + reason
    TurnComplete { duration: Duration, tokens: TokenUsage },
    Error(String),
    ModeChanged(AgentMode),
    ConversationUpdated,   // signal UI to re-render messages
}
```

The `AgentLoop` runs as a standalone `tokio::spawn` task with its own `async fn run(&mut self)` method that:
1. Waits for `AgentCommand` from the UI
2. On `SendMessage`: builds system prompt, calls provider, drains the stream
3. On tool call: checks permissions, either auto-executes or sends `ApprovalRequired`
4. On `ApproveToolCall` / `DenyToolCall`: continues the loop
5. On `Cancel`: triggers the `CancellationToken`

```rust
impl AgentLoop {
    pub async fn run(&mut self) {
        loop {
            match self.cmd_rx.recv().await {
                Some(AgentCommand::SendMessage { text, attachments }) => {
                    self.handle_send(text, attachments).await;
                }
                Some(AgentCommand::ApproveToolCall { call_id }) => {
                    self.execute_approved_tool(call_id).await;
                }
                Some(AgentCommand::DenyToolCall { call_id, reason }) => {
                    self.handle_denial(call_id, reason).await;
                }
                Some(AgentCommand::Cancel) => {
                    self.cancel_current().await;
                }
                Some(AgentCommand::Shutdown) | None => break,
                // ... other commands
            }
        }
    }

    async fn agent_turn(&mut self) {
        // The explicit agent loop — replaces the implicit callback chain
        loop {
            self.cancellation = CancellationToken::new();
            let stream = self.call_provider().await;

            match self.drain_stream(stream).await {
                TurnOutcome::Done => break,
                TurnOutcome::ToolCall(call) => {
                    match self.check_permission(&call) {
                        Permission::Allow => {
                            let result = self.execute_tool(&call).await;
                            self.conversation.add_tool_result(result);
                            // loop continues — call provider again
                        }
                        Permission::Ask => {
                            let _ = self.ui_tx.send(AgentEvent::ApprovalRequired(call, reason));
                            // Wait for approval command — break inner loop,
                            // will resume when ApproveToolCall arrives
                            break;
                        }
                        Permission::Deny => {
                            self.conversation.add_tool_result(ToolResult::denied(&call));
                            // loop continues
                        }
                    }
                }
                TurnOutcome::Cancelled => break,
                TurnOutcome::Error(e) => {
                    if self.should_retry(&e) {
                        self.retry_count += 1;
                        continue;
                    }
                    let _ = self.ui_tx.send(AgentEvent::Error(e));
                    break;
                }
            }
        }
    }
}
```

#### Slimmed-down `App` (stays in `atomcode-tui`)

```rust
// crates/atomcode-tui/src/app.rs

pub struct App {
    // --- UI-only state ---
    pub mode: UiMode,
    pub input: InputState,
    pub scroll_offset: usize,
    pub at_bottom: bool,
    pub confirm_quit: bool,
    pub attached_files: Vec<AttachedFile>,
    pub slash_menu: SlashMenu,
    pub input_history: Vec<String>,
    pub history_index: Option<usize>,
    pub history_stash: Option<String>,
    pub suggestion: Option<String>,
    pub tick_count: usize,

    // --- Render state ---
    pub render_cache: Vec<Line<'static>>,
    pub render_cache_msg_count: usize,

    // --- Provider manager UI ---
    pub provider_mgr: Option<ProviderManager>,
    pub model_list: Vec<(String, String)>,
    pub model_selected: usize,

    // --- Display-only copies (read from AgentEvent) ---
    pub display_messages: Vec<DisplayMessage>,  // rendered conversation
    pub current_stream: Option<String>,         // current streaming text
    pub executing_tool_info: String,
    pub current_step_count: usize,
    pub total_tokens: usize,
    pub turn_tokens: usize,
    pub turn_start: Option<Instant>,
    pub last_turn_duration: Option<Duration>,
    pub working_dir: PathBuf,

    // --- Communication ---
    agent_tx: mpsc::UnboundedSender<AgentCommand>,
    agent_rx: mpsc::UnboundedReceiver<AgentEvent>,

    // --- Config ---
    pub config: Config,
}

pub enum UiMode {
    Normal,
    Streaming,
    WaitingApproval { tool_name: String, reason: String },
    ToolExecuting,
    ProviderManager,
    ModelSelector,
    Exiting,
}
```

The `App` never touches the provider or tool registry directly. It sends `AgentCommand`s and reacts to `AgentEvent`s.

#### Key benefits

- **Headless mode becomes trivial:** `AgentLoop` can run without any TUI. A CLI pipe mode, a test harness, or a future web frontend can all drive the same `AgentLoop` via `AgentCommand/AgentEvent`.
- **Testable agent logic:** The agent loop can be tested with mock providers and mock tools, without spinning up a terminal.
- **Clear ownership:** No more 30-field struct. App owns pixels, AgentLoop owns intelligence.

---

### 1.2 Proper Tool Dispatch (no hardcoded tool names)

**Problem:** BashTool is special-cased in `execute_tool()` — it is re-instantiated on every call with the current `working_dir`, bypassing the registry. CdTool execution is intercepted in `handle_tool_result()` to update the app's working directory. These are code smells where tool-specific logic leaks into the orchestrator.

**Target:** All tools go through `ToolRegistry` uniformly. Tools that need shared state access it through a `ToolContext`.

#### ToolContext: shared mutable state for tools

```rust
// crates/atomcode-core/src/tool/context.rs

use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared execution context available to all tools.
pub struct ToolContext {
    pub working_dir: Arc<RwLock<PathBuf>>,
    pub cancellation: CancellationToken,
    // Future: environment variables, shell history, etc.
}
```

#### Updated Tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDef;
    fn approval(&self, args: &str) -> ApprovalRequirement;
    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult>;

    /// Optional lifecycle hooks
    fn on_register(&self, _ctx: &ToolContext) {}
    fn on_unregister(&self) {}
}
```

#### BashTool reads working_dir from context

```rust
impl Tool for BashTool {
    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: BashArgs = serde_json::from_str(args)?;
        let working_dir = ctx.working_dir.read().await.clone();

        let mut child = Command::new("bash")
            .arg("-c")
            .arg(&parsed.command)
            .current_dir(&working_dir)
            // ...
    }
}
```

BashTool no longer stores `working_dir` as a field. It is registered once and reads the current directory from the shared context on every execution.

#### CdTool writes working_dir to context

```rust
impl Tool for CdTool {
    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: CdArgs = serde_json::from_str(args)?;
        let resolved = resolve_path(&parsed.path, &*ctx.working_dir.read().await)?;

        if resolved.is_dir() {
            *ctx.working_dir.write().await = resolved.clone();
            Ok(ToolResult {
                call_id: String::new(),
                output: format!("Changed directory to {}", resolved.display()),
                success: true,
            })
        } else {
            Ok(ToolResult {
                call_id: String::new(),
                output: format!("Not a directory: {}", resolved.display()),
                success: false,
            })
        }
    }
}
```

CdTool is now self-contained. No interception in `handle_tool_result()` is needed.

#### Dynamic registration

```rust
impl ToolRegistry {
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.definition().name.to_string();
        tool.on_register(&self.context);
        self.tools.insert(name, Arc::from(tool));
    }

    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(tool) = self.tools.remove(name) {
            tool.on_unregister();
            Some(tool)
        } else {
            None
        }
    }

    /// Execute a tool by name, going through the registry uniformly.
    pub async fn execute(&self, name: &str, args: &str) -> Result<ToolResult> {
        let tool = self.tools.get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown tool: {}", name))?;
        tool.execute(args, &self.context).await
    }
}
```

---

### 1.3 Streaming Cancellation (CancellationToken)

**Problem:** When the user presses Ctrl+C or Esc during streaming, the mode is set to Normal and stale events are filtered by checking the mode. But the spawned tokio task that drains the HTTP stream continues running. The HTTP request is never aborted. For tool executions (`tokio::spawn`), the background process continues.

**Target:** Use `tokio_util::sync::CancellationToken` propagated to every spawned task. Cancellation is immediate and deterministic.

#### Add dependency

```toml
# Cargo.toml (atomcode-core)
[dependencies]
tokio-util = { version = "0.7", features = ["sync"] }
```

#### Pass token to stream handler

```rust
impl AgentLoop {
    async fn drain_stream(
        &self,
        mut stream: Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>,
    ) -> TurnOutcome {
        let cancel = self.cancellation.clone();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    return TurnOutcome::Cancelled;
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(StreamEvent::Delta(text))) => {
                            let _ = self.ui_tx.send(AgentEvent::StreamDelta(text));
                        }
                        Some(Ok(StreamEvent::ToolCallDone(call))) => {
                            return TurnOutcome::ToolCall(call);
                        }
                        Some(Ok(StreamEvent::Done)) => {
                            return TurnOutcome::Done;
                        }
                        Some(Ok(StreamEvent::Error(e))) => {
                            return TurnOutcome::Error(e);
                        }
                        None => return TurnOutcome::Done,
                        _ => {} // other events
                    }
                }
            }
        }
    }
}
```

#### Pass token to tool execution

```rust
async fn execute_tool(&self, call: &ToolCall) -> ToolResult {
    let tool = self.tool_registry.get_arc(&call.name).unwrap();
    let ctx = ToolContext {
        working_dir: self.working_dir.clone(),
        cancellation: self.cancellation.child_token(),
    };

    tokio::select! {
        result = tool.execute(&call.arguments, &ctx) => {
            match result {
                Ok(mut r) => { r.call_id = call.id.clone(); r }
                Err(e) => ToolResult {
                    call_id: call.id.clone(),
                    output: format!("Tool error: {}", e),
                    success: false,
                },
            }
        }
        _ = self.cancellation.cancelled() => {
            ToolResult {
                call_id: call.id.clone(),
                output: "Cancelled by user.".to_string(),
                success: false,
            }
        }
    }
}
```

#### BashTool respects cancellation

```rust
impl Tool for BashTool {
    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        // ... spawn child process ...

        tokio::select! {
            result = wait_for_output(&mut child, timeout_secs) => {
                // normal completion
                result
            }
            _ = ctx.cancellation.cancelled() => {
                let _ = child.kill().await;
                Ok(ToolResult {
                    call_id: String::new(),
                    output: "Process killed: user cancelled.".to_string(),
                    success: false,
                })
            }
        }
    }
}
```

#### Cancel flow

```
User presses Ctrl+C
    |
    v
App sends AgentCommand::Cancel
    |
    v
AgentLoop.cancel_current():
    self.cancellation.cancel()      // all child tokens are cancelled
    |
    +--> drain_stream() sees cancel → returns TurnOutcome::Cancelled
    +--> execute_tool() sees cancel → kills child process
    +--> HTTP stream task sees cancel → drops connection (reqwest abort)
    |
    v
AgentLoop sends AgentEvent::ModeChanged(Idle)
    |
    v
App sets UiMode::Normal
```

No more stale event filtering. No more orphan tasks.

---

### 1.4 Permission System

**Problem:** The current `ApprovalRequirement` is a binary enum: `AutoApprove` or `RequireApproval(String)`. There is no way to always-allow a tool, always-deny it, or configure permissions per session. Every destructive bash command triggers a prompt every time.

**Target:** A four-level permission model, configurable per tool, with session memory.

#### Permission model

```rust
// crates/atomcode-core/src/permission.rs

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum PermissionLevel {
    /// Always execute without asking.
    AlwaysAllow,
    /// Ask the user each time (default for most tools).
    Ask,
    /// Auto-approve if the tool's own `approval()` says so, ask otherwise.
    ToolDefault,
    /// Never execute this tool.
    AlwaysDeny,
}

pub struct PermissionStore {
    /// Per-tool permission overrides (tool_name -> level).
    tool_overrides: HashMap<String, PermissionLevel>,
    /// Session-level "always allow" grants (tool_name -> set of argument patterns).
    session_grants: HashMap<String, HashSet<String>>,
    /// Default level for tools not in overrides.
    default_level: PermissionLevel,
}

pub enum PermissionDecision {
    Allow,
    Ask(String),   // reason to show user
    Deny(String),  // reason for denial
}

impl PermissionStore {
    pub fn check(&self, tool_name: &str, args: &str, tool_approval: ApprovalRequirement) -> PermissionDecision {
        // 1. Check tool-level override
        if let Some(level) = self.tool_overrides.get(tool_name) {
            match level {
                PermissionLevel::AlwaysAllow => return PermissionDecision::Allow,
                PermissionLevel::AlwaysDeny => return PermissionDecision::Deny(
                    format!("Tool '{}' is denied by configuration.", tool_name)
                ),
                PermissionLevel::Ask => { /* fall through to tool's own check */ }
                PermissionLevel::ToolDefault => { /* fall through */ }
            }
        }

        // 2. Check session grants
        if self.session_grants.get(tool_name)
            .map_or(false, |grants| grants.contains("*") || grants.contains(args))
        {
            return PermissionDecision::Allow;
        }

        // 3. Delegate to tool's approval() method
        match tool_approval {
            ApprovalRequirement::AutoApprove => PermissionDecision::Allow,
            ApprovalRequirement::RequireApproval(reason) => PermissionDecision::Ask(reason),
        }
    }

    /// Grant session-level always-allow for a tool (user pressed "Always Allow").
    pub fn grant_session(&mut self, tool_name: &str) {
        self.session_grants
            .entry(tool_name.to_string())
            .or_default()
            .insert("*".to_string());
    }
}
```

#### Config integration

```toml
# ~/.atomcode/config.toml

[permissions]
default = "tool_default"   # "always_allow" | "ask" | "tool_default" | "always_deny"

[permissions.tools]
bash = "ask"              # always prompt for bash
read_file = "always_allow"
write_file = "tool_default"
edit_file = "always_allow"
change_dir = "always_allow"
```

#### UI approval flow (expanded)

When the user is prompted for approval, they now have four options:

- **Y / Enter** — allow this one call
- **A** — always allow this tool for the rest of the session
- **N / Esc** — deny this one call (feed denial to LLM)
- **D** — always deny this tool for the rest of the session

---

## Part 2: LLM Provider Improvements

### 2.1 Claude Provider — implement native tool_use

**Problem:** The Claude provider ignores `_tools: Option<&[ToolDef]>` entirely. Tool role messages are sent as user messages. This makes Claude unable to participate in the agent loop.

**Target:** Full implementation of Claude's native `tool_use` content block format.

#### Message formatting for Claude tool_use

```rust
fn format_messages(messages: &[Message]) -> (Option<String>, Vec<serde_json::Value>) {
    let mut system = None;
    let mut msgs = Vec::new();

    for m in messages {
        match (&m.role, &m.content) {
            (Role::System, MessageContent::Text(s)) => {
                system = Some(s.clone());
            }
            (Role::User, MessageContent::Text(s)) => {
                msgs.push(json!({"role": "user", "content": s}));
            }
            (Role::Assistant, MessageContent::Text(s)) => {
                msgs.push(json!({"role": "assistant", "content": [
                    {"type": "text", "text": s}
                ]}));
            }
            (Role::Assistant, MessageContent::AssistantWithToolCalls { text, tool_calls }) => {
                let mut content = Vec::new();
                if let Some(t) = text {
                    content.push(json!({"type": "text", "text": t}));
                }
                for tc in tool_calls {
                    let args: serde_json::Value = serde_json::from_str(&tc.arguments)
                        .unwrap_or(json!({}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": args,
                    }));
                }
                msgs.push(json!({"role": "assistant", "content": content}));
            }
            (Role::Tool, MessageContent::ToolResult(r)) => {
                msgs.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": r.call_id,
                        "content": r.output,
                        "is_error": !r.success,
                    }]
                }));
            }
            _ => {}
        }
    }
    (system, msgs)
}
```

#### Tool definitions for Claude format

```rust
fn format_tools(tools: &[ToolDef]) -> Vec<serde_json::Value> {
    tools.iter().map(|td| json!({
        "name": td.name,
        "description": td.description,
        "input_schema": td.parameters,
    })).collect()
}
```

#### Content block streaming

Claude streams tool_use as content blocks:

```
event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_xxx","name":"bash","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}
```

The streaming parser must track content block type (text vs tool_use) and accumulate `input_json_delta` fragments to assemble the final tool call arguments.

```rust
// New event types for Claude parsing
#[derive(Deserialize)]
struct ClaudeStreamEvent {
    #[serde(rename = "type")]
    event_type: String,
    index: Option<usize>,
    content_block: Option<ContentBlock>,
    delta: Option<ClaudeDelta>,
    message: Option<ClaudeMessage>,
    usage: Option<ClaudeUsage>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    id: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct ClaudeDelta {
    #[serde(rename = "type")]
    delta_type: Option<String>,
    text: Option<String>,
    partial_json: Option<String>,
}
```

#### `max_tokens` made configurable

Replace the hardcoded `4096` with a provider-config field:

```rust
pub struct ProviderConfig {
    pub provider_type: String,
    pub api_key: Option<String>,
    pub model: String,
    pub base_url: Option<String>,
    pub system_prompt: Option<String>,
    pub max_tokens: Option<usize>,     // NEW — defaults by model
}
```

---

### 2.2 Multi-tool-call support

**Problem:** The OpenAI provider sets `parallel_tool_calls: false` and the streaming parser only tracks a single tool call buffer (`tc_id`, `tc_name`, `tc_args` as local variables). Only one `ToolCallDone` is ever emitted per stream.

**Target:** Parse and execute multiple tool calls from a single assistant response.

#### StreamEvent changes

```rust
pub enum StreamEvent {
    Delta(String),
    ToolCallStart { index: usize, id: String, name: String },
    ToolCallDelta { index: usize, arguments: String },
    ToolCallDone(Vec<ToolCall>),   // Changed: Vec instead of single
    Usage(TokenUsage),
    Done,
    Error(String),
}
```

#### OpenAI parser: track multiple tool calls

```rust
// Replace the three local variables with a map:
let mut tool_calls: HashMap<usize, (String, String, String)> = HashMap::new();  // index -> (id, name, args)

// On delta:
for tc in tool_calls_delta {
    let index = tc.index.unwrap_or(0);
    let entry = tool_calls.entry(index).or_insert_with(|| (String::new(), String::new(), String::new()));
    if let Some(id) = &tc.id { entry.0 = id.clone(); }
    if let Some(func) = &tc.function {
        if let Some(name) = &func.name { entry.1 = name.clone(); }
        if let Some(args) = &func.arguments { entry.2.push_str(args); }
    }
}

// On finish_reason: "tool_calls":
let calls: Vec<ToolCall> = tool_calls.into_values()
    .map(|(id, name, args)| ToolCall { id, name, arguments: args })
    .collect();
let _ = tx.send(Ok(StreamEvent::ToolCallDone(calls)));
```

#### Agent loop: parallel execution

```rust
async fn execute_tool_calls(&self, calls: Vec<ToolCall>) -> Vec<ToolResult> {
    let mut handles = Vec::new();

    for call in &calls {
        let tool = self.tool_registry.get_arc(&call.name).unwrap();
        let args = call.arguments.clone();
        let call_id = call.id.clone();
        let ctx = self.make_tool_context();

        handles.push(tokio::spawn(async move {
            match tool.execute(&args, &ctx).await {
                Ok(mut r) => { r.call_id = call_id; r }
                Err(e) => ToolResult {
                    call_id,
                    output: format!("Tool error: {}", e),
                    success: false,
                },
            }
        }));
    }

    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }
    results
}
```

Enable `parallel_tool_calls: true` by default for OpenAI provider, with a config toggle.

---

### 2.3 Token Budget Management

**Problem:** The system uses fixed message-count windowing (30 for initial, 20 for continuations). The system prompt + project context + 30 messages can easily exceed a model's context window. There is no pre-request token estimation.

**Target:** Token-count-aware windowing. Set a budget per request (e.g., 80% of the model's context window). Smart pruning keeps system prompt + recent turns, summarizes old ones.

#### Token estimator

We use a simple character-based estimator (4 chars per token for English, 2 chars per token for CJK) rather than pulling in a full tiktoken dependency. This is accurate enough for budgeting purposes.

```rust
// crates/atomcode-core/src/token.rs

pub struct TokenBudget {
    /// Maximum context window for the current model.
    pub context_window: usize,
    /// Budget for the request (context_window * budget_ratio).
    pub request_budget: usize,
    /// Fraction of context window to use (default 0.80).
    pub budget_ratio: f64,
}

impl TokenBudget {
    pub fn for_model(model: &str) -> Self {
        let context_window = estimate_context_window(model);
        Self {
            context_window,
            request_budget: (context_window as f64 * 0.80) as usize,
            budget_ratio: 0.80,
        }
    }
}

/// Estimate token count for a string.
/// Uses a heuristic: ~4 chars/token for Latin, ~2 chars/token for CJK.
pub fn estimate_tokens(text: &str) -> usize {
    let mut count = 0;
    for ch in text.chars() {
        if ch.is_ascii() {
            count += 1;  // roughly 4 ASCII chars = 1 token, but we count chars
        } else {
            count += 2;  // CJK chars are ~2 tokens each
        }
    }
    // Convert char count to token estimate
    count / 3 + 1
}

/// Known model context windows.
fn estimate_context_window(model: &str) -> usize {
    match model {
        m if m.contains("claude-3") || m.contains("claude-sonnet") || m.contains("claude-opus") => 200_000,
        m if m.contains("gpt-4o") => 128_000,
        m if m.contains("gpt-4-turbo") => 128_000,
        m if m.contains("gpt-4") => 8_192,
        m if m.contains("deepseek") => 64_000,
        m if m.contains("qwen") => 32_000,
        _ => 8_192,  // conservative default
    }
}
```

#### Token-aware message windowing

Replace `to_provider_messages_windowed(system_prompt, window_count)` with:

```rust
pub fn to_provider_messages_budgeted(
    &self,
    system_prompt: &str,
    budget: &TokenBudget,
) -> Vec<Message> {
    let system_tokens = estimate_tokens(system_prompt);
    let mut remaining = budget.request_budget.saturating_sub(system_tokens);

    // Walk backwards from the most recent message, accumulating tokens
    let mut included = Vec::new();
    for msg in self.messages.iter().rev() {
        let msg_tokens = msg.estimate_tokens();
        if msg_tokens > remaining {
            break;
        }
        remaining -= msg_tokens;
        included.push(msg.clone());
    }
    included.reverse();

    // Ensure valid boundary (no orphan ToolResults at the start)
    self.fix_message_boundary(&mut included);

    let mut result = Vec::with_capacity(included.len() + 1);
    result.push(Message::new(Role::System, system_prompt));
    result.extend(included);
    result
}
```

---

## Part 3: Context & Memory

### 3.1 Smart Context Summarization

**Problem:** When the conversation exceeds the window, old messages are simply dropped. There is no summarization. The LLM loses track of decisions made earlier in the conversation.

**Target:** When old turns must be evicted, replace them with a compact summary.

#### Summarization strategy

```rust
pub struct ConversationSummarizer {
    /// Summary of evicted turns.
    pub summary: Option<String>,
    /// Number of messages that have been summarized.
    pub summarized_count: usize,
}

impl ConversationSummarizer {
    /// Generate a summary of messages that are about to be evicted.
    pub fn summarize_evicted(&mut self, messages: &[Message]) -> String {
        let mut summary_parts = Vec::new();

        for msg in messages {
            match (&msg.role, &msg.content) {
                (Role::User, MessageContent::Text(s)) => {
                    summary_parts.push(format!("User asked: {}", truncate(s, 100)));
                }
                (Role::Assistant, MessageContent::Text(s)) => {
                    summary_parts.push(format!("Assistant: {}", truncate(s, 200)));
                }
                (Role::Assistant, MessageContent::AssistantWithToolCalls { text, tool_calls }) => {
                    let tools: Vec<String> = tool_calls.iter()
                        .map(|tc| tc.name.clone())
                        .collect();
                    summary_parts.push(format!(
                        "Assistant called tools: [{}]{}",
                        tools.join(", "),
                        text.as_ref().map(|t| format!(" — {}", truncate(t, 100))).unwrap_or_default()
                    ));
                }
                (Role::Tool, MessageContent::ToolResult(r)) => {
                    let status = if r.success { "OK" } else { "FAILED" };
                    summary_parts.push(format!(
                        "Tool result ({}): {}",
                        status,
                        truncate(&r.output, 80)
                    ));
                }
                _ => {}
            }
        }

        self.summarized_count += messages.len();
        let new_summary = summary_parts.join("\n");

        match &self.summary {
            Some(existing) => {
                self.summary = Some(format!("{}\n---\n{}", existing, new_summary));
            }
            None => {
                self.summary = Some(new_summary);
            }
        }

        self.summary.clone().unwrap()
    }
}
```

The summary is injected as the first user message after the system prompt:

```
[System] You are AtomCode...
[User] <CONVERSATION SUMMARY>
The following is a summary of the earlier conversation:
...
</CONVERSATION SUMMARY>
[User] (most recent actual message)
[Assistant] ...
```

#### LLM-powered summarization (Phase 2)

In a later phase, instead of the rule-based summarizer above, send the old turns to the LLM with a summarization prompt. This produces higher-quality summaries but costs API tokens.

---

### 3.2 Project Context — on-demand, not upfront

**Problem:** `project_context.rs` builds a 6000-char context string injected into every system prompt. This includes a 2-level file tree and truncated descriptor files. For large projects, this wastes context window. For small projects, it is redundant (the LLM can just ask). Claude Code does not do this — it gives the LLM tools to explore on demand.

**Target:** Remove upfront project context injection. Add a `list_directory` tool. The system prompt mentions the working directory but does not include the file tree.

#### Simplified system prompt

```
You are AtomCode, an AI coding agent in the terminal.

Working directory: /path/to/project

You have tools to explore the project:
- list_directory: list files in a directory
- read_file: read file contents
- grep_search: search for patterns across files
- bash: run shell commands
- write_file, edit_file: modify files

Explore the project as needed using your tools. Do not guess file contents — read them.
```

This is approximately 100 tokens instead of 1500+ tokens for the current system prompt with embedded project context.

#### New tool: ListDirectoryTool

```rust
pub struct ListDirectoryTool;

impl Tool for ListDirectoryTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "list_directory",
            description: "List files and directories at a given path. Returns names with type indicators (/ for dirs). Use to explore project structure.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path to list. Defaults to working directory."
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Recursion depth (default 1, max 3)"
                    }
                }
            }),
        }
    }
    // ...
}
```

#### Migration consideration

The upfront project context is actually useful for the first turn — the LLM does not need to waste a tool call to see the project structure. A pragmatic compromise:

- **First turn:** Include a lightweight project context (file tree only, no descriptor file contents) as a user message, not in the system prompt.
- **Subsequent turns:** The LLM uses `list_directory` on demand.

This gives the LLM a head start without permanently wasting system prompt space.

---

## Part 4: Tool System Enhancements

### 4.1 Tool Plugin Architecture

**Problem:** Tools are hardcoded in `main.rs`. Adding a new tool requires modifying source code.

**Target:** A tool trait with lifecycle hooks, supporting both built-in and external tool registration.

#### Enhanced Tool trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    /// Tool metadata.
    fn definition(&self) -> ToolDef;

    /// Check if this invocation needs user approval.
    fn approval(&self, args: &str) -> ApprovalRequirement;

    /// Execute the tool.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult>;

    /// Called when the tool is registered. Use for initialization.
    fn on_register(&self, _ctx: &ToolContext) {}

    /// Called when the tool is unregistered. Use for cleanup.
    fn on_unregister(&self) {}

    /// Default permission level for this tool.
    fn default_permission(&self) -> PermissionLevel {
        PermissionLevel::ToolDefault
    }
}
```

#### Tool manifest for external tools

An external tool is a separate binary or script that implements a simple JSON protocol over stdin/stdout. The manifest declares the tool:

```toml
# ~/.atomcode/tools/my_tool.toml
[tool]
name = "my_custom_tool"
description = "Does something custom"
command = "/path/to/my_tool_binary"
permission = "ask"

[tool.parameters]
type = "object"
properties.input = { type = "string", description = "Input to process" }
required = ["input"]
```

A `ExternalTool` wrapper in atomcode-core executes the binary, passes arguments as JSON on stdin, and reads the result from stdout:

```rust
pub struct ExternalTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    command: String,
    permission: PermissionLevel,
}

#[async_trait]
impl Tool for ExternalTool {
    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let working_dir = ctx.working_dir.read().await.clone();
        let output = Command::new(&self.command)
            .current_dir(&working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        // write args to stdin, read result from stdout
        // ...
    }
}
```

Tool loading at startup:

```rust
// In main.rs or AgentLoop::new()
fn load_external_tools(registry: &mut ToolRegistry) -> Result<()> {
    let tools_dir = Config::config_dir().join("tools");
    if tools_dir.exists() {
        for entry in std::fs::read_dir(&tools_dir)? {
            let path = entry?.path();
            if path.extension().map_or(false, |e| e == "toml") {
                let manifest: ToolManifest = toml::from_str(&std::fs::read_to_string(&path)?)?;
                registry.register(Box::new(ExternalTool::from_manifest(manifest)));
            }
        }
    }
    Ok(())
}
```

---

### 4.2 New Essential Tools

These tools bring AtomCode to parity with Claude Code's built-in capabilities.

#### 4.2.1 GrepSearchTool

```rust
pub struct GrepSearchTool;

impl Tool for GrepSearchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "grep_search",
            description: "Search for a regex pattern across files in a directory. Returns matching lines with file paths and line numbers. Powered by ripgrep-style matching.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex pattern to search for" },
                    "path": { "type": "string", "description": "Directory or file to search (default: working dir)" },
                    "glob": { "type": "string", "description": "File glob filter, e.g. '*.rs'" },
                    "case_insensitive": { "type": "boolean", "description": "Case insensitive search" },
                    "max_results": { "type": "integer", "description": "Max matches to return (default 50)" }
                },
                "required": ["pattern"]
            }),
        }
    }
    // Implementation uses `grep` or `rg` subprocess
}
```

#### 4.2.2 GlobTool

```rust
pub struct GlobTool;

impl Tool for GlobTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "glob",
            description: "Find files matching a glob pattern. Returns matching file paths.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern, e.g. '**/*.rs' or 'src/**/*.ts'" },
                    "path": { "type": "string", "description": "Base directory (default: working dir)" }
                },
                "required": ["pattern"]
            }),
        }
    }
}
```

#### 4.2.3 ListDirectoryTool

(Described in section 3.2 above.)

---

## Part 5: Target File Structure

```
atomcode/
  Cargo.toml                          workspace root

  crates/
    atomcode-core/
      src/
        lib.rs
        agent/
          mod.rs                       AgentLoop struct, AgentCommand, AgentEvent
          turn.rs                      agent_turn() loop logic
          stream_handler.rs            drain_stream() with CancellationToken
        config/
          mod.rs                       Config, load/save
          provider.rs                  ProviderConfig (+ max_tokens)
        conversation/
          mod.rs                       Conversation struct
          message.rs                   Message, Role, MessageContent
          summarizer.rs                ConversationSummarizer (NEW)
        permission/
          mod.rs                       PermissionStore, PermissionLevel, PermissionDecision (NEW)
        provider/
          mod.rs                       LlmProvider trait, create_provider()
          openai.rs                    OpenAI provider (multi-tool-call support)
          claude.rs                    Claude provider (native tool_use) (REWRITTEN)
          ollama.rs                    Ollama provider (function calling added)
        stream/
          mod.rs                       StreamEvent (updated for multi-tool)
        token/
          mod.rs                       TokenBudget, estimate_tokens() (NEW)
        tool/
          mod.rs                       Tool trait (updated), ToolRegistry (updated)
          context.rs                   ToolContext (NEW)
          bash.rs                      BashTool (uses ToolContext)
          cd.rs                        CdTool (writes to ToolContext)
          read.rs                      ReadFileTool
          write.rs                     WriteFileTool
          edit.rs                      EditFileTool
          list_dir.rs                  ListDirectoryTool (NEW)
          grep.rs                      GrepSearchTool (NEW)
          glob.rs                      GlobTool (NEW)
          external.rs                  ExternalTool wrapper (NEW)

    atomcode-tui/
      src/
        lib.rs                         run() entry point
        app.rs                         App (UI-only state, ~400 lines target)
        event.rs                       EventLoop, AppEvent
        command.rs                     SlashMenu, slash command definitions
        file_attach.rs                 File attachment logic
        provider_manager.rs            Provider CRUD wizard
        ui/
          mod.rs
          chat_panel.rs
          input_box.rs
          status_bar.rs
          welcome.rs
          markdown.rs
          model_selector.rs
          provider_panel.rs
          slash_menu.rs
          approval_prompt.rs           Expanded approval UI (Y/N/A/D) (NEW)

    atomcode-cli/
      src/
        main.rs                        CLI parsing, tool registration, launch
```

Estimated line counts after refactoring:
- `app.rs`: ~400 lines (down from ~1450)
- `agent/mod.rs`: ~300 lines
- `agent/turn.rs`: ~200 lines
- `agent/stream_handler.rs`: ~100 lines
- `claude.rs`: ~250 lines (up from ~180, now with real tool_use)

---

## Part 6: Migration Strategy

### Phase 1: Foundation (can be done incrementally)

**Duration:** 1-2 weeks
**Risk:** Low (additive changes, no breakage)

| Task | Description | Risk |
|------|-------------|------|
| 1a. Add `ToolContext` | Create `tool/context.rs` with `working_dir: Arc<RwLock<PathBuf>>`. Update `Tool::execute()` signature to accept `&ToolContext`. Update all 5 tool implementations. | Low — mechanical signature change |
| 1b. Fix BashTool | Remove `working_dir` field from `BashTool`. Read from `ToolContext` instead. Remove the re-instantiation hack in `execute_tool()`. | Low — direct improvement |
| 1c. Fix CdTool | Make `CdTool::execute()` write to `ToolContext.working_dir`. Remove the interception in `handle_tool_result()`. | Low — self-contained |
| 1d. Add `CancellationToken` | Add `tokio-util` dependency. Thread `CancellationToken` through `ToolContext`. Update `BashTool` to check for cancellation. | Low — additive |
| 1e. Add `PermissionStore` | Create `permission/mod.rs`. Wire it into `handle_tool_call()` alongside the existing `approval()` check. Add config file support. | Low — additive |

### Phase 2: Claude Provider (independent track)

**Duration:** 1 week
**Risk:** Medium (API format changes, needs testing with real API)

| Task | Description | Risk |
|------|-------------|------|
| 2a. Implement Claude tool_use format | Rewrite `format_messages()` and `format_tools()` for Claude's content-block format. | Medium — must match API spec exactly |
| 2b. Implement content block streaming | Parse `content_block_start`, `content_block_delta`, `content_block_stop` events. Track text vs tool_use blocks. Accumulate `input_json_delta`. | Medium — stateful parsing |
| 2c. Add token usage reporting | Parse `message_start` and `message_delta` for `usage` field. Emit `StreamEvent::Usage`. | Low |
| 2d. Make `max_tokens` configurable | Add field to `ProviderConfig`. Default by model. | Low |

### Phase 3: Multi-tool-call support (independent track)

**Duration:** 3-4 days
**Risk:** Medium

| Task | Description | Risk |
|------|-------------|------|
| 3a. Update `StreamEvent::ToolCallDone` | Change from single `ToolCall` to `Vec<ToolCall>`. Update all consumers. | Low — type change |
| 3b. Update OpenAI parser | Track multiple tool calls by index. Emit all on `finish_reason: "tool_calls"`. | Medium — must handle edge cases |
| 3c. Parallel execution | In `handle_tool_call()` (or `agent_turn()`), spawn multiple tool executions concurrently. Collect results. Add all to conversation. | Medium — ordering matters |

### Phase 4: Extract AgentLoop (the big one)

**Duration:** 2-3 weeks
**Risk:** High (major structural change)

This is the only phase that requires a "big rewrite" approach for the affected file. The strategy:

| Step | Description | Risk |
|------|-------------|------|
| 4a. Define `AgentCommand` and `AgentEvent` | Create the channel protocol types in `agent/mod.rs`. | Low — new types |
| 4b. Implement `AgentLoop::run()` | Move `send_message()`, `handle_tool_call()`, `execute_tool()`, `handle_tool_result()`, `continue_agent_loop()` out of `App` and into `AgentLoop`. Adapt to use channels instead of direct state mutation. | High — the bulk of the work |
| 4c. Slim down `App` | Remove agent-related fields and methods from `App`. Replace with `agent_tx` / `agent_rx`. Update `handle_event()` to send commands and process events. | High — touches every method in app.rs |
| 4d. Update `lib.rs` main loop | Spawn `AgentLoop` as a separate tokio task. Merge its event channel into the main event loop (using `tokio::select!`). | Medium |
| 4e. Integration testing | Test the full flow: user message -> agent loop -> tool calls -> completion. Test cancellation. Test provider switching. | Critical |

**Strategy for 4b/4c:** Do NOT attempt to incrementally refactor `app.rs` method by method. Instead:
1. Write `AgentLoop` from scratch in `agent/mod.rs`, implementing the same logic but with the new architecture.
2. Write a thin adapter in `App` that bridges `AppEvent` to `AgentCommand` and `AgentEvent` to UI updates.
3. Delete the agent-related methods from `App` all at once.
4. Fix compilation errors.

This is a "strangler fig" pattern — build the new system alongside the old, then switch over.

### Phase 5: Context improvements (after Phase 4)

**Duration:** 1 week
**Risk:** Low-Medium

| Task | Description | Risk |
|------|-------------|------|
| 5a. Token budget management | Implement `TokenBudget` and `estimate_tokens()`. Replace `to_provider_messages_windowed()` with `to_provider_messages_budgeted()`. | Low |
| 5b. Context summarization | Implement `ConversationSummarizer`. Inject summary when old turns are evicted. | Medium — affects conversation quality |
| 5c. On-demand project context | Add `ListDirectoryTool`, `GrepSearchTool`, `GlobTool`. Simplify system prompt. Keep lightweight first-turn context. | Low |

### Phase 6: Polish (after Phase 5)

**Duration:** Ongoing
**Risk:** Low

| Task | Description |
|------|-------------|
| 6a. External tool plugin system | Implement `ExternalTool`, tool manifests, tool directory scanning |
| 6b. Ollama function calling | Implement Ollama's tool calling API |
| 6c. Configurable constants | Move all hardcoded constants to config: timeouts, line limits, char limits, etc. |
| 6d. Bounded channels | Replace `UnboundedChannel` with bounded channels + backpressure |
| 6e. reqwest::Client reuse | Share a single `Client` across provider rebuilds |
| 6f. History schema versioning | Add a version field to the history JSON format |

---

## Dependency Graph Between Phases

```
Phase 1 (Foundation) ──────────────────────────┐
    |                                           |
    v                                           v
Phase 2 (Claude Provider)    Phase 3 (Multi-tool)
    |                             |
    +──────────+──────────────────+
               |
               v
         Phase 4 (Extract AgentLoop)
               |
               v
         Phase 5 (Context Improvements)
               |
               v
         Phase 6 (Polish)
```

Phases 1, 2, and 3 can all proceed in parallel. Phase 4 depends on Phase 1 (ToolContext and CancellationToken must exist first). Phase 5 depends on Phase 4 (token budgeting lives in AgentLoop). Phase 6 is independent cleanup.

---

## Success Criteria

After all phases are complete, the following must be true:

1. `AgentLoop` can be instantiated and driven without any TUI code (headless mode).
2. All three providers (OpenAI, Claude, Ollama) support function calling and participate in the agent loop.
3. Ctrl+C immediately cancels all in-flight tasks (HTTP streams, child processes).
4. Multiple tool calls from a single LLM response are executed in parallel.
5. The conversation never exceeds the model's context window (token-budget enforcement).
6. Old conversation turns are summarized, not silently dropped.
7. `app.rs` is under 500 lines.
8. Adding a new tool requires zero changes to `app.rs` or `main.rs` (just register in the tool directory or programmatically).
9. All tools go through `ToolRegistry.execute()` uniformly — no special-casing.
10. The permission system supports per-tool configuration and session-level grants.
