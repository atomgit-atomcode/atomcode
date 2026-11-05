# Static debugging skill auto-loading

Baseline release/v5.2.6 at 8a22de173. Preserve concurrent changelog, recovery-summary,
and mode-model edits. User approved a follow-up feature, not a v5.2.6 release
promise or automatic publication.

Owner: capabilities owns conservative task matching and static expansion. The
existing skills-host/skills rows mount a shared PreStep adapter. It appends the
body to StepDecision.injections, so the existing loop commits it before the first
request. No kernel coding policy, new catalog, transient request mutation, tool
denial or additional lifecycle owner.

Policy: exact opt-in auto-load: debugging metadata; a narrow legacy fallback for
systematic-debugging and atomcode-workflows:debug with a debugging description.
Only one eligible candidate, bounded body, no required arguments or dynamic shell
blocks. Explicit off metadata, skills-host auto_load:false and unquoted user
opt-out suppress automatic loading. Simple translation/rename/format requests and
quoted/fenced reports are negative examples. No claim to understand all implicit
bug reports. Multiple candidates fail open to ordinary model-led selection.

Automatic bodies use Reminder provenance, not Skill: the latter grants
allowed-tools. No shell expansion and no authorization changes. Reuse resource
notes from the existing expander. Deduplicate against the calling agent's retained
model context; after compaction removes the body a later matching turn can reload.
Only first-step real-user messages participate; internal/peer input does not.

Test matcher positives/negatives, quoting, opt-out, ambiguity, static expansion,
no side effects, bounded input/body, first-request visibility before any tools,
per-agent isolation, resume/dedup and unchanged permissions. Run capabilities
skills and harness/coding tests plus workspace compile/fmt gates.

## Verification

- Capabilities (skills + session): 1612 passed, 1 skipped. An existing 1-second
  external-process test flaked once under load, passed in isolation, and passed
  in the subsequent complete run; its source was not changed.
- Harness: 301 passed. Coding: 1241 passed, 9 skipped (two existing 75-second cases).
- Seven production-assembly criteria cover five implicit reports before the first
  task action, no duplicate bodies, unchanged approval, explicit opt-out/simple
  tasks/dynamic snippets, host disable, resume, compaction and agent isolation.
- Matcher negatives were strengthened after a documentation-edit false positive;
  the corrected matcher and full suites pass. No live model/API calls were made.
- Workspace/feature compile gate, rustfmt check, diff check passed. Cargo.lock has
  no changes or private-overlay crypto packages. No version or changelog entry
  was added for this follow-up feature.
