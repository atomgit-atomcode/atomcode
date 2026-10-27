# Project-scoped interactive resume

Baseline: release/v5.2.5, 119e7cbee; clean worktree before this change.

The picker previously fell back to the global catalog for an empty project, and
interactive resume could pin that project to the selected foreign bucket. This
confused a new directory with a renamed directory. If the current project already
owned sessions, the pin was skipped, but a busy foreign session could still be
forked before runtime lease validation rejected the mismatched bucket.

Keep the native session catalog and CodingRuntime as the respective storage and
live-state owners. Filter the picker by the resolved project bucket, with no
empty-project global fallback. Existing project markers continue to identify
renamed projects. Do not add an all-projects picker.

Interactive disk resume obtains the runtime working directory successfully and
rejects a foreign bucket before preparing or forking anything. It no longer
mutates project identity. This guards both direct idle resume and the hidden
candidate used by running/shared foreground navigation. Existing live background
navigation remains unchanged. Explicit catalog APIs without a directory retain
their global listing semantics.

Tests cover empty-project filtering, storage-bucket priority over recorded paths,
marker identity after rename, and a leased foreign session refused without a new
fork, foreground switch, or project rebinding. Removing the automatic pin helper
also removes tests that required that superseded behavior. This does not delete
previously created forks or attempt speculative cleanup after unrelated uncertain
runtime commits.
