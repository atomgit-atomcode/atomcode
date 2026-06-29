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

## Crate / API generation (resolved)

We use `agent-client-protocol = "1.0.1"` — the latest published version. Its API
is a **builder + handler-closure** model (NOT the older `trait Agent` /
`AgentSideConnection` shape, which only exists in ~year-old 0.4.x releases — both
0.15.x and 1.0.x already moved to the builder API). The wire data types live in
`agent-client-protocol-schema` v1.1.0, re-exported as
`agent_client_protocol::schema::v1::*`.

Key facts that shape the code:
- The crate is **edition 2024** and uses **native async closures** (`AsyncFnMut`),
  so it needs a Rust toolchain ≥ 1.85. Our `atomcode-acp` crate stays edition 2021
  and just depends on it.
- An agent is built as
  `Agent.builder().name("atomcode").on_receive_request::<InitializeRequest>(handler, on_receive_request!())… .connect_to(Stdio::new()).await`.
  Each request handler closure receives `(req, responder, cx: ConnectionTo<Client>)`;
  it calls `responder.respond(resp)` to answer and uses `cx.send_notification(...)`
  / `cx.send_request(...)` to stream updates and request permission.
- The `on_receive_request!()` / `on_receive_notification!()` macros supply a
  required `to_future_hack` boxing argument.
- Nearly every schema type is `#[non_exhaustive]`; construct via `::new(...)`
  builders, not struct literals.
- Wire protocol is identical to the older series (advertise `ProtocolVersion::V1`),
  so editor/orchestrator interop (e.g. Zed) is unaffected by the crate generation.

**Open risk to confirm in the first task (spike):** whether `connect_to`'s
dispatch loop runs handler futures concurrently — i.e. whether a `session/cancel`
notification is delivered while a `session/prompt` handler is still awaiting. Our
cancel semantics depend on it; the spike's smoke test pins this behavior.

## Components

Three internal modules, each independently testable. (The crate's builder
subsumes what would otherwise be a hand-rolled JSON-RPC transport, so there is no
separate `transport` module — but the **single-writer / stdout-discipline**
invariant still applies and is owned by the crate's `Stdio` transport.)

### `protocol`
Thin adapter over `agent_client_protocol` (incl. `schema::v1::*`): re-exports the
types the rest of the crate uses and centralizes capability/version construction,
so dispatch/translate depend on our adapter surface rather than scattering crate
paths. Also owns construction of the agent `Builder` with all handlers wired.

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

kernel `StopReason` → ACP `schema::v1::StopReason`: `Stopped → EndTurn`;
`MaxRounds`/`MaxContinuations → MaxTurnRequests`; `Cancelled → Cancelled`;
`PromptRejected → Refusal`; `ProviderError`/`Timeout`/`RateLimited` → JSON-RPC
error returned from the prompt handler (not a stop reason).

Tool **kind** mapping: atomcode tool names → ACP `ToolKind` (`Read` / `Edit` /
`Execute` / `Search` / `Fetch` / … / `Other`) so the client shows appropriate
affordances/icons. Edit/write tools attach a `ToolCallContent::Diff { path,
old_text, new_text }` so the client renders a diff.

## Permission flow

The kernel approval request carries `ApprovalRequest { call_id, tool, args }`
(`atomcode-capabilities/src/tools/approval.rs`); the response it expects is
`ApprovalResponse { decision: "allow"|"allow_always"|"deny", remember: bool }`
(fail-closed to `deny`).

```
kernel  AgentEvent::Request{ id: RequestId(u64), kind:"approval",
                              payload: ApprovalRequest }
  → cx.send_request(RequestPermissionRequest::new(session_id, tool_call_update, options))
        options = [AllowOnce, AllowAlways, RejectOnce, RejectAlways]
                  (PermissionOption with stable option_id strings)
  → client returns RequestPermissionResponse {
        outcome: Selected { option_id } | Cancelled }
  → map option_id → decision JSON:
        allow_once    → {"decision":"allow"}
        allow_always  → {"decision":"allow","remember":true}
        reject_*      → {"decision":"deny"}
        Cancelled     → {"decision":"deny"}   (fail closed)
  → AgentCommand::Respond{ id, value }
```

The kernel-native `RequestId` lets multiple concurrent approvals correlate
correctly — the reason for choosing the kernel-native path over the legacy bridge.

## `initialize` capabilities (v1)

`InitializeResponse::new(req.protocol_version).agent_capabilities(...)`:
- `prompt_capabilities`: `image(true)` (kernel `SendMessage` already carries
  `images: Vec<ImageContent>`); `embedded_context` left false in v1
- `load_session`: false (resume deferred to a later phase)
- `auth_methods`: `[]` — atomcode authenticates via its own `/login` / config.
  When unauthenticated, `session/new` returns a clear error directing the user to
  run `atomcode login`.

Echo the client's `protocol_version` back (clamped to a version we support;
`ProtocolVersion::V1`).

## Error handling

- unknown/unhandled method → answered via the crate's catch-all dispatch with a
  JSON-RPC error; the dispatch loop survives (1.0.1 also ignores unhandled
  notifications by default)
- kernel fatal (`ProviderError` / `Timeout` / `Error`) → the prompt handler
  returns `Err(agent_client_protocol::Error)` → JSON-RPC error response
- stdout-corruption prevented by routing ALL diagnostics to stderr/file (the
  crate's `Stdio` owns stdout); enforced by review + a no-`println!`-in-crate rule

## Testing strategy (TDD)

- **Unit — `translate`**: table-driven; each kernel `AgentEvent` → asserted ACP
  `SessionUpdate` (compare serialized JSON). Pure functions, highest-value
  coverage. Also `StopReason` mapping and tool-name → `ToolKind`.
- **Unit — `dispatch` session map + permission mapping**: pure helpers — session
  insert/lookup, `option_id → ApprovalResponse` decision mapping — tested directly
  without the transport.
- **Integration**: a fake ACP client (itself built with `Client.builder()` over an
  in-process duplex, or a spawned `atomcode acp` child over stdio pipes) drives
  `initialize → session/new → session/prompt`, asserting it receives
  `agent_message_chunk`s, a `request_permission` round-trip, and a terminal
  `stop_reason`. Uses a stub provider so no network is required.

## File touch list (anticipated)

- `crates/atomcode-acp/` — new crate (`Cargo.toml`, `src/lib.rs`,
  `src/protocol.rs`, `src/dispatch.rs`, `src/translate.rs`, `src/engine.rs`,
  tests)
- `crates/atomcode-cli/src/main.rs` — `Acp` command variant + handler
- `Cargo.toml` (workspace) — `agent-client-protocol` + `agent-client-protocol-schema`
  in `[workspace.dependencies]` (pinned `=1.0.1` / matching schema)
- `crates/atomcode-cli/Cargo.toml` — depend on `atomcode-acp`

## Build notes

Per repo constraints: build per-package with `CARGO_INCREMENTAL=0`, not the whole
workspace. The `agent-client-protocol` 1.0.1 dep is **edition 2024 + native async
closures** → needs toolchain ≥ 1.85 (fine for 2026); `atomcode-acp` itself stays
edition 2021. Watch the added dependency weight under the size-optimized release
profile (`opt-level=z`, `lto`, `panic=abort`); pin `=1.0.1` to avoid surprise
API churn in this young crate.
