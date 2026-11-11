# Desktop turn lease: execution-boundary findings

Status: design evidence; desktop mutual exclusion is not implemented or certified by this document.

## Verified baseline

AtomCode: release/v5.2.7 02d478966. Local Codex: /Users/theo/Documents/workspace/codex at 806d9732c9. Codex bundled_hooks.rs routes Stop, Interrupt and SubagentStop to connector-specific turn_ended operations. This is a cleanup contract for a particular backend, not a reusable desktop lock.

The installed Open Computer Use package reports `1.3.0 (build 97, commit e551c13)`. Its exact tag source was checked at e551c13e7082ce4c097234137582fbf4a1bc647a, outside the product repository; no backend binary, npm package or clone is added to AtomCode.

Relevant source at that commit:

- apps/OpenComputerUse/Sources/OpenComputerUse/MacOSAppAgentProxy.swift: the stdio process forwards requests over a Unix socket to a shared macOS App agent. Each socket connection has its own StdioMCPServer, and requests run through AppAgentEnvironment.withOverrides. The latter serializes individual requests with NSLock; it does not reserve the desktop across observation/action/model round trips.
- packages/OpenComputerUseKit/Sources/OpenComputerUseKit/MCPServer.swift: notifications/turn-ended resets the visual cursor. It does not acquire/release a turn lease or clear observation caches.
- packages/OpenComputerUseKit/Sources/OpenComputerUseKit/SoftwareCursorOverlay.swift: the CLI turn-ended command broadcasts a distributed notification with no connection owner identity. Calling it as AtomCode cleanup could affect another client's cursor; it is not an acceptable ownership boundary.
- MacOSAppAgentProxy.swift: terminating a stdio proxy does not terminate the shared App agent. An already forwarded operation may still be executing in that agent. AtomCode must not terminate the shared App to reclaim another client's resources.

Source reference: https://github.com/opensymph/open-computer-use/tree/e551c13e7082ce4c097234137582fbf4a1bc647a

## Required lease contract

A full desktop turn spans fresh observation, model thinking/approval, actions and verification. The execution backend must retain the resource until its already accepted work actually finishes. A frontend-only Mutex, per-call file lock or process-lifetime lock cannot satisfy that contract: the first two allow interleaving between calls; the last reserves idle sessions, and killing a proxy does not prove the remote App stopped.

The next implementation must provide an opt-in backend-owned, connection-scoped lease:

1. Acquire a private per-user OS file lock before the first desktop tool in a turn. Busy is an explicit tool failure, not a wait or permission grant. Never unlink a live lock file or rely on a PID file as authority.
2. Bind an opaque host-generated turn owner to that connection. Two agents sharing one MCP registry must not inherit each other's authority. Supply owner metadata outside model-controlled tool arguments. Generic/unscoped direct calls must fail closed when lease mode is enabled.
3. Release only the matching owner on an addressed end-of-turn operation, after prior requests finish. Disconnect releases the lease from the backend connection after any executing request completes. Host cancellation must not prematurely unlock an operation still running in the shared App.
4. Clear the released scope's cached observation references; a new owner must observe its selected window again before acting. No global turn-ended broadcast or shared-App termination is used.
5. Reuse AtomCode's existing TurnFinishing and runtime shutdown boundaries. Do not add a second agent owner, kernel coding command, second session writer, or parallel turn-end state machine.
6. Advertise and verify the backend lease capability. Existing 1.3.0 cannot silently be described as supporting it; unknown/old backends cannot get a fabricated safety flag. A compatibility path must remain honest about single-client support.

This is cooperative exclusivity between participating agent clients, not a sandbox against a malicious local user or a promise to undo GUI effects. Other apps and the human can still change the UI; post-action observation remains required.

## Acceptance still required

Use two actual processes (including different product directories), two agents within one runtime, normal turn end, approval waiting, cancellation after a real side effect, forced client death, stale owner cleanup, replacement/reload and shutdown. The second owner must not act until the first backend connection has finished its accepted work. Verify fresh-observation requirements after ownership changes, and keep unrelated backend connections alive.

The available host is macOS 14.5 with SDK 14.5. The backend tag declares swift-tools-version 6.2; installed compilers are 5.10 and 6.1.3. No unsupported manifest downgrade or new backend build has been claimed as accepted. Backend extension/build and host wiring remain work, alongside browser origin enforcement, activity presentation and other-platform acceptance.
