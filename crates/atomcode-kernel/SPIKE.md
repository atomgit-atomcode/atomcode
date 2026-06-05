# atomcode-kernel — Phase A0 Spike

Design-validation spike for the neutral-kernel platform strategy
(`docs/superpowers/specs/2026-06-05-atomcode-kernel-platform-strategy.md`).
Internals are minimal/throwaway — the public API *shape* is what Phase A1 carries
the proven production hot-paths into.

## What it proves

| Claim | Where |
|---|---|
| 1. Neutral kernel — a turn runs with no persona and no middleware | `tests/spike_claims.rs::neutral_turn_runs_without_persona_or_middleware` |
| 2. Approval is an external middleware over an id-correlated round-trip | `tests/spike_claims.rs::approval_middleware_gates_risky_tool_via_id_roundtrip` |
| 3. Selective tool mounting — unmounted tools invisible/inert | `src/tool.rs::tests::only_mounted_tools_are_exposed_or_resolvable` |
| 4. One primitive serves one-shot AND interactive drivers | `tests/...::one_shot_adapter_auto_answers_and_aggregates` + `examples/minimal_specialization.rs` |
| 5. Wire-compatible (serde round-trip) → web/daemon can use the same seam | `tests/...::events_and_commands_are_wire_serializable` |

## Driver model

One primitive: a long-lived session consuming `AgentCommand` and emitting
`AgentEvent` (`AgentHandle`). The round-trip seam is the id-correlated
`AgentEvent::Request{id,kind,payload}` ↔ `AgentCommand::Respond{id,value}`. The
`oneshot` that resolves a middleware's await lives only in `RequestCtx` (kernel),
never in an event — so events/commands are serializable and work in-process AND
over the wire. `run_to_completion(input, policy)` is the one-shot adapter for
batch/CI. All four driver shapes (one-shot/CI, TUI, web, server) sit on this one
primitive:

| Driver | Command source | Event sink | Request answered by |
|---|---|---|---|
| one-shot / CI / CodeReview | one SendMessage | aggregated Outcome | AutoRespond policy |
| TUI | keypresses | render loop | modal → Respond |
| Web | WS/HTTP → AgentCommand | AgentEvent → SSE/WS | user → Respond frame |
| server / daemon | per-session RPC | per-session SSE | policy or remote user |

## Key boundary facts

- The kernel core (`agent.rs`, `event.rs`, `tool.rs`) never names "approval".
  Tools carry only a `RiskLevel` flag; approval lives in
  `testkit::ApprovalMiddleware` (specialization side) over `RequestCtx::request`.
- `ToolContext` carries no semantic/graph/lsp services — the kernel needs none.
- Crate excluded from workspace `default-members`, so product builds are untouched.

## Run

    cargo test -p atomcode-kernel
    cargo run -p atomcode-kernel --example minimal_specialization

## Next (Phase A1)

Carry production hot-paths into these slots WITHOUT rewriting: `TurnRunner` loop →
`agent.rs`; `ctx/render` → a `CtxBuilder` impl behind the persona injection point;
`conversation` → `message.rs`; neutral provider impls → `provider.rs`. Preserve
prefix-cache invariants and existing edge-case fixes.
