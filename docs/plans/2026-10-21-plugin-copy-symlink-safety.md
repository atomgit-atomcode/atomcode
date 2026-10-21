# Plugin copy symlink safety

Baseline: release/v5.2.4 at 2cf0f497cff7e806d4b65763fd38490b6b077e14. Preserve concurrent grep/MCP edits.

Capabilities plugin installer owns filesystem copying; installed-plugin state is written by existing install callers only after copying succeeds. Both external local sources and project/local scope copies share the helper. Git cloning and inline user-scope loading remain unchanged.

Reject symbolic links and unsupported special files instead of dereferencing or silently skipping them. Resolve source/destination containment before stale-directory cleanup, rejecting overlap in either direction. Use an iterative walk into a sibling temporary directory and rename only after the copy finishes; failed copies discard temporary data and do not publish a final directory or installation record. This is not a new protection against concurrent hostile source replacement.

Verify self/parent/two-link/dangling/file links, overlap through symlinked ancestors, ordinary files/directories and permissions, failed-copy cleanup, plus a public local-install regression ensuring state remains unregistered. Run plugin tests, capabilities lib and compile gate, fmt/diff/lock checks. No commit/push requested for this fix.

## Result and validation

Implemented shared staged `copy_plugin_tree`, iterative traversal, symlink/special-file rejection, and canonical containment checks before both stale-copy cleanup sites. Four added tests cover self/parent/two-link/dangling/file/root links, aliased destination containment, special files, executable permission retention, unchanged destinations, public local-source install state, and Project/Local failure followed by successful retry.

Installer-targeted suite: 23 passed. Capabilities lib with plugin: 1,236 passed / 1 failed. The independently rerun failure is `plugin::loader::tests::plugin_skill_dirs_use_the_supplied_project_directory`, asserting that `reload_skill_registry` returns no warnings; that loader reads the real user home and does not call the modified installer copy code. Loader/skills are unchanged by this task. All four compile-gate checks, rustfmt check and diff checks passed. Cargo.lock unchanged and private-dependency audit clean. Unix symlink cases ran on macOS; Windows-specific junction behavior was not executed. No commit/push performed.

## Submission audit

User authorized commit/push after audit. At a0ce4c13c8bd800df6297a9f615936cd72225d58, reran installer tests (23 passed) and all four compile-gate checks (passed). Independently checked the loader failure in a detached clean HEAD worktree: the same zero-warning assertion fails without this patch, confirming an existing baseline failure. Temporary worktree removed afterward. Formatting/diff checks and Cargo.lock audit passed. Only installer.rs and this plan are included; concurrent grep/world edits remain separate. Windows junction behavior remains unexecuted.
