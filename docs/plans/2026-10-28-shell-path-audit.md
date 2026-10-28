# Audit of merge c26d6574 (!1157)

Baseline: release/v5.2.5 at bab293d15. Existing changelog edits are unrelated.
The merge introduces background separator checks and memory provenance/projection
fixes. Shared shell path policy belongs to capabilities; event log projection stays
in kernel and snapshot import stays in capabilities. No new lifecycle owner or
persistent format is introduced by this follow-up.

Reproduced two missing sensitive-read decisions: an input redirect in an intermediate
working directory, and a parent-relative read following a subshell cd. The syntax-only
invocation list flattens scopes, so a single mutable cwd is not authoritative.
Keep the starting directory and each possible literal cd base; inspect operands and
redirects against the conservative set. Include canonical bases for symlinked dirs.
Unresolved destinations, repeating control flow with cd, or over 64 candidate bases
require approval. This may produce extra approvals for ambiguous shell constructs.
Bare literal operands are candidates too: a filename without an extension can be a
symlink to a credential. Option and expansion patterns remain excluded from literal
resolution; other shell approval/classification layers still apply.

Regressions cover intermediate redirects, bare symlink operands, subshell cd, unknown
cd targets, repeated cd, and an ordinary non-sensitive cd/read. Review existing
compaction provenance, snapshots, visibility, rewind/undo and recovery through the
kernel, capabilities(session,memory) and harness suites, plus the compile gate.

Review validation: capabilities with session,memory: 1542 passed, 1 skipped;
harness: 291 passed. Additional while/function cd cases passed. Compile gate
passed. Added cfg(unix) to the symlink regression so Windows does not compile
Unix-only APIs. Windows execution was not tested on this macOS host.
