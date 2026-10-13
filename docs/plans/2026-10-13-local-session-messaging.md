# Local Session Messaging Implementation Plan

**Goal:** Same-user, same-machine online text messaging between independent AtomCode terminal sessions, defaulting to hold.

**Architecture:** CLI host owns discovery, transport, admission and pending messages. CodingRuntime remains the only lifecycle owner; delivery uses its existing note API and native peer facts. Host plugins expose tools, while the TUI command resolves the currently displayed runtime by session identity. No new kernel command, session format or persistent mailbox.

**Tech Stack:** Rust, Tokio Unix sockets, serde JSON, existing SHA-256 and runtime APIs. macOS/Linux first; Windows explicitly unavailable.

## Scope and invariants

- Instance UUID plus exact session id addresses a target; session switches invalidate pending messages and stale addresses.
- Socket directory and peers are owned by the current uid, private and non-symlinked. Bounded frames, connection timeout, queue size and admission rate.
- Models see list_agents and send_message. Sends are risky tools governed by the sender's existing tool approval. Messages are peer context, never human approval; received slash syntax is plain text. Existing recipient permission checks remain mandatory.
- accept/hold/refuse is runtime-local. Default hold; only the local terminal command approves a held message or changes mode. Remote wire has no approve, mode or execution operation.
- Online-only. No offline replay, automatic retry, durability guarantee or promise of model completion. Receipts distinguish held, runtime accepted, refused and uncertain; source error details are preserved.
- Default hold prevents automatic turn loops; accept is an explicit local opt-in. Bounded rate/queue prevents unbounded arrivals.
- Runtime event pump owns the socket guard. Shutdown unregisters it; provider rebuild keeps the same service, pending messages retain their session binding.

## Tasks

1. Create CLI peers transport and protocol; test uid/permissions, malformed frames, size, stale target, socket cleanup, rate and queue limits.
2. Implement admission state and delivery adapter using RuntimeControl weak references plus CodingRuntimeHandle::note. Test held/approve/deny and identity switches, and ensure no pending approval can be answered by messages.
3. Add host runtime plugin with list_agents/send_message; wire native CLI startup and runtime attachment; keep unavailable drivers explicit.
4. Add /peers list, pending, approve, deny and mode commands, resolving the current AgentClient session; bilingual wording and visible held notifications.
5. Exercise two real endpoints and fake/real runtime delivery; run affected CLI tests, formatting and compile gate. Commit only on feature branch; do not merge or push to release.

## Delivery status (2026-10-13)

Implemented on `feat/local-session-messaging`, based on `6727dd4c1426c30ce21ae66e15e750d75a603ea1`, in a separate worktree. All five tasks completed. The runtime seam is `note_to_session`: the existing lifecycle owner checks the target session before its normal peer-note path. Tools require the caller's own root session identity; child agents cannot impersonate the terminal session.

Validation:

- Peer transport/host tests: 17 passed, including two independent OS processes, sender tool approval and deny, and uncertain receipt caching.
- Affected CLI/TUI/coding suite: 3194 run, 3188 passed, 6 failed, 10 skipped. All six failures reproduced on the unchanged baseline with the matching `atomgit` feature: differential `a_write_outside_the_workspace_diverges_and_here_is_why`, differential `an_always_covers_the_scope_not_just_the_one_call`, permission_grants `always_allow_grants_survive_reassembly`, product_identity `the_agent_says_it_is_the_product_that_started_it`, runtime_criteria `an_always_allow_survives_an_undo`, and tool_args_repair `approval_sees_the_repaired_arguments_that_execute`. No golden files changed.
- `bash gates/compile.sh`, `cargo fmt --all -- --check`, and `git diff --check`: passed.
- Cargo.lock unchanged and checked for private dependency contamination.

Not verified on a Linux host or through manual two-terminal UI use; macOS subprocess/socket integration and real host runtime tests cover the implementation. Windows/classic/ACP/headless remain outside this first version. No release merge or push.
