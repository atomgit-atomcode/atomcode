# WebUI historical images and current session visibility

Baseline: release/v5.2.7 / 6efa2d988, initially clean. While working, unrelated TUI paging edits were committed as 5be8313ff; preserve those changes. Reviewed native persistence, preprocessing, receipt claims, transcript/history paging and recent Git history.

The screenshot's native user fact contains a VL caption but images=[], with no image sidecar or image-bearing UI cache. The bytes cannot be reconstructed. Inline history was rendering raw TurnRecord.user rather than the normal user display projection.

User clarification: old unavailable pictures should retain only [Image #1]-style labels, with no unavailable-image warning. Future pictures, including TUI→WebUI, must render as images.

Implemented boundaries:
- Display-only transcript projection separates prompt text from Chinese/English VL annotations, carries optional user_images and omits payloads from navigation summaries. Frontend compatibility handles older servers; raw facts/model context are unchanged.
- Runtime stages VL-stripped originals under an existing driver receipt (or an internal receipt hidden from product events). The session-store row associates InputClaimed with the next matching committed user fact. Withdrawal, changed text, turn termination and generation drop release pending data safely.
- SessionManager writes one bounded atomic file at <id>.display-images/<seq>.image under the active lease. A text digest prevents reuse against a changed fact. These files never enter resume or model messages, are copied with verified fork facts, and are removed with their session. Attachment failures are auxiliary warnings, not uncertain conversation commits.
- HTTP history, live snapshots and full transcript details use exact source identities. Legacy ordinal image sets are used only when cardinality matches, avoiding attachment shifts over old TUI records. Already-captioned daemon input is not recognized twice; /chat preserves originals until the runtime captures and strips them.
- Active session and its project are prioritized only in sidebar display order, leaving timestamps and peer order intact.

Validation covers preserved original bytes versus text-only provider input, reload and fork, source identity/digest checks, cancellation/repeated prompts, sidecar deletion, legacy ambiguity, index payload bounds and stable sidebar ordering. Run affected nextest, frontend tests/typecheck/build, compile and fmt gates. No old originals are fabricated and no existing user sessions are rewritten.

Final validation (2026-11-08): 80 image/receipt/transcript regressions passed, plus 22 storage/fork/delete checks. The final explicit-project-bucket display path passed 28 daemon display/image tests. WebUI 418 tests, typecheck and production build passed (existing chunk-size warning only). bash gates/compile.sh passed including feature compilation and negative controls. No real user session was rewritten.

Additional exploratory check `cargo check -p atomcode-capabilities --no-default-features --features session` fails in existing unmodified world.rs and session/snapshot.rs optional dependency gating (tools/tokio_util/tracing and follow-on errors). These files and Cargo feature declarations are unchanged; the required workspace/feature compile gate passes. This independent minimal-feature limitation is recorded rather than expanded into this fix.

Final review: no blocking findings. Receipt claims/withdrawals are restricted to the owning session (team members cannot claim its image bytes); six scope/storage/runtime follow-up tests passed. The final delivery compile gate and fresh fmt/diff/Cargo.lock checks passed. Current baseline 5be8313ff matches origin/release/v5.2.7; all staged changes belong to these two requested behaviors.
