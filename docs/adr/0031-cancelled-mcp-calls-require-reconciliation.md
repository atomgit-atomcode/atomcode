# 0031: Cancelled dispatched MCP calls require reconciliation

Date: 2026-11-11
Status: Implemented

## Context

Harness already supplies turn cancellation through ToolContext. MCP adapters previously ignored it. Dropping a dispatched stdio request can leave a partial or late reply on the shared stream; dropping an HTTP request cannot prove that its remote side effect stopped. Returning promptly must not imply that browser or desktop actions were undone.

## Decision

Use the existing turn token in the adapter. A pre-cancelled call sends no request. Each transport arms a synchronous guard immediately before dispatching a tool request. Dropping that future marks its client uncertain before serialized access is released. Discovery and subsequent requests then fail closed until the existing explicit reconnect replaces the client. A queued, undispatched call does not poison its neighbor. A normal result disarms only its own guard; concurrent success and late responses never clear another request's uncertainty.

Keep runtime as the lifecycle owner and MCP clients as transport owners. Add no kernel command, turn-end hook, automatic tool replay or browser-close action. Explicit reconnect restores transport availability, but the caller must re-observe external state before deciding its next action. Neither cancellation nor reconnect promises rollback.

HTTP session deletion remains best effort with a two-second budget. User-pinned session IDs are attachments and must not be deleted automatically. Remote process termination is outside this contract.

## Alternatives and consequences

Simply abandoning a future leaves an unsafe connection usable. Waiting for the remote result defeats prompt cancellation. Automatically replaying the action can duplicate mutations; invoking cleanup tools can exceed cancelled authorization or close another context. The reconnect barrier instead adds a deliberate recovery step and can invalidate a client after a request that never reached the server: uncertainty is preferable to claiming false safety.

Tests cover pre-dispatch cancellation, queued and dispatched future drops, actual side-effect counts, concurrent HTTP completion, late replies, explicit reconnect and runtime session preservation. Owned stdio teardown closes input and drains output to allow cooperative backend disposal, with a two-second grace period followed by bounded direct-child kill/reap. Drop schedules this cleanup on the existing executor, retaining kill_on_drop if no executor exists or cleanup is interrupted. Native desktop exclusivity, escaped descendant containment and origin policy remain separate boundaries; this decision does not implement them.

## Awaited owner shutdown

CodingRuntime finalization now awaits registry shutdown after stopping its agent and before releasing the session lease or publishing RuntimeExit. Registry closing is distinct from cancel_pending_work (which also serves reload withdrawal). A shared connection barrier covers background and explicit startup, so unpublished candidates clean up before shutdown returns and cannot publish late. Dispatch/discovery and stale adapters fail closed. Published clients close in parallel with an eight-second outer budget; exceeded budgets produce a runtime warning. HTTP session ownership is taken once under its shutdown lock. Drop remains a fallback for abrupt owner/executor destruction; normal awaited shutdown no longer relies on detached Drop work. See the shutdown-barrier plan for executable evidence and limitations.

## Abandoned owned work at turn completion

Controlled-download runtime acceptance exposed a distinction between fencing a client and stopping its owned work: a cancelled unscoped request could remain active and publish a late body even though the client already reported an unknown result. The existing awaited MCP turn-finishing seam now visits unscoped clients too. Healthy/stateless clients do nothing; abandoned unscoped stdio clients drain and retire the captured owned transport before the turn terminal, preserving their uncertainty marker and explicit-reconnect requirement. Lease-negotiated cleanup retains its addressed behavior. No business cleanup tool, automatic replay, second hook or promise of remote rollback is introduced. The existing bounded EOF/kill teardown applies. See [controlled download runtime acceptance](../plans/2026-11-12-controlled-download-runtime-acceptance.md) for the failing control and runtime evidence.
