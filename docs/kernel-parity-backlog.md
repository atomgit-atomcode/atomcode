# Kernel/Capabilities/Coding → cli/tuix Parity Backlog

**Goal:** complete the new inverted stack (L0 `atomcode-kernel` + L1 `atomcode-capabilities` + L2 `atomcode-coding`) to **full functional parity** with what production `cli/tuix`需要, BEFORE wiring it into cli/tuix — so the eventual integration is parity, not degraded.

**Hard rules (every item):**
- **Zero `atomcode-core` change** — `cargo tree -p <crate>` shows 0 atomcode-core; `git status --short -- crates/atomcode-core` empty after every commit.
- All kernel additions are **additive** (`#[serde(default)]`, no breaking variant/field changes to construction sites that can't be batch-fixed).
- **Cache red-line**: a text-only conversation serializes byte-identical. Memory / context injection happens at `session_start`/`turn_start` (permanent, pre-cache) or as a **tail** `pre_request` append — NEVER a prefix mutation (the `pre_request` guard now Warns on this).
- New providers/tools/middleware/hooks are validated with the **conformance kit** (`atomcode_kernel::conformance::{provider,tool,middleware,hooks}::check`).

Legend: effort `[T]`rivial `[S]`mall `[M]`edium `[L]`arge · status ☐ todo / ◐ in-progress / ☑ done.

---

## Phase A — Kernel L0 additive primitives (unblock everything; small, zero-core)

These few primitives unlock the bulk of Tier-3 driver features without per-feature kernel work.

- ☐ **A1 [M] `AgentCommand::SetConversation(SessionSnapshot)`** — runtime history replace (re-seed convo, re-validate tool-call/result pairing, bump `cache_epoch`). **Unlocks**: `ClearConversation` (replace with empty), `SetMessages`/`/resume` in-flight (replace with loaded), `/undo`/`UndoToPrompt` (driver computes the truncated snapshot, sends it), model-swap-with-continuity (snapshot → new agent → SetConversation). Acceptance: replace mid-session; next turn sends the new history; pairing valid; cache_epoch bumps.
- ☐ **A2 [S] `AgentCommand::ChangeDir(PathBuf)`** — mutate the working dir the `ToolContext` reports for subsequent tool calls (currently build-time only). **Closes**: `/cd`, `WorkingDirChanged`. Acceptance: a `WorkingDirProbeTool` sees the new dir after the command; snapshot/UI reflect it.
- ☐ **A3 [S] Pre-turn input injection** — `AgentCommand::AppendInput(String)` queues a user message merged into the NEXT turn (before the LLM call); plus a "inject a message WITHOUT triggering an LLM turn" path for `LocalShell` (a `SendMessage`-like with a `respond:false` flag, or a driver-side concern — decide during design). Acceptance: queued text lands before the next LLM call; the no-LLM path adds a message and ends without a provider call.
- ☐ **A4 [S] `SessionSnapshot.turn_stats: Vec<TurnStat>`** (additive) — per-turn `{after_message, turn_count, tool_call_count, duration_ms, total_tokens, errored}` so `/resume` re-renders the `✓ … tokens` dividers. Acceptance: snapshot round-trips turn_stats; old snapshots load (`serde(default)` → empty).
- ☐ **A5 [M] Context-budget introspection** — emit an `AgentEvent::ContextStats{system_tokens, sent_tokens, dropped_tokens, working_set_tokens, total_messages, tool_defs_tokens, ctx_window, ...}` (via a builder-opt hook fired after message assembly / in `on_request`). **Closes**: `/context`, `RefreshContextStats`. Acceptance: a driver can render the budget breakdown without guessing.
- ☐ **A6 [S] (decide) terminal-event persistence** — either add `#[serde(default)] messages` to `TurnComplete`/`Error`/`Cancelled`, OR document "driver sends `Snapshot` on terminal". Prefer the latter (no duplication); make it a driver-adapter task (D-tier), NOT a kernel change. Acceptance: driver can persist on every terminal path.

> A1–A5 are the only kernel-source changes in this backlog. Each ships with conformance-style tests + zero-core verification.

---

## Phase B — L1 `atomcode-capabilities` (the bulk; new capability modules)

- ☐ **B1 [L] MCP capability** (`atomcode-capabilities::mcp`, the reserved L1) — McpRegistry + server lifecycle (spawn/reload/login/logout) + dynamic tool discovery, surfaced as kernel `Tool`s. Dynamic mounting tension: kernel mounts at build → either a **shared mutable tool registry** the agent re-reads, or **re-spawn** the agent on MCP change (lean re-spawn first; revisit). Validate each discovered tool with `conformance::tool::check`. Acceptance: an MCP server's tools become callable in a turn; `/mcp` lifecycle works.
- ☐ **B2 [M] Provider factory + runtime swap** — `create_provider(config)` over the L1 providers (OpenAiCompat / Anthropic / Ollama, now present), plus the **re-spawn** pattern: on `/model`·`/provider`, build a new provider → new `Agent` → `SetConversation(snapshot)` for continuity. No kernel change (factory pattern, like core's `AgentRuntimeFactory`). Validate each provider with `conformance::provider::check`. Acceptance: switch model mid-session, conversation preserved, no prefix-cache break on the carried history.
- ☐ **B3 [L] Session persistence layer** (`atomcode-capabilities::session` or a driver crate) — `SessionManager` over `SessionSnapshot` + metadata (`name`, `working_dir`, `created_at`/`updated_at`, `user_renamed`, `turn_stats`) + `save/load/list/delete/rename` to `$ATOMCODE_HOME/sessions/<project_hash>/`. Resume re-seeds via A1 `SetConversation` (in-flight) or `AgentBuilder::resume` (build-time). Acceptance: list/load/save/rename/delete; `/resume` picker populated; dividers re-render (needs A4).
- ☐ **B4 [M] Memory store + injection hook** (`atomcode-capabilities::memory`) — `memory.md` read/write (global + project) + a `LifecycleHooks` that injects memory into the system/persona at **`session_start`/`turn_start`** (permanent, pre-cache — NOT `pre_request`, to respect the cache guard). **Closes**: `/remember`·`/forget`·`/memory`, `Remember/Forget/ShowMemory`. Acceptance: `/remember` persists; next session's system carries it; cache prefix stays byte-stable across turns.
- ☐ **B5 [M] Plan-mode middleware** — a `ToolMiddleware` that blocks write/risky tools when a shared `plan_mode` flag is set (toggled by a command or `Respond`). **Closes**: `SetPlanMode`, `/plan`. Validate with `conformance::middleware::check`. Acceptance: in plan mode, `write_file`/`edit_file`/risky `bash` are blocked with a clear ToolResult; toggle flips it.
- ☐ **B6 [M] File-history / edit-undo + git checkpoint** (capabilities tool-level) — record file edits so a file-level `/undo` can revert, + optional git checkpoint per turn. Distinct from conversation `/undo` (A1). Acceptance: a tool edit can be reverted to its prior bytes.
- ☐ **B7 [S] Live tool-output streaming** — bash/long tools emit real stdout chunks via the kernel `ProgressSink` (→ `AgentEvent::ToolProgress`). Adapter renders them as `ToolOutputChunk`. Acceptance: `bash` output streams live mid-execution, not just at result.
- ☐ **B8 [S] Skills wiring** — mount `use_skill`/`list_skills` (already in `atomcode-capabilities::skills`) into the coding assembly; expose the registry for the slash palette. Acceptance: `/use_skill` works; palette lists `user_invocable()` skills.

---

## Phase C — L2 `atomcode-coding` (assemble the completed capabilities)

- ☐ **C1 [M] "Full" coding-agent assembly** — extend `build_coding_agent` (or a new `build_full_coding_agent`) to wire: memory hook (B4), plan-mode middleware (B5), MCP tools (B1), file-history (B6), skills (B8), session persistence handle (B3), provider factory (B2). Order load-bearing (approval middleware first). Acceptance: one assembled agent exposes every parity capability; assembly test asserts each is mounted/wired.
- ☐ **C2 [L] Background + parallel sub-agent composition** — `/bg` (isolated child session) and `parallel_edit` as **L2 composition** over kernel `Agent` (kernel already proves subagent-by-composition + `ToolProgress` for nested progress). A small pool + per-task turn budget. **Closes**: `Background/BackgroundComplete`, `SubAgentDispatch*`, `parallel_edit_files`. Acceptance: a background task runs isolated and reports completion; parallel edits dispatch + report per-task.
- ☐ **C3 [S] Vision/image preprocessing middleware** — pre-process pasted images (optional VL describe) before `SendMessage` reaches the kernel (kernel just forwards `ImageContent`). **Closes**: `VisionPreprocessSuccess`/`RestorePendingImages` (as L2/driver concerns). Acceptance: an image send round-trips; preprocessing failure surfaces a re-attach path.

---

## Phase D — Driver adapter prerequisites (tuix-side glue; build once A–C land)

- ☐ **D1 [M] Bidirectional event/command translator** — kernel↔tuix `AgentEvent`/`AgentCommand` mapping, incl. **PhaseChange synthesis** (state machine over TurnStarted/TextDelta/ToolStarted/Request), **ToolBatch grouping** (same assistant message ⇒ one batch), **approval-id correlation** (track pending `Request.id` → `Respond{id}`), **duration tracking** (ToolStarted→ToolResult elapsed), TurnComplete metadata aggregation.
- ☐ **D2 [L] Slash-command dispatcher** — route tuix `/cmd` → kernel commands / `Respond` / capability calls (30+ commands). Reuse existing modals (DirPicker/ModelPicker/SessionPicker) but target the new stack.
- ☐ **D3 [M] Message-shape bridge** — core `Message::MessageContent` enum ↔ kernel flat `Message` (incl. `ToolResultRef` disk-backed summary: bridge or deliberately drop). Only needed to interop OLD core sessions; new kernel sessions are self-consistent.

---

## Phase E — Integration (AFTER A–D; the actual wiring + strangle)

- ☐ **E1 [M] Parallel driver path behind a flag** — a tuix entry that spawns the full coding agent (C1) through the adapter (D1) and renders a real turn. Everything from A–C is wired, nothing degraded.
- ☐ **E2 [M] Parity validation** — run the existing tuix flows (turn, approval, tools, compaction, sessions, /undo, /model, /cd, plan, skills, MCP, bg) against the new path; close gaps.
- ☐ **E3 [L] Strangle** — make the new path default; remove the core agent path feature-by-feature once each is at parity.

---

## Suggested order & critical path

1. **A1 → A2 → A4 → A3 → A5** (kernel primitives; I can do these — small, zero-core, conformance-tested).
2. **B2 (provider factory) + B3 (session persistence)** — needed for any real multi-turn parity.
3. **B4 (memory) + B5 (plan) + B7 (tool-output) + B8 (skills) + B6 (file-history)** — parallelizable.
4. **B1 (MCP)** — largest L1; can lag.
5. **C1 (full assembly)**, then **C2 (bg/parallel)**, **C3 (vision)**.
6. **D1 → D2 → D3** (adapter), then **E**.

**Parity gate before E:** A1–A5 ☑, B2/B3/B4/B5/B7/B8 ☑, C1 ☑ (B1/B6/C2/C3 may trail as known-degraded if explicitly accepted).
