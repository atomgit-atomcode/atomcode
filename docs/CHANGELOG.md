# AtomCode Changelog

All notable changes to AtomCode are documented in this file.

---

## v1.0.0 — Production-Ready Agent (Claude Code-Aligned Architecture)

**Major architecture overhaul aligning with Claude Code's design principles.**

### Architecture
- Pre-read ALL source files into system prompt (removed keyword matching)
- Dynamic pre-read ratio: 15% (≤32K context) / 20% (32K-100K) / 30% (>100K)
- System prompt rules moved to END (recency effect for better model compliance)
- File tree depth 2→4 levels, context cap 6K→15K chars
- `context_window` default 64K, configurable per-provider in config.toml
- Removed large file read truncation (300-line threshold) — always return full file
- Removed fuzzy matching (caused destructive silent edits)

### Safety & Correctness
- Edit: large deletion warning (>10 lines net removed)
- Edit: replace_all high count warning (>10 occurrences)
- Edit: surrounding context (10 lines) returned after each successful edit
- Verify: detects failed bash output ("build failed", "error") — no false "Done"
- "ADD DON'T REPLACE" rule: new features added alongside existing code, not replacing
- Specialized edit_file JSON parser for malformed arguments from weak models
- Sibling file check: after editing, prompts to check same-directory files for same bug

### Stability
- **Approval loop fix**: pending_tool_calls not removed in Ask branch (root cause of "press Y loops, press A exits")
- 400 "messages illegal" auto-recovery: trim conversation + retry
- `pkill` excluded from `kill -9` destructive detection
- Background commands (`&`) marked as success (not false failure)
- Bash file-reading commands properly increment consecutive_reads counter
- Auto-attach file content after 3+ consecutive bash reads on same file

### Metrics (same model, same task)
| Metric | Before | v1.0.0 |
|--------|--------|--------|
| Simple edits | 35+ steps | 2-6 steps |
| Complex multi-file | 30+ (often failed) | 15-25 steps |
| Edit success rate | ~60% | ~95% |
| Build verification | None | Automatic |
| Loop/crash incidents | Frequent | Zero |

---

## v0.9.0 — UI Polish + Verification Loop + Stability

### Agent
- Verification loop: auto-verify edits with syntax check before finishing (once per turn)
- Edit-file specialized JSON parser: extract old_string/new_string from malformed JSON
- JSON string unescape fix: `extract_json_fields` properly handles `\n` `\t` `\"` `\\`
- Auto-summary: only fires when model produced no text after final tool calls
- Dynamic step limit: 25 base + 5 per edited file, max 50
- System reminder: includes syntax check prompt after every edit
- 400 "messages illegal" auto-recovery: trim conversation and retry
- `should_verify` simplified: checks if last tool was bash (not broken reverse iteration)

### UI
- Code blocks: Claude Code-style borders (`╭─ lang ─╮ │code│ ╰──╯`)
- Tool results: multi-line with diff coloring (red removed, green added)
- Tool calls: edit_file shows "find:" preview, bash shows multi-line
- Selection: fixed blue overlay (not color-inverted), scroll-preserving
- Drag selection: auto-scroll near edges with acceleration
- `Ctrl+Shift+C`: manual copy shortcut
- `Ctrl+L`: clear conversation
- Welcome page: full keyboard shortcut reference
- Input box: bottom hint bar (Enter/Shift+Enter/commands/Ctrl+L)
- Bracketed paste: enabled via crossterm feature flag, long paste collapsed to indicator
- Streaming mode: full keyboard access (not just basic input)
- Approval: accepts both uppercase and lowercase Y/A/N
- Loop force-stop: friendly message when work was completed
- Input box: word wrap for long pasted text
- Selection highlight: clipped to chat area (no blue bar on input box)

---

## v0.8.0 — Agent Intelligence Overhaul (20+ Optimizations)

**The largest single release — closing the gap with Claude Code.**

### Core Improvements
- `edit_file`: `replace_all` mode for bulk safe edits (e.g., change all CSS classes)
- `edit_file`: fuzzy whitespace matching + closest-match hints on failure
- `edit_file`: atomic writes (temp file + rename)
- System reminders every 4 steps (task + progress + sibling file hints)
- Pre-read file injection as system context (not synthetic tool calls)
- Token-budget-aware conversation windowing (hot/cold zones with condensation)
- Per-turn markdown logging (`datalog/` directory)
- `.atomcode.md` project instruction file support
- Environment metadata (OS, shell, git status) in system prompt

### Agent Loop Control
- Loop detection: 3 repeats → block, 4 consecutive blocks → force stop
- Read budget: 3 consecutive reads → hard redirect to edit
- Post-edit read blocking and sibling file check prompts
- Scouting detection (context-aware: allows curl when user reports runtime issues)
- Auto-summary when model doesn't summarize

### Error Recovery & Multi-Model Support
- 3-layer JSON repair pipeline (repair → extract → fallback)
- Stream timeout (120s idle), HTTP timeout (30s connect, 5min request)
- Token counting fix (DeepSeek cumulative usage values)
- CJK char boundary panic fix (4 locations)
- Conversation sanitize (orphan ToolResult removal via state machine)

### Tools
- `grep`/`glob`: fixed missing `current_dir` (caused ~75% grep failure rate)
- `glob`: fixed `**` pattern support (find -name parsing)
- `read_file`: smart large file handling, directory auto-recovery
- `write_file`: overwrite diff summary with script change detection
- `bash`: full output capture for long-running processes
- `bash`: `rm` source file detection requires approval
- Unified `SKIP_DIRS` across all tools (single definition)

### Key Metrics
| Metric | Before | v0.8.0 |
|--------|--------|--------|
| Simple tasks | 35+ steps | 3-6 steps |
| Complex tasks | 25+ (often failed) | 11-20 steps |
| grep success | ~25% | ~95% |
| edit success | ~60% | ~90% |
| Loops/crashes | Frequent | Eliminated |
| API 400 errors | Frequent | Eliminated |

---

## v0.7.0 — Phase 4 Complete: AgentLoop Extracted

**God Object decomposition — App split into AgentLoop + UI.**

### Architecture
- `AgentLoop` (atomcode-core): owns conversation, tools, provider, permissions
- `App` (atomcode-tui): owns UI state only (input, scroll, render cache)
- Channel-based communication: 7 `AgentCommand` types, 9 `AgentEvent` types
- AgentLoop runs as independent tokio task
- Main loop: `tokio::select!` polls TUI events and AgentEvents concurrently
- Recursive async with `Pin<Box>` for tool call chains
- `project_context` moved from atomcode-tui to atomcode-core

---

## v0.6.0 — Phase 1-3 Complete: Architecture Refactoring

### Phase 1 (Foundation)
- 1a: `ToolContext` — shared working_dir via `Arc<RwLock<PathBuf>>`
- 1b: `BashTool` stateless — reads working_dir from context
- 1c: `CdTool` self-contained — updates context directly
- 1d: `CancellationToken` — proper task cancellation on Ctrl+C
- 1e: `PermissionStore` — 4-level permissions, session grants, [A]lways allow

### Phase 2 (Claude Provider)
- Native `tool_use` content block protocol
- Streaming `content_block_start/delta/stop` parsing
- `input_json_delta` accumulation for tool arguments

### Phase 3 (Multi-tool-call)
- OpenAI provider tracks multiple tool calls by index
- `parallel_tool_calls` enabled
- Sequential execution with `pending_tool_calls` queue
- 8 tools, 3 providers with tool support, 34 tests

---

## v0.5.0 — Phase 1 Complete: Foundation Refactoring

### Changes
- `ToolContext`: shared working directory via `Arc<RwLock<PathBuf>>`
- `CancellationToken`: proper async task cancellation on Ctrl+C
- `PermissionStore`: 4-level permission system (AlwaysAllow/Ask/SessionAllow/AlwaysDeny)
- Destructive bash command detection (18 patterns)
- Sensitive file path protection
- Head+tail smart output truncation
- System prompt reasoning encouragement
- 8 tools: read, write, edit, bash, cd, grep, glob, list_directory

---

## v0.4.0 — Refactoring Phase 1 Start

### Changes
- New tools: grep, glob, list_directory
- Smart output truncation (head + tail)
- Reasoning-enhanced system prompt

---

## v0.3.0 — Intent Pre-processing + Project Intelligence

### Changes
- Project context injection (file tree + descriptor files)
- Dynamic project environment detection

---

## v0.2.0 — CLI Coding Agent MVP

### Features
- 5 tools: Read, Write, Edit, Bash, Change Dir
- Multi-provider: Claude, OpenAI, Ollama, any OpenAI-compatible API
- Agent loop with unlimited tool calls + auto-retry
- Claude Code-style TUI: markdown rendering, code with line numbers, tables
- Full conversation persistence and input history
- Real token counting from API
- Ratatui-based terminal UI with status bar
