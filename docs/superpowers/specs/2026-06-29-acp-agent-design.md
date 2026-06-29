# `atomcode acp` — ACP Agent Mode (v1)

Date: 2026-06-29
Branch: `feat/acp-agent` (worktree off `main`)
Status: design approved, pending spec review

## Motivation

Multi-agent collaboration is increasingly common. atomcode currently cannot be
plugged into editors/orchestrators that speak the **Agent Client Protocol (ACP)**
— the JSON-RPC-over-stdio protocol (from Zed) that lets a *client* (editor or
multi-agent orchestrator) launch an *agent* subprocess and drive it. We want
`atomcode acp` so atomcode can act as one of the agents in such a team — the same
way Claude Code can be dropped into Zed.

## Scope (v1)

**Role: Agent side only.** atomcode runs as an ACP agent subprocess, driven over
stdio. (Client side — atomcode orchestrating *other* ACP agents — is explicitly
out of scope; future separate spec.)

**Feature set: core + permissions.**

In scope:
- `initialize`, `session/new`, `session/prompt`, `session/cancel`
- streaming `session/update` notifications (text, reasoning, tool calls)
- `session/request_permission` wired to atomcode's existing approval flow
- tool-call updates carry structured diff content for edit tools (so Zed renders
  diffs) — this is presentation only, NOT full filesystem delegation

Out of scope (future phases, each its own spec):
- Phase 2: filesystem delegation (`fs/read_text_file`, `fs/write_text_file`) so the
  editor's unsaved buffers are respected
- Phase 3: terminal delegation (`terminal/*`), `plan` updates, `available_commands`
  (slash commands), `authenticate`
- `session/load` (session resume) — advertised as unsupported in v1

## Key decisions

| Decision | Choice | Rationale |
|---|---|---|
| Role | Agent (driven) | Direct meaning of "plug atomcode into a multi-agent team" |
| v1 scope | Core + permissions | Minimal complete set that actually runs inside an ACP client; nearly a pure translation layer over existing kernel channels |
| Engine | kernel-native `AgentHandle` via coding `assemble` | ACP permissions need JSON-RPC request-id correlation; the kernel's native `RequestId` maps 1:1, while the legacy bridge collapses concurrent approvals |
| Protocol types | official `agent-client-protocol` crate | Wire-format + version-negotiation correctness, best Zed interop; isolated behind a thin adapter so it can be swapped |
| Isolation | dedicated worktree off `main`, new branch `feat/acp-agent` | New feature, keep release branch clean, avoid per-turn auto-commit hook packaging WIP |

## Architecture

New crate **`atomcode-acp`**, depending on `atomcode-kernel`,
`atomcode-coding`, `atomcode-capabilities`, the official `agent-client-protocol`
crate, plus `serde_json` / `tokio`.

Single public entry point:

```rust
pub async fn serve_stdio(opts: AcpServeOptions) -> anyhow::Result<()>
```

`AcpServeOptions` carries provider/model overrides resolved from CLI global flags
and the resolved atomcode config. The working directory is NOT fixed here — the
client supplies it per session via `session/new`.

CLI: add an `Acp` variant to the `Commands` enum in
`crates/atomcode-cli/src/main.rs`. The handler resolves config (reusing the
existing provider/model resolution the headless path uses) and calls
`atomcode_acp::serve_stdio`. The subcommand reuses the existing global
`--provider` / `--model` flags; cwd comes from the client.

### Stdout discipline (hard invariant)

In ACP mode **stdout is reserved exclusively for the ACP JSON-RPC stream**. Any
stray `println!` corrupts the protocol. atomcode's headless mode already leaves
stderr pointed at the real terminal and keeps stdout clean (no global stdout sink
— confirmed in cli startup). All diagnostics in `atomcode-acp` go to stderr or a
file sink. This is guarded by code review and a transport-level single-writer.

## Components

Four internal modules, each independently testable.

### `transport`
JSON-RPC 2.0 over stdio, newline-delimited (NDJSON — ACP's wire format).
- stdin read loop → decoded requests/notifications/responses
- **single stdout writer task** fed by an mpsc channel, so concurrent
  notifications never interleave
- allocates outbound request ids for agent→client calls (permission now; `fs/*`
  later)
- protocol/parse errors produce JSON-RPC error responses; the read loop never
  crashes

(The official crate provides a connection harness over an async read/write pair;
`transport` adapts atomcode's stdin/stdout into it and owns the single-writer
guarantee.)

### `protocol`
ACP wire types for the v1 surface, sourced from the official
`agent-client-protocol` crate, re-exported / thinly wrapped so the rest of the
crate depends on our adapter, not the crate directly.

### `dispatch`
Method router + session table (`HashMap<SessionId, SessionState>`; multiple
concurrent sessions supported).
- `initialize` → capabilities response (see below)
- `session/new` → run `prepare → assemble → spawn` bound to the client-supplied
  cwd; store the `AgentHandle`; return a fresh `sessionId`
- `session/prompt` → translate prompt content blocks into
  `AgentCommand::SendMessage { text, images }`, pump kernel events into
  `session/update` notifications until `TurnComplete`, then return
  `{ stopReason }`
- `session/cancel` (notification) → `AgentCommand::Cancel`

### `translate`
Pure functions: kernel `AgentEvent` → ACP `session/update` (and `StopReason` →
ACP `stopReason`). The primary unit-test target (table-driven).

## Engine integration (path (a))

Per session:
1. `prepare(&cfg, PrepareOptions { cwd, ... }).await` → `CodingParts`
   (`atomcode-coding`; handles MCP connect, skill loading, session binding)
2. `assemble(&mut parts, &cfg, provider).await` → kernel-native `Agent`
   (`crates/atomcode-coding/src/parts.rs:396`)
3. `agent.spawn()` → `AgentHandle { commands, events, task }`
   (`crates/atomcode-kernel/src/agent.rs:366`)
4. pump loop: drain `events` → `session/update`; route inbound prompt/cancel →
   `commands`
5. on session end / shutdown: `AgentCommand::Shutdown`, await `task`

This mirrors how the cli builds a provider and `CodingAgentConfig` for its
headless path, but keeps the **native** handle (the cli headless v2 path routes
through `spawn_bridged_runtime`; ACP deliberately does not, to preserve native
`RequestId`).

## Event mapping

| kernel `AgentEvent` | ACP |
|---|---|
| `TextDelta(s)` | `session/update` → `agent_message_chunk` |
| `Reasoning(s)` | `session/update` → `agent_thought_chunk` |
| `ToolStarted { call }` | `session/update` → `tool_call` (id, title, kind, status; edit tools carry structured diff content) |
| `ToolResult { result }` | `session/update` → `tool_call_update` (status completed/failed + content) |
| `Request { id, kind:"approval", payload }` | `session/request_permission` request; client choice → `AgentCommand::Respond { id, value }` |
| `TurnComplete { reason }` | prompt response `stopReason` |
| `Error` / provider failure | prompt returns a JSON-RPC error |
| `Cancelled` | `stopReason: cancelled` |
| `Usage` / `RateLimited` / `Warning` | v1: log only (no standard ACP update field) |

`StopReason` → ACP `stopReason`: `Stopped → end_turn`;
`MaxRounds`/`MaxContinuations → max_turn_requests`; `Cancelled → cancelled`;
`ProviderError`/`Timeout` → JSON-RPC error on the prompt response.

Tool **kind** mapping: atomcode tool names → ACP kinds (read / edit / execute /
search / …) so the client shows appropriate affordances/icons.

## Permission flow

```
kernel  AgentEvent::Request{ id: RequestId(u64), kind:"approval", payload }
  → transport sends session/request_permission { sessionId, toolCall, options }
  → client returns selected option (allow once | allow always | reject once | reject always)
  → map to atomcode decision JSON
  → AgentCommand::Respond{ id, value }
```

The native `RequestId` lets multiple concurrent approvals correlate correctly —
the reason for choosing the kernel-native path over the legacy bridge.

## `initialize` capabilities (v1)

- `promptCapabilities`: text + image (kernel `SendMessage` already carries
  `images: Vec<ImageContent>`)
- `loadSession`: false (resume deferred to a later phase)
- `authMethods`: `[]` — atomcode authenticates via its own `/login` / config.
  When unauthenticated, `session/new` returns a clear error directing the user to
  run `atomcode login`.

## Error handling

- protocol parse / unknown-method → JSON-RPC error response; read loop survives
- kernel fatal (`ProviderError` / `Timeout` / `Error`) → JSON-RPC error on the
  prompt response
- stdout-corruption prevented by the single-writer transport + the no-stdout-print
  rule (review-enforced)

## Testing strategy (TDD)

- **Unit — `translate`**: table-driven; each `AgentEvent` → asserted ACP JSON
  frame. Pure functions, highest-value coverage.
- **Unit — `dispatch`**: feed a mock `AgentHandle` (scripted `AgentEvent`s at the
  channel level), assert the emitted JSON-RPC frame sequence for
  `initialize → session/new → session/prompt → chunks → stopReason`, including a
  `request_permission` round-trip.
- **Integration**: a fake ACP client drives the real binary over stdio pipes
  through `initialize → session/new → session/prompt`, asserting it receives
  message chunks, a permission round-trip, and a terminal `stopReason`.

## File touch list (anticipated)

- `crates/atomcode-acp/` — new crate (`Cargo.toml`, `src/lib.rs`,
  `src/transport.rs`, `src/protocol.rs`, `src/dispatch.rs`, `src/translate.rs`,
  tests)
- `crates/atomcode-cli/src/main.rs` — `Acp` command variant + handler
- `Cargo.toml` (workspace) — `agent-client-protocol` in `[workspace.dependencies]`
- `crates/atomcode-cli/Cargo.toml` — depend on `atomcode-acp`

## Build notes

Per repo constraints: build per-package with `CARGO_INCREMENTAL=0`, not the whole
workspace. Watch the dependency weight added by `agent-client-protocol` (release
profile is size-optimized: `opt-level=z`, `lto`, `panic=abort`).
