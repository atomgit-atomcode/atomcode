# Skill invocations with images

Baseline b6183b2be, release/v5.2.6. Preserve concurrent reasoning-loop edits.
Invoke gains optional images (old wire requests default empty). Catalog skill
preparation validates and expands without side effects, producing one contextual
user input. Coding runtime submits that input through its existing generation,
session, image-preprocessing, cancel and receipt path; skill-origin context stays
atomic with the user message. No second submit or mutable attachment mailbox.
The standalone harness uses the same preparation with its ordinary inbox.
TUI forwards images for actual skill/prompt commands, including /skills and $
aliases, and retains the ordinary management-command image warning. Test actual
provider input, legacy text commands, refusal, cancellation and UI routing.

Implementation uses optional Invoke.images and CommandDescription.accepts_images.
Only image-bearing lead commands enter submit_catalog; member commands retain
their native inbox route. The runtime resolves/validates the skill before opening
a turn and attaches its Skill-origin context atomically after ordinary vision
preprocessing. The text-only injection contract remains unchanged. CLI queues
synchronously and waits off its command pump so Cancel cannot be blocked by VL.
ACP forwards images only for commands advertising support; management commands
retain their existing refusal behavior.

Validation: five affected crates passed 4326 tests (9 skipped); final focused
regressions passed 8/8 after the routing refinement. Actual provider input,
headless TUI three skill spellings, native images/VL fallback, receipts, cancel,
no next-message leakage, legacy wire defaults and text command compatibility are
covered. Workspace/feature compilation gate and formatting/diff checks passed.
Linux physical-terminal testing has not been performed. Concurrent reasoning-loop
changes were preserved and became commit 17159e4ec while this work was in flight.

Pre-push audit found no blocking issues. Rechecked the unchanged-code full and
focused test results, compilation gate, formatting, receipt ordering and protocol
compatibility. Cargo.lock is unchanged and the prohibited package counts are zero.
