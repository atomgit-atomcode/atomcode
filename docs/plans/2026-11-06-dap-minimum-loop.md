# DAP Minimum Loop Implementation Plan

**Goal:** Default-on tool-driven debugging: launch a program with breakpoints, inspect a stop, step, continue, and clean up.

**Architecture:** `atomcode-capabilities::tools::debug` owns DAP transport and one live session per prepared tool. Coding prepares and mounts that tool through the existing host-tools registry. Kernel stays neutral; no new runtime owner or persistent debug state. Config holds adapter command/args only, never credentials. Default is enabled; installed Python/debugpy and LLDB adapters are discovered lazily inside the approved launch call. A provider-only patch or in-place skill refresh preserves the tool; an actual capability/session/config rebuild drops its session. Cancel, timeout, protocol error, and tool Drop fail closed and reap the adapter process group/job. No attach, reverse process launch, auto-install, or memory writes in v1. No process probes at runtime startup; missing debugpy produces an interpreter-specific install hint.

**Tech Stack:** Existing Rust/Tokio/serde dependencies; DAP Content-Length framing with seq/request_seq (not JSON-RPC).

## Decisions and trade-offs

- Prefer a native capability to shell-driven ad hoc debuggers or an MCP dependency: protocol state stays explicit and lifecycle ownership follows existing tool assembly. Cost: maintain framing and a small session state machine.
- Generic stdio adapters, one session. Built-in lazy detection for Python (project venv, active venv, PATH) and LLDB; explicit configuration overrides defaults. Avoid a large catalog and network transport while validating the loop. Target real debugpy as the first integration.
- All actions conservatively Risky, including evaluate and variable rendering (adapters may execute getters). Existing approval and plan-mode gates apply. No alternate grant path.
- Launch includes initial source breakpoints, because adapters may defer launch completion until configurationDone. Start launch without waiting for its response, await initialized, configure, then await completion.
- Serialize calls. Return a session id plus stop epoch; stop-scoped ids require both to reject stale frame/variable references. No persisted handles and no implicit resume.
- Bound frame bytes, headers, output, arrays, and strings; reject unsupported reverse requests. Timeout/cancel destroys ambiguous sessions rather than advertising a usable debugger.

## Tasks

1. Implement bounded DAP stdio client and RAII process cleanup under `crates/atomcode-capabilities/src/tools/debug/`.
2. Implement `debug` tool actions, state guards, cancellation, bounded output and schemas; add deterministic fixture tests for launch ordering, errors, stale refs, cancellation and timeout.
3. Add `[dap]` / `[dap.adapters.<id>]` schema in atomcode-config and typed conversion in coding. Mount by default unless explicitly disabled through existing `parts::prepare`, including daemon configuration. No changes to kernel, lifecycle hooks, or session formats.
4. Test a real debugpy loop and production tool mounting; document configuration, approval, limitations and lifecycle behavior.
5. Run relevant nextest suites, compile gate, fmt and diff checks. Do not commit/push without a request.

Baseline: release/v5.2.7 at 5cdd4ad77; clean worktree. Reference: local oh-my-pi `src/dap/{client,session,config}.ts`, `src/tools/debug.ts`; official DAP overview.

User refinement: default-on must work without hand-written configuration. Validate the full real debugpy loop with Settings::default(), a project .venv, and no adapter argument; retain --no-tools and explicit disable behavior. Runtime state/failed-test investigations may select debug when dynamic evidence is needed, without a fixed auto-debug stage.

Review validation: exercise adapter and child cleanup through the production runtime, including preservation across provider patches/in-place skill refresh and cleanup on actual capability rebuild, explicit disable, and shutdown.
