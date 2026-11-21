> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

# 0032: Desktop turn leases belong to the executing connection

Date: 2026-11-11
Status: Experimental macOS backend with negotiated stdio host integration; isolated App/proxy no-GUI acceptance verified; GUI/platform acceptance pending

## Context

The verified Open Computer Use 1.3.0 macOS stdio process forwards to a shared App agent. Terminating the proxy cannot prove that an already forwarded request finished. Its existing turn-ended operation resets a visual cursor and is not a lease. A per-call mutex permits observation/action interleaving; holding a process-lifetime lock reserves idle sessions. Neither satisfies turn ownership.

## Decision

Put the cooperative OS file lock on the actual backend server connection. Negotiate experimental protocol v1 and bind its lease to a bounded opaque host turn identity carried outside model arguments. Another process or another turn on that same connection gets an explicit busy failure. Only matching addressed release clears observations. Preserve monotonic screenshot IDs when clearing caches; negotiated legacy app actions must not silently refresh and act using old indices. Unaddressed broadcasts do not release a lease.

Connection destruction releases its file descriptor after that connection's synchronous request processing has returned. This is the chosen execution boundary, not a promise to undo GUI effects or freeze an application after a reply. The host must use its existing awaited TurnFinishing/shutdown seams and the real executing agent/turn identity. No kernel business command, second runtime owner, session log or independent end-of-turn state machine is introduced.

Keep the backend optional and separately built. Store only a pinned MIT source patch, reproducible build recipe and protocol/smoke tests in the repository. Do not replace an installed App, grant permissions, terminate a shared App or activate an unverified backend silently. Legacy nonparticipating clients remain compatible but outside cooperative exclusion; this is not a local-user sandbox.

## Verification and limits

A fresh pinned checkout builds on the recorded Swift 6.1.3 / SDK 14.5 baseline with compiler-checked compatibility changes. Native smoke verifies cache invalidation, non-reuse of screenshot IDs, explicit-observation refusal and in-process exclusion. Five real-process protocol groups cover profiles/processes, owners, negotiation, EOF/death, file defenses and private shared-App connections/neighbor survival. Dirty-source refusal preserves source. Original XCTest targets were not run because the local SDK lacks XCTest.

These tests do not prove actual GUI in-flight cancellation or window handover. Host scope propagation and awaited normal/cancel lifecycle wiring are implemented and tested through the production runtime. Scope identity is resolved from the actual executing realm and its committed TurnStart sequence; no model argument or publication-time lead context supplies authority. Negotiated mutation RPCs cannot cross a recovered connection, and cleanup errors cannot poison a replacement client. Isolated App/default-socket identity and no-GUI proxy acceptance are implemented; verified dead-owner records prevent full socket backlogs from being mistaken for stale listeners. Negotiated observation IDs also include per-service random identity to prevent cross-connection reuse. Real GUI permission/cancellation acceptance and other platforms are not completed. The prototype is not enabled in the current product. Evidence and remaining criteria are in the desktop-lease execution-boundary plan and integrations/open-computer-use/README.md.
