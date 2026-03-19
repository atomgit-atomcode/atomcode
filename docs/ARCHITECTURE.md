# AtomCode Technical Architecture Document

**Version:** 0.1.0 (pre-release)
**Date:** 2026-03-19
**Purpose:** Comprehensive architecture reference for external review

---

## 1. Project Overview

AtomCode is a terminal-based AI coding agent written in Rust. It is conceptually similar to Claude Code and Cursor -- a conversational AI assistant that can read/write files, run shell commands, and edit code autonomously within a terminal UI. The user interacts via a TUI (ratatui-based), sends natural language instructions, and the agent executes multi-step plans by calling LLM APIs and local tools in a loop.

**Core design goals (from CLAUDE.md):**

- Tech-stack agnostic: never hardcode language-specific logic; dynamically detect project environments
- System-level performance: fast startup, low latency, minimal memory
- Clean separation of LLM Provider, Tool Registry, and Agent Loop
- Tool calling safety: destructive commands require explicit user approval
- Graceful error handling: tool failures become LLM observations, never panics
- Context-aware token management: windowed conversation, file truncation, project context capping

**What it does today:**

- Streams LLM responses with real-time markdown rendering and syntax highlighting
- Executes an autonomous agent loop: LLM requests tool calls, tools execute, results feed back to the LLM
- Supports three provider backends: OpenAI-compatible (with full function calling), Claude (Anthropic), Ollama (local)
- Provides five built-in tools: bash, read_file, write_file, edit_file, change_dir
- Persists conversation history, supports file attachments, slash commands, input history

---

## 2. Crate Structure

```
atomcode/                         (workspace root)
  Cargo.toml                      workspace definition (resolver = "2")

  crates/
    atomcode-core/                headless library -- no TUI dependency
      config/                     Config loading/saving, ProviderConfig
      conversation/               Conversation state, Message/Role types
      stream/                     StreamEvent enum (Delta, ToolCall*, Usage, Done, Error)
      provider/                   LlmProvider trait + OpenAI, Claude, Ollama impls
      tool/                       Tool trait, ToolRegistry, 5 tool implementations

    atomcode-tui/                 TUI layer -- ratatui + crossterm
      app.rs                      App state machine (1450 lines, the heart of the application)
      event.rs                    EventLoop: crossterm -> AppEvent channel
      command.rs                  Slash command definitions + autocomplete menu state
      project_context.rs          Project tree scanner, descriptor file reader
      provider_manager.rs         Multi-step provider CRUD wizard
      file_attach.rs              File path detection + content extraction (PDF, Excel, etc.)
      ui/                         Render functions (chat_panel, input_box, status_bar, etc.)

    atomcode-cli/                 Binary entry point
      main.rs                     CLI arg parsing, first-run wizard, tool registration, launch
```

### Dependency Graph

```
atomcode-cli
  --> atomcode-core   (config, provider, tool, conversation, stream)
  --> atomcode-tui    (app, event, ui, project_context, etc.)
        --> atomcode-core
```

`atomcode-core` is fully independent (no TUI deps). `atomcode-tui` depends on `atomcode-core` for types and traits. `atomcode-cli` wires everything together and is the sole binary target.

### Key External Dependencies

| Crate | Purpose |
|-------|---------|
| `reqwest` (0.12, stream+json) | HTTP client for all LLM API calls |
| `tokio` (full) | Async runtime, process spawning, timers |
| `ratatui` (0.29) + `crossterm` (0.28) | Terminal UI rendering + input capture |
| `pulldown-cmark` (0.12) | Markdown parsing for response rendering |
| `syntect` (5) | Syntax highlighting in code blocks |
| `serde` / `serde_json` / `toml` | Serialization for config, messages, tool args |
| `clap` (4) | CLI argument parsing |

---

## 3. Data Flow Diagram

```
User Input (keyboard)
      |
      v
+-------------+    crossterm events     +------------+
|  EventLoop  | ----------------------> |  AppEvent  |
| (2 tasks:   |    AppEvent::Key(...)   |  channel   |
|  key reader |                         | (unbounded |
|  + 250ms    |                         |  mpsc)     |
|    tick)    |                         +-----+------+
+-------------+                               |
                                              v
                                    +---------+---------+
                                    |     App::handle   |
                                    |     _event()      |
                                    |                   |
                                    | (state machine    |
                                    |  dispatch by      |
                                    |  AppMode)         |
                                    +---------+---------+
                                              |
                           +------------------+------------------+
                           |                                     |
                    User sends message                    Stream/Tool events
                           |                                     |
                           v                                     v
                  +--------+--------+               +------------+----------+
                  | send_message()  |               | handle_tool_result()  |
                  |                 |               | continue_agent_loop() |
                  | 1. Build system |               |                       |
                  |    prompt       |               | Adds ToolResult to    |
                  | 2. Window msgs  |               | conversation, then    |
                  |    (last 30)    |               | calls provider again  |
                  | 3. Get tool     |               +-----------+-----------+
                  |    definitions  |                           |
                  | 4. Call         |                           |
                  |    provider     |                           |
                  |    .chat_stream |                           |
                  +--------+--------+                          |
                           |                                   |
                           v                                   v
                  +--------+--------+               +----------+---------+
                  | spawn_stream_   |               | provider.chat_     |
                  | handler()       |               | stream()           |
                  |                 |               | (same path)        |
                  | tokio::spawn    |               +--------------------+
                  | drains stream   |
                  | sends AppEvents |
                  +--------+--------+
                           |
              +------------+------------+
              |            |            |
              v            v            v
         StreamDelta  ToolCallDone  StreamDone
              |            |            |
              v            v            v
         push_delta   handle_tool   finalize
         (buffer)     _call()       _stream()
                           |
                    +------+------+
                    |             |
                 AutoApprove  RequireApproval
                    |             |
                    v             v
              execute_tool   WaitingApproval
              (tokio::spawn)  (Y/N prompt)
                    |
                    v
              ToolFinished(result) --> handle_tool_result() --> loop
```

---

## 4. Core Abstractions

### 4.1 LlmProvider Trait

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn chat_stream(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDef]>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent>> + Send>>>;

    fn model_name(&self) -> &str;
}
```

Returns a `Pin<Box<dyn Stream<Item = Result<StreamEvent>>>>`. The stream is consumed by `spawn_stream_handler()` which maps `StreamEvent` variants to `AppEvent` variants and sends them through the unbounded mpsc channel.

Factory function `create_provider(config) -> Box<dyn LlmProvider>` dispatches on `provider_type` string: `"claude"`, `"openai"`, `"ollama"`.

### 4.2 Tool Trait

```rust
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDef;
    fn approval(&self, args: &str) -> ApprovalRequirement;
    async fn execute(&self, args: &str) -> Result<ToolResult>;
}
```

- `definition()` returns a `ToolDef` with name, description, and JSON Schema parameters -- fed directly to the LLM's function calling API.
- `approval()` inspects args to decide if user confirmation is needed (e.g., `rm -rf` in bash).
- `execute()` performs the operation and returns `ToolResult { call_id, output, success }`.

### 4.3 ToolRegistry

A `HashMap<String, Arc<dyn Tool>>`. Tools are registered at startup in `main.rs`. Lookup by name, returns `Arc` for cross-thread dispatch.

### 4.4 Message Types

```rust
enum Role { System, User, Assistant, Tool }

enum MessageContent {
    Text(String),
    AssistantWithToolCalls { text: Option<String>, tool_calls: Vec<ToolCall> },
    ToolResult(ToolResult),
}

struct Message { role: Role, content: MessageContent }
```

This is the canonical internal message format. Each provider translates it into its own wire format in `format_messages()`. The `ToolResult` variant maps to OpenAI's `role: "tool"` messages.

### 4.5 StreamEvent

```rust
enum StreamEvent {
    Delta(String),                          // text chunk
    ToolCallStart { id: String, name: String }, // function call begins
    ToolCallDelta(String),                  // argument fragment
    ToolCallDone(ToolCall),                 // complete tool call
    Usage(TokenUsage),                      // prompt + completion counts
    Done,                                   // stream finished
    Error(String),                          // error during streaming
}
```

This is the provider-agnostic streaming protocol. All three providers emit these events. The TUI layer never touches provider-specific types.

---

## 5. Agent Loop (State Machine)

### 5.1 AppMode Enum

```
Normal --> Streaming --> ToolExecuting --> Streaming --> ... --> Normal
                    \--> WaitingApproval --> ToolExecuting (if approved)
                                        \--> Normal (if denied, feeds denial to LLM)

Normal --> ModelSelector --> Normal
Normal --> ProviderManager --> Normal
Normal --> Exiting
```

### 5.2 Mode Transitions in Detail

| From | Event | To | Action |
|------|-------|----|--------|
| Normal | User presses Enter | Streaming | `send_message()`: add user msg, call `provider.chat_stream()` |
| Streaming | StreamDelta | Streaming | Append to `conversation.stream_buffer` |
| Streaming | ToolCallDone | ToolExecuting or WaitingApproval | `handle_tool_call()`: check approval, dispatch |
| Streaming | StreamDone | Normal | `finalize_stream()`, maybe auto-summary, persist history |
| Streaming | StreamError (network) | Streaming | Retry with exponential backoff (no limit) |
| Streaming | StreamError (API 4xx) | Normal | Show error, no retry |
| WaitingApproval | Y/Enter | ToolExecuting | `execute_tool()` |
| WaitingApproval | N/Esc | Streaming | Feed "Denied by user" as ToolResult, continue loop |
| ToolExecuting | ToolFinished | Streaming | `handle_tool_result()` -> `continue_agent_loop()` |
| Any | Ctrl+C (1st) | Normal | Cancel current operation |
| Any | Ctrl+C (2nd within 1s) | Exiting | Exit program |
| Any | Esc (during streaming) | Normal | Cancel stream |

### 5.3 Agent Loop Execution

The agent loop is implicit, not a formal loop construct. After each `ToolResult`, `handle_tool_result()` calls `continue_agent_loop()`, which calls `provider.chat_stream()` again. The LLM sees the tool result and either:
- Emits another `ToolCallDone` (loop continues)
- Emits text + `StreamDone` (loop ends, mode -> Normal)

There is no hard limit on iterations. The tool call counter resets every 25 calls to prevent the old limit from blocking long workflows, effectively making it unbounded.

---

## 6. Tool System

### 6.1 Registered Tools

| Tool | Name (API) | Approval | Key Behavior |
|------|-----------|----------|--------------|
| `BashTool` | `bash` | Checks destructive patterns (rm -rf, git push --force, etc.) | 30s default timeout, early return for long-running processes, captures stdout+stderr |
| `ReadFileTool` | `read_file` | Always auto-approve | Line-numbered output, 2000-line default limit, binary file detection |
| `WriteFileTool` | `write_file` | Checks sensitive paths (/etc, ~/.ssh, shell configs) | Creates parent dirs, reports byte count |
| `EditFileTool` | `edit_file` | Always auto-approve | Exact string match replacement, requires unique match (fails if 0 or >1 occurrences) |
| `CdTool` | `change_dir` | Always auto-approve | Validates path, resolves ~. Note: does NOT actually change process cwd |

### 6.2 Tool Execution Flow

1. LLM emits `ToolCallDone(call)` via stream
2. `handle_tool_call()` looks up tool in registry
3. Calls `tool.approval(args)` -- if `RequireApproval`, enters `WaitingApproval` mode
4. `execute_tool()` spawns a tokio task with the tool's `execute()` method
5. On completion, `ToolFinished(result)` event is sent back through the channel
6. `handle_tool_result()` adds the result to conversation and calls `continue_agent_loop()`

### 6.3 Tool Argument Resolution

`resolve_tool_args()` intercepts `file_path` arguments and converts relative paths to absolute paths based on `working_dir`. This happens before tool execution.

### 6.4 Output Truncation

Tool outputs exceeding 8000 characters are truncated with a `[truncated...]` message. This prevents oversized payloads in subsequent LLM API calls.

### 6.5 BashTool Special Handling

BashTool is stateful -- it needs the current `working_dir`. Unlike other tools (which are registered once), BashTool is re-instantiated with the current `working_dir` on every execution (`execute_tool()` creates `Arc::new(BashTool::new(self.working_dir.clone()))`). Other tools are dispatched via `tool_registry.get_arc()`.

### 6.6 CdTool Coordination

CdTool execution is intercepted in `handle_tool_result()`. When `change_dir` succeeds, the app's `working_dir` is updated, `project_context_cache` is invalidated, and the config's `default_workdir` is saved. The tool itself does NOT call `std::env::set_current_dir()` -- it only validates the path and reports success/failure.

---

## 7. LLM Provider System

### 7.1 Provider Implementations

#### OpenAI Provider (fully featured)

- Endpoint: `{base_url}/chat/completions` (auto-normalized)
- Supports function calling via `tools` parameter
- Parses SSE `data: ...` lines, handles `[DONE]` sentinel
- Tracks tool call state across streaming chunks (id, name, accumulated arguments)
- Emits `StreamEvent::ToolCallDone` on `finish_reason: "tool_calls"`
- Reports token usage from the final chunk
- Sets `parallel_tool_calls: false` (sequential tool execution)
- Supports OpenAI-compatible APIs (Deepseek, Qwen, Zhipu, Moonshot, SiliconFlow, etc.)

#### Claude Provider (text-only, no function calling)

- Endpoint: hardcoded `https://api.anthropic.com/v1/messages`
- Parses Claude SSE format (`content_block_delta`, `message_stop`)
- **Does NOT support function calling**: the `_tools` parameter is ignored (`_tools: Option<&[ToolDef]>`)
- Tool role messages are sent as user messages (workaround)
- `max_tokens` is hardcoded to 4096
- No token usage reporting

#### Ollama Provider (text-only, no function calling)

- Endpoint: `{base_url}/api/chat`
- Parses newline-delimited JSON (not SSE)
- **Does NOT support function calling**: `_tools` is ignored
- No token usage reporting

### 7.2 Streaming Architecture

All three providers follow the same pattern:
1. Build the HTTP request synchronously
2. Spawn a tokio task that:
   - Sends the request
   - Reads the response body as a byte stream
   - Parses the stream format (SSE or NDJSON)
   - Sends `StreamEvent`s through an `UnboundedSender<Result<StreamEvent>>`
3. Return a `Pin<Box<dyn Stream>>` wrapping `UnboundedReceiverStream`

The receiver is consumed by `spawn_stream_handler()` in `app.rs`, which maps `StreamEvent` to `AppEvent` and forwards to the main event channel.

### 7.3 Error Handling and Retry

- API errors (4xx): shown to user, no retry
- Network errors / rate limits (429): automatic retry with exponential backoff
  - Rate limit: 5s * retry_count, capped at 30s
  - Network error: 2s * retry_count, capped at 15s
  - No maximum retry count (infinite retries)

---

## 8. TUI Architecture

### 8.1 Render Pipeline

```
Main Loop (lib.rs):
  loop {
    terminal.draw(|frame| ui::render(frame, &mut app));
    event_loop.next().await  // blocks until event
    app.handle_event(event, &event_tx);
    // drain queued events (non-blocking)
  }
```

`ui::render()` dispatches based on app mode:
- `ProviderManager` -> full-screen provider panel + status bar
- `Normal` with empty conversation -> welcome screen
- Otherwise -> status bar + chat panel + input box + overlays (slash menu, model selector)

### 8.2 Layout

```
+--------------------------------------------------+
| Status Bar (1 line)                               |
| [AtomCode] [mode] | ~/project | 12s | 1.2k tokens| model |
+--------------------------------------------------+
| Chat Panel (flex)                                 |
|   > User message                                  |
|   | Assistant response (markdown rendered)        |
|   | > Tool Call: bash command=...                  |
|   | + success output                              |
|   | Thinking... (spinner)  12s | 500 tokens       |
+--------------------------------------------------+
| [file tags]                                       |
| +----------------------------------------------+ |
| | > Input box (multi-line, auto-grow)          | |
| +----------------------------------------------+ |
+--------------------------------------------------+
```

### 8.3 Render Caching Strategy

The chat panel uses a two-tier rendering approach:

1. **Cached lines** (`render_cache: Vec<Line<'static>>`): All completed messages are rendered to `Line` objects once and cached. The cache is invalidated only when `conversation.messages.len()` changes (tracked by `render_cache_msg_count`).

2. **Dynamic lines**: Streaming buffer, spinner, tool execution indicator, and approval prompts are rendered fresh every frame. These are appended after the cached lines.

3. **Visible-slice optimization**: Only the lines visible in the viewport (plus 5 lines padding) are cloned into the `Paragraph` widget. This avoids cloning the entire conversation on every frame.

### 8.4 Markdown Rendering

`markdown.rs` uses `pulldown-cmark` for parsing and `syntect` for code syntax highlighting. Features:
- Headings (H1/H2/H3) with distinct colors and H1 underline
- Bold, italic, inline code, links (with URL display)
- Code blocks with language labels, line numbers, and syntax-highlighted content
- Ordered and unordered lists with nesting
- Block quotes with vertical bar
- Tables with auto-calculated column widths
- Horizontal rules
- Emoji stripping (all Unicode emoji ranges removed)

The `MarkdownRenderer` is initialized once via `OnceLock` (loads syntax definitions + theme). The theme is `base16-ocean.dark`.

### 8.5 Event Loop

Two background tokio tasks:
1. **Key/mouse reader**: reads crossterm events, maps to `AppEvent`, sends through channel
2. **Tick timer**: sends `AppEvent::Tick` every 250ms (for spinner animation and timer updates)

Mouse scroll (ScrollUp/Down) is captured but click/drag events are ignored (native terminal selection is preserved). Mouse capture is explicitly NOT enabled.

### 8.6 Input Box

Multi-line input with:
- Shift+Enter for newline, Enter to send
- Unicode-aware cursor positioning (CJK wide char support)
- Auto-growing height (up to half the terminal height)
- Input history (Up/Down navigation when at first/last line)
- Ghost text suggestions (Tab to accept)
- Slash command autocomplete menu (floating popup)
- File path detection and auto-attachment

---

## 9. Project Context System

`project_context.rs` builds a context string injected into the system prompt. It is **language-agnostic** by design.

### What it collects:
1. **File tree** (2 levels deep, skipping common noise dirs: node_modules, .git, target, etc.)
2. **Descriptor files** (raw content, each with a max line cap):
   - README.md (40 lines), Makefile (50), package.json (30), Cargo.toml (20), pyproject.toml (30), go.mod (5), Gemfile (15), docker-compose.yml (30), justfile (50), Taskfile.yml (40), .env.example (10)
3. **Executable files** at project root (.sh files, Dockerfile, Procfile, or files with execute permission)

### Constraints:
- Total context capped at 6000 characters
- Individual descriptor files capped at 2000 characters
- Context is cached in `App.project_context_cache` and invalidated only on `/cd`

### System Prompt Structure:
```
You are AtomCode, a terminal coding agent.

Working directory: /path/to/project

[project context: tree + descriptor files]

---
RULES (follow strictly):
[system prompt rules]
```

Rules are placed last (recency effect for LLM attention).

---

## 10. Conversation Management

### 10.1 Message Storage

`Conversation` holds a `Vec<Message>` with a hard cap of `MAX_MESSAGES = 1000`. When the cap is exceeded, the oldest messages are drained.

### 10.2 Windowed Context for API Calls

`to_provider_messages_windowed(system_prompt, window)` sends only the last N messages to the LLM:
- Initial user message: window = 30
- Agent loop continuations: window = 20

The windowing logic ensures it never starts in the middle of a tool_call/tool_result pair (skips orphan ToolResults, seeks forward to a User message boundary).

### 10.3 Stream Buffer

During streaming, text deltas accumulate in `Conversation.stream_buffer`. On `StreamDone`, `finalize_stream()` commits the buffer as an Assistant message. On `ToolCallDone`, `finalize_stream_with_tool_call()` commits the buffer as an `AssistantWithToolCalls` message.

A `ToolCallBuffer` tracks incremental tool call assembly (id, name, accumulated argument fragments) during streaming.

### 10.4 Persistence

- **History file**: `~/.atomcode/history.json`
- **Save strategy**: Atomic write (write to `.tmp`, rename to final path). Happens asynchronously via `tokio::spawn` on `StreamDone`.
- **Load strategy**: On startup, loads from disk. If JSON is corrupted, backs up to `.bak` and starts fresh. Truncates to last `MAX_MESSAGES`.
- **Clear**: `/clear` command writes `[]` to the history file.

### 10.5 Auto-Summary

When an agent turn ends without the LLM providing a final text summary (only tool calls), `maybe_add_auto_summary()` generates a brief summary from the tool results (status, step count, one-line output per step).

---

## 11. Known Architectural Weaknesses

### 11.1 Critical: Claude and Ollama Providers Lack Function Calling

The Claude and Ollama providers **ignore the tools parameter entirely** (`_tools`). This means the agent loop (tool calling) only works with OpenAI-compatible providers. Claude's native tool_use API and Ollama's function calling support are not implemented. For Claude, tool role messages are incorrectly mapped to user messages. This is a major gap: 2 of 3 providers cannot participate in the agent loop.

### 11.2 Major: app.rs God Object

`app.rs` is ~1450 lines containing the App struct with 30+ fields and all business logic: event handling, state transitions, tool dispatch, history management, slash commands, provider management, clipboard, file attachment, and input handling. This violates the CLAUDE.md principle of clean separation. The struct should be decomposed into focused components.

### 11.3 Major: No Concurrent Tool Execution

`parallel_tool_calls: false` is hardcoded in the OpenAI provider. The tool call buffer and agent loop assume single-tool-at-a-time execution. Multi-tool parallelism (a strength of models like GPT-4) is deliberately disabled.

### 11.4 Major: BashTool Working Directory is Stale

BashTool is registered once at startup with the initial working directory. While `execute_tool()` creates a fresh `BashTool` instance with the current `working_dir`, this pattern is fragile -- if someone calls the tool through the registry directly (e.g., `tool_registry.get("bash")`), they get the stale instance. The special-casing in `execute_tool()` is a code smell.

### 11.5 Moderate: No Token Budget Estimation

While token usage is tracked from API responses, there is no pre-request token estimation. The system prompt + project context + 30 messages could easily exceed model context limits, and the only mitigation is the fixed window size. There is no adaptive windowing based on actual token counts.

### 11.6 Moderate: No Streaming Cancellation Propagation

When the user cancels a stream (Ctrl+C/Esc), the mode is set to Normal and stale events are dropped. But the spawned tokio task continues running until the HTTP stream ends naturally. The underlying HTTP request is not aborted. For long tool executions (`tokio::spawn` in `execute_tool`), the process continues running in the background.

### 11.7 Moderate: Hardcoded Constants

- `max_tokens: 4096` in Claude provider
- `MAX_MESSAGES: 1000` in conversation
- `MAX_OUTPUT_CHARS: 8000` for tool output truncation
- Window sizes `30` and `20` for provider messages
- `6000` char cap for project context
- `50_000` char cap for attached file content
- `2000` line default for read_file

None of these are configurable by the user.

### 11.8 Moderate: Event Channel Backpressure

All event channels are `UnboundedChannel`. There is no backpressure mechanism. A fast-streaming LLM could theoretically fill memory if the render loop falls behind.

### 11.9 Minor: No Unit Tests for TUI Logic

The `app.rs` file has tests only for `InputState`. The state machine transitions, tool dispatch logic, slash command handling, and provider management are untested. Core crate modules (tool/mod.rs, conversation/mod.rs, config/mod.rs) have reasonable test coverage.

### 11.10 Minor: CdTool Does Not Change Process CWD

`CdTool.execute()` validates the path and returns success/failure, but does NOT call `std::env::set_current_dir()`. The actual working directory update is handled by intercepting the tool result in `handle_tool_result()`. This means `CdTool` is not self-contained -- its semantics depend on the caller.

### 11.11 Minor: Message Serialization for History

Conversation history is serialized with the internal `MessageContent` enum structure. If the enum variants change (e.g., adding a new variant), old history files could become unreadable. There is no schema versioning.

### 11.12 Minor: Config Stores API Keys in Plaintext

`~/.atomcode/config.toml` stores API keys as plain strings with no encryption or keychain integration.

### 11.13 Minor: Duplicate Code in Stream Handler

`spawn_stream_handler()` and `spawn_stream_handler_delayed()` contain identical `StreamEvent -> AppEvent` mapping logic, violating DRY. The delayed version only adds a `tokio::time::sleep` before the identical stream consumption loop.

### 11.14 Minor: No Graceful HTTP Connection Reuse

Each provider creates a new `reqwest::Client` on construction. While `reqwest::Client` internally uses connection pooling, a new client is created on every `rebuild_provider()` call (model switch), discarding the pool.

---

## 12. CLAUDE.md Compliance Status

The CLAUDE.md file defines core development principles. Here is an honest assessment of compliance:

### Principle 1: Tech-Stack Agnostic

| Requirement | Status | Notes |
|-------------|--------|-------|
| No hardcoded language-specific logic | **MET** | Project context scans for generic descriptor files without interpreting them. System prompt instructs the LLM to read them. |
| Dynamic project detection via descriptor files | **MET** | `project_context.rs` scans for Cargo.toml, package.json, pyproject.toml, go.mod, etc. without special handling per type. |
| No special file extension handling in core | **MET** | `file_attach.rs` has extension-to-label mapping for display purposes only (e.g., "Rust", "Python"), but no logic branches. |
| Plugin/adapter model for extensions | **NOT MET** | No plugin system exists. Tools are hardcoded in `main.rs`. Adding a new tool requires code changes. |

### Principle 2: Architecture & Performance

| Requirement | Status | Notes |
|-------------|--------|-------|
| System-level performance | **PARTIALLY MET** | Rust gives baseline perf. Render caching is well-implemented. But no benchmarks, no startup time optimization, and syntect loads all themes/syntaxes eagerly. |
| Clean separation of LLM Provider, Tool Registry, Agent Loop | **PARTIALLY MET** | Provider and Tool are properly abstracted in `atomcode-core`. But the Agent Loop is entangled with TUI state in `app.rs` rather than being a standalone component. A headless agent (without TUI) would require significant refactoring. |

### Principle 3: Tool Calling Safety

| Requirement | Status | Notes |
|-------------|--------|-------|
| Tools as standardized JSON Schema | **MET** | All tools produce `ToolDef` with JSON Schema `parameters`. |
| Safety interception for destructive commands | **MET** | BashTool checks 17 destructive patterns (rm -rf, git push --force, dd, etc.). WriteFileTool checks sensitive paths. Both require user approval. |
| Graceful error feedback as observations | **MET** | Tool execution errors are caught, formatted as `ToolResult { success: false, output: error_msg }`, and fed back to the LLM. The app never crashes on tool failure. |

### Principle 4: Context & Token Management

| Requirement | Status | Notes |
|-------------|--------|-------|
| No bulk file injection | **MET** | `read_file` defaults to 2000 lines. Tool outputs are truncated at 8000 chars. Attached files are capped at 50,000 chars. |
| Sliding window memory | **MET** | `to_provider_messages_windowed()` sends only last 20-30 messages with boundary-safe truncation. |
| Conversation cap | **MET** | MAX_MESSAGES = 1000 with drain on overflow. |
| Summary/compression mechanism | **NOT MET** | No summarization of old messages. The sliding window simply drops them. No outline extraction for large files beyond line limits. |

### Interaction & Output

| Requirement | Status | Notes |
|-------------|--------|-------|
| Color-coded Thought/Action/Response | **PARTIALLY MET** | Tool calls (cyan), results (green/red), assistant text (rendered markdown), and user input (purple accent) are visually distinct. But there is no explicit "Thought" vs "Action" separation -- the model's planning text is shown inline. |
| No emoji | **MET** | `strip_emoji()` in markdown.rs actively removes all emoji from rendered output. System prompt says "No emoji." |

---

*This document was generated from a complete reading of all source files in the AtomCode workspace as of 2026-03-19.*
