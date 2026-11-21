> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

# Native App launch callback correction

Baseline AtomCode 6c99543ef, clean isolated checkout. Exact experimental App doctor cold start reproducibly emits a MainActor data-race warning at MacOSAppAgentProxy.swift:102. The installed SDK declares the NSWorkspace completion handler nullable. Existing startup already waits for and verifies the exact App/socket/executable identity. Replace the unused actor-inheriting empty closure with nil, keeping that identity handshake and timeout authoritative. No new task, continuation, lifecycle owner, installed App replacement or permission change.

Regenerate the complete pinned overlay from its existing development source, build from a fresh clean pinned clone, run both native smokes, package a separate new App with measured provenance, then run real App isolation and native lease acceptance. Assert cold-start/proxy diagnostics contain no actor-race warning. OS GUI permission acceptance remains separate. Reconcile old status paragraphs with current committed host and TUI evidence.

## Verification

Clean pinned source product build and both native smokes passed on Swift 6.1.3 / SDK 15.2. All 15 overlay files applied from scratch; modified executable and source closure are measured by build_provenance.py. New separate development App executable SHA-256: 3cbfd17e3d95410c44973ab8c4bd0946645a5ebe74d9a0776ec9ad39e0aa0f31. No existing bundle was overwritten.

Seven actual App/proxy groups, five native lease protocol groups, four packaging refusal groups and the opt-in production registry handover test passed. The same updated App regression fails on the old App with the exact actor-isolation warning, establishing a real regression check. A fresh cold doctor on the corrected App emitted zero stderr bytes, verified its exact permission identity, and still reported both OS permissions false. The owned helper was stopped; no GUI actions were sent. No upstream XCTest success is claimed.

Evidence is retained outside Git in the acceptance scratch directory (launch-fix provenance/isolation/native-protocol/packaging/doctor reports) and build/registry logs. The prior screenshot/grouping Rust source remains unchanged. Full compile gate (including feature variants and negative controls), cargo fmt check, diff check and unchanged-lock checks passed. Forbidden private package names remain zero; Cargo.lock index flag is H.
