# Native Runtime Datalog Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Restore the configured per-turn Markdown datalog and per-LLM-round JSONL request log on the native `CodingRuntime` path.

**Architecture:** Add a provider-neutral observer implementing both `LifecycleHooks`
and `ToolMiddleware` in `atomcode-capabilities` and mount the same instance from
`atomcode-coding::assemble`, the common production assembly boundary used by CLI,
TUI, daemon, ACP, and clix. Preserve the project-bucket and paired Markdown/JSONL
layout without restoring `atomcode-core` or a second runtime owner.

**Tech Stack:** Rust, `atomcode-kernel::LifecycleHooks`, `atomcode-config::DatalogConfig`, serde JSON, SHA-256.

---

## Design

`DatalogHook` owns only observation state for the active turn. `user_prompt_submit`
buffers the final rewritten prompt. The first `on_request` creates a collision-safe
pair whose name includes session/turn/process/instance identity, appends the final
neutral kernel request (`messages`, `tools`, `ChatOptions`, `TurnCtx`) as one compact
JSONL record, and adds a request summary to the Markdown file. `on_model_response`
records assistant text, reasoning, tool calls, and usage. The tool middleware records
success, failure, and denied tool results even when there is no next LLM request.
`on_error` records provider/runtime errors. `turn_complete` records the terminal
reason and duration and drains queued writes.

The configured root follows the historical rules: omitted directory uses
`$ATOMCODE_HOME/datalog`; `~/...` expands against the real user home; absolute paths
remain fixed; relative paths resolve from the runtime working directory. A sanitized
project basename plus an eight-character stable SHA-256 suffix is always appended.
All directory creation and writes are best-effort: logging must never reject a prompt,
alter a request, panic, or change a turn terminal. File work runs on a dedicated
single-writer thread, project directories and files are `0700`/`0600` on Unix, and
new pairs use `create_new` with a collision suffix. The hook contains no provider,
session, controller, or persistence ownership.

`DatalogConfig` is threaded through `CodingRuntimeConfig` and `CodingAgentConfig`.
The hook is built during `assemble`, not `prepare`, so provider/model reassembly records
the active model while reusing the same runtime/session lifecycle. Disabled logging
mounts no hook and creates no files.

## Task 1: Add the neutral datalog hook

**Files:**
- Create: `crates/atomcode-capabilities/src/datalog.rs`
- Modify: `crates/atomcode-capabilities/src/lib.rs`

1. Add failing tests for disabled logging, path resolution, project collision
   avoidance, multi-round JSONL, Markdown response/error content, and terminal flush.
2. Implement the minimal `LifecycleHooks` writer.
3. Run `cargo test -p atomcode-capabilities datalog --lib`.

## Task 2: Mount it at the common coding boundary

**Files:**
- Modify: `crates/atomcode-coding/src/config.rs`
- Modify: `crates/atomcode-coding/src/parts.rs`
- Update: explicit `CodingAgentConfig` / `CodingRuntimeConfig` constructors reported
  by the compiler.

1. Thread `DatalogConfig` from the product config into agent assembly.
2. Mount one generation-local hook when enabled.
3. Add an assembly test proving a real mock-provider turn creates both files.
4. Run `cargo test -p atomcode-coding datalog`.

## Task 3: Cross-entry verification

1. Run `cargo test -p atomcode-capabilities -p atomcode-coding --lib`.
2. Run compile checks for CLI, daemon, clix, and TUI consumers if constructor changes
   reach them.
3. Run `git diff --check`.
4. Inspect the final diff to verify no changes overlap the user's existing TUI/Cargo
   worktree modifications.
