# Incremental workspace checkpoint index

Windows capture logs show 13,096 selected paths and 24 seconds in update-index, followed by another 5.5 seconds for the after snapshot. Resetting the shadow index loses Git's stat cache on every capture.

Keep the shadow index under the existing WorkspaceCheckpoint thread and process locks. Enumerate the same source tracked/untracked files and apply the same sensitive, directory and size filters. Compare the selected set with the shadow index: force-remove entries no longer selected, and update only newly selected or diff-files-reported changed paths. Git retains responsibility for stat-cache and racily-clean detection; no application mtime cache or tree reuse shortcut is introduced. write-tree still validates every resulting tree.

All capture callers, including rewind recovery and restore precondition checks, use this implementation. Errors remain explicit; a partial index update is reconciled against the current worktree by the next capture. Existing tree objects and rewind facts remain authoritative, never the mutable index. User Git index, HEAD and branches are untouched.

NUL-delimited stdin preserves literal filenames and avoids Windows command-line length limits. Log index_changes with updated_count and removed_count without path or content data. Empty changes skip update-index. First capture still hashes selected files; this step primarily improves subsequent captures. No claim of Windows speedup without measurements.

Validation: session tests including same-size rapid edits, deletion/recreation, newly ignored untracked paths and re-inclusion, user index preservation, existing restore/compensation, sensitive filtering and crash recovery. Run format and compile gates before delivery.
