> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

> Current delivery status: [completion audit](2026-11-11-computer-use-delivery-audit.md). Progress paragraphs below preserve their historical baseline. Host scope wiring, isolated App/socket construction, activity grouping and clickable result pictures were implemented afterward; GUI and execution-policy acceptance remain incomplete.

# Desktop turn lease: execution-boundary findings

Status: execution-side macOS protocol prototype is implemented and separately tested; AtomCode host integration and actual GUI exclusivity are not complete or certified.

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

The available host is macOS 14.5 with SDK 14.5. The backend tag declares swift-tools-version 6.2; installed compilers are 5.10 and 6.1.3. No unsupported manifest downgrade or new backend build has been claimed as accepted. An explicitly modified Swift 6.1.3 build baseline is now verified by the experimental patch described below. Host wiring and actual GUI acceptance remain work, alongside browser origin enforcement, activity presentation and other-platform acceptance.


## Experimental backend implementation

`integrations/open-computer-use/` now carries the pinned MIT source patch, a build script and an opt-in real-process acceptance harness. It does not bundle native binaries or activate the patch in current sessions. Backend support is negotiated as experimental protocol v1; the owner metadata is separate from model tool arguments. The OS lock belongs to each actual server connection in the shared App, and only matching addressed release clears its observation caches. Cache clearing preserves the monotonic screenshot counter. Negotiated legacy app actions require explicit prior observation instead of silently refreshing stale indices. Legacy clients remain compatible but outside cooperative exclusion.

Verified on macOS 14.5 / SDK 14.5 / Swift 6.1.3:

- Fresh clean v1.3.0 checkout accepts the patch and builds the native product. All eight patched source files match the development source. The script refuses dirty source without changing it.
- An XCTest-independent native smoke validates screenshot cache invalidation, non-reuse of prior screenshot identities, explicit-observation refusal for a phantom app target, same-process lease exclusion and stale-owner refusal. No GUI action is dispatched.
- Five real-process acceptance groups cover different product homes, reentrancy/wrong owners, metadata injection refusal, strict version negotiation/downgrade refusal, normal EOF, native process death, symlink defenses and owner byte bounds. Private shared-App connections exclude one another and direct processes; one connection may disconnect without terminating the shared App or its neighbor.
- No installed App, user MCP configuration or OS permission was replaced or changed. The direct test binary reports Accessibility=false; original XCTest targets remain unavailable on this Command Line Tools installation. Actual GUI in-flight cancellation and window handover are not accepted by these protocol tests.

Before activation, the host must still propagate the real agent/turn identity, release via the existing awaited TurnFinishing seam, handle unknown outcomes, and verify a lease-capable backend. An isolated App/default-socket identity is also required: raw development binaries can otherwise resolve the installed App through upstream discovery. The harness intentionally uses direct protocol mode or a unique private App socket to avoid that path. Do not enable this raw test executable as a production desktop backend.

Prototype delivery checks: final clean-source product build plus native smoke passed; all eight patched files match the development source; five real-process groups passed on the reproduced executable, including strict successful EOF and actual SIGKILL cases. Shell/Python syntax, whitespace, cargo format, full compile gate (feature variants and negative canaries), and forbidden-lock-package checks passed. The only new integration payload is text source/patch/tests/license/docs; no Rust dependency or native binary is added. ADR 0032 records this boundary without claiming host or GUI completion.
