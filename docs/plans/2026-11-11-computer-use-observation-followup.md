> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

# Computer Use observation follow-up

Baseline: release/v5.2.7 at 685ef5751. Existing runtime/recovery/truncation work is dirty and belongs to other ongoing work. This follow-up owns only the Computer Use skill and its documentation; no runtime state owner, protocol, persistence format or approval rule changes.

Real-model acceptance found that display-escaped accessibility text was treated as literal document text. An immediate save/action snapshot was also mistaken for final durable state. The transport and approvals worked; the first autonomous restoration was not correct. Explicit operator correction and an independent disk comparison eventually restored the controlled file.

Harden the installed skill at its owning boundary: distinguish JSON/tree display escapes from actual characters, use a fresh screenshot for line layout, demonstrate one JSON newline escape, preserve genuine user backslashes, and avoid editing already-correct content just because tree text is ambiguous. Separate content observations, modified flags and disk durability. After an authorized save, get a new window observation, with at most two additional read-only observations; report uncertainty instead of claiming unsupported byte equality or starting another editing method.

Verification uses the existing compiled AtomCode with a freshly loaded copy of this skill, the configured real vision-capable model, normal single-call approvals, and only the uniquely named TextEdit test document. Repeat the natural task without an operator-provided escaping correction. Inspect decoded tool arguments for real line feeds, verify the new observation, and independently compare the final test-file bytes. A successful run supports the guidance change, not a guarantee of unattended GUI reliability. No expected image answer is sent to the model and no credentials are printed or stored in the test config.

The review also identified the setup updater's exact-current-body check: an installed original v1 skill would be mistaken for a local edit after this asset change. Recognize only the SHA-256 of the actual d8be8b9bf body, keep that historical body as a test fixture, and permit a one-way update/removal of that exact known body. Never infer ownership from a name, frontmatter field or partial match. Modified old bodies and auto-load opt-outs stay protected; the MCP config still requires its existing integration ownership check. Tests cover successful old-to-new upgrade, no-overwrite on modified old bodies, and removal on explicit disable. No dependency change is needed (the CLI already uses sha2).

Verification runs in an isolated worktree at the recorded baseline plus only this follow-up's files, so concurrent runtime work is neither formatted nor mixed into the change. Its tests/compile/format results do not claim validation of unrelated current dirty runtime edits.

## Verification results

- Isolated baseline 685ef5751 plus these changes: 345 CLI library/setup tests passed; gates/compile.sh and cargo fmt --all -- --check passed.
- Real installed v1 skill upgraded through the new executable's computer enable command (exit 0); resulting skill exactly matches the current bundled asset. Modified originals and opt-outs remain protected by tests.
- Natural real-model edit/restore run sent actual LF characters (no literal backslashes), confirmed content through fresh observations, and independently restored the original file bytes. An external AX -25204 error occurred after the value changed; the model reobserved instead of blindly repeating the write. Save observations remained ambiguous; the model bounded its checks and reported uncertainty.
- Initial testing exposed mixed app/window observation context. The final guidance now requires list_windows then get_window_state. A fresh real-model read-only session followed this sequence, obtained the correct document and screenshot, and completed without mutations. Its first text_limit argument was invalid; it corrected that argument on retry.
- These are model instructions, not a hard runtime guarantee. Backend AX partial failures and ambiguous modified-state reporting remain external limitations; no claim is made that all models or GUI applications behave identically. The follow-up commit scope excludes unrelated dirty runtime work.
