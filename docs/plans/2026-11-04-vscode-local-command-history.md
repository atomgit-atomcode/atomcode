# VS Code local-command history cache repair

Baseline: release/v5.2.6 at de3ea27c2, initially clean worktree. Concurrent UI and scroll changes appeared during implementation and are outside this repair.

The session store owns saved UI-only command records. The extension owns transient history caches. A pending history read can already contain a saved local command when `_persistLocalCommandTranscript` appends the same command pair to its cache, producing four displayed rows from two stored rows. Controlled tests reproduce this in the actual provider. Restoration does not execute these commands. Repeated disk entries at different anchors are not established as duplicates by this reproduction.

After saving a local command, retire the older history read and reread the saved history using the existing guarded panel loader. Refresh both extension caches without replacing the currently visible transcript. Window initialization follows a replacement history promise if its original read was retired. Preserve generation and panel ownership checks; do not update a replaced runtime or revive a closed panel. Read failures leave invalid caches available for normal history-load retry and do not repeat the successful write.

Do not deduplicate by text: consecutive real `/login` or `/reload` executions must remain distinct. This repair changes neither the session formats nor existing disk records.

Verification covers history-before-save, history-after-save, retired late snapshots, window initialization, concurrent saves with reversed response order, closed panels, read failure/retry, and consecutive identical commands. Run the VS Code webview suite and extension build, diff/format checks and the repository compile gate. Windows VS Code interactive reproduction remains unverified.

Validation completed: full VS Code webview regression runner (including the provider completion marker), extension TypeScript compile/webview build, repository compile gate, cargo fmt check and git diff check passed.

Pre-commit review at 0ac2d300b: repeated the full extension regression suite and build; checked stale-read ownership, window initialization, closed-panel behavior, format/diff checks and clean Cargo.lock. The previous compile gate remains valid because the change only touches extension TypeScript and this document.
