# Running session switching implementation plan

**Goal:** Selecting a historical session in the TUI while a turn runs keeps that turn in its existing background runtime.

**Baseline:** release/v5.2.4, c49616dc1. Main checkout has unrelated dirty work; implementation and verification use an isolated worktree.

**Architecture:** CodingRuntime remains each session's sole lifecycle owner. CLI Background owns the existing runtime slots and screen routing. TUI /resume and its picker use the same HostCommand::Resume. No runtime/kernel protocol or storage format changes.

**Failure semantics:** Prepare a hidden candidate using the existing spawn and RuntimeControl resume path before changing the foreground. On any failure stop the candidate and leave the foreground and slots intact. Recheck session identity and movement restrictions before commit. Existing background targets reuse their live runtime. Serialize resume and background slot transitions with the existing operation mutex. Keep sharing, pending approval and reconfiguration restrictions and the 16-slot limit.

## Steps
1. Add end-to-end regressions in crates/atomcode-cli/tests/tui_bg.rs for picker/typed resume during a held turn, background reuse, failed target and approval restrictions. Run them before implementation.
2. Change CLI background.rs to handle resume transitions transactionally; remove only the TUI resume mid-turn guard in commands.rs. Keep ordinary runtime replacement busy checks.
3. Document running view switches, lifetime and multi-process storage boundaries in site/docs/{zh,en}/sessions.html; regenerate search indexes.
4. Run relevant CLI/TUI tests, cargo fmt and gates/compile.sh in the isolated worktree. Review the diff and transfer only this feature's hunks to the main checkout, preserving unrelated work. No commit or push unless requested.

## Verification record
- Before implementation, `resume_while_running_keeps_the_old_turn_and_reuses_it_on_return` failed because the foreground never switched during the held turn.
- Final CLI regression: `cargo nextest run -p atomcode`: 582 passed. Covers typed/picker navigation, simultaneous running sessions, correct cancellation target, missing history rollback, pending input before/during preparation, stale source rejection and shutdown during preparation.
- TUI regression: `cargo nextest run -p atomcode-tui`: 1,717 passed.
- Integrated the concurrent committed WebUI change 118baaa11 in this isolated worktree before the compile gate. It does not change the feature's files.
- Windows terminal behavior remains to be verified manually; automated checks use the headless TUI on macOS.
- Compile gate passed, including workspace/all-targets, feature-gated test compilation and both negative controls. Formatting and diff checks passed; Cargo.lock is unchanged.
