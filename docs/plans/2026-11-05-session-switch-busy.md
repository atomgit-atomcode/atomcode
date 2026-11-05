# Session-switch busy feedback

Baseline: c8c2e7e28b975bedc525f486904ecdd040f16254 on release/v5.2.6; unrelated recovery and skill work is preserved.

The embedded background-session owner returned stringified HostError values, so a typed Busy refusal could become RuntimeRejected instead of the existing ActiveTurn wire signal. The session owner and CodingRuntime remain unchanged. EmbeddedSessions now returns the existing HubError, and Background maps HostError::Busy to ActiveTurn. Other failures retain their existing diagnostics.

WebUI recognizes the precise old `coding runtime is busy` diagnostic for compatibility with older daemons. Both ordinary switching and re-enabling synchronization show dedicated bilingual busy guidance. A rejected switch keeps the previous session; re-enabling sync does not proceed on rejection. No automatic cancel, retry, or forced handover is introduced. Read-only browsing remains separate from runtime binding.

Validation: 705 CLI/daemon library tests, 401 frontend tests, TypeScript check, production build, desktop/mobile browser regression with both structured and old string-based busy rejection, subsequent retry, and retained background interaction. Full compile gate, cargo fmt --all -- --check and git diff --check passed.

Release audit: reviewed the exact eight-file scope against 62a9b1cb6 in a clean checkout, excluding unrelated recovery changes. Busy remains an explicit rejection with no handover or cancellation; other failures remain distinguishable. Release commit/push authorized by the user after verification.
