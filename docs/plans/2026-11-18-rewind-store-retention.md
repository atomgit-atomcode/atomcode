# Rewind store retention

Baseline release/v5.2.8 at 3d3aaa1aa; two unrelated docs/plans edits remain in the shared worktree.

Report: a user's `~/.atomcode` grew about 112 MB every few minutes, projected to 70 GB a month. Locally `rewind/` held 609 stores (731 MB), 413 of them for one project, none ever removed.

Cause in the code: each session has its own shadow store, `rewind/<worktree bucket>/<session id>`. `STORE_BUDGET_BYTES` (500 MiB) bounds one store only. Untracked files up to 5 MiB are not shared through alternates, so each new session stores them again. `SessionManager::delete` removed the session's files but not its store.

Owner and boundary: the store is auxiliary evidence for `/rewind`; the event log stays the session authority. A missing store already quarantines its points (`SnapshotHook::new_with_context`, 938f11b90), so removal never fails or rewrites a conversation.

Change (`session/rewind_retention.rs`):

- `SessionManager::delete` removes the session's stores in every rewind bucket (best-effort, under the lease the delete already holds), named by the leased id, never by the id an event header claims. The worktree bucket can differ from the session bucket when a session starts in a subdirectory, so stores are found by session id.
- Opening a session's store starts one background sweep per process: keep the 20 most recently used stores per project, then evict least recently used until all stores fit 2 GiB. The opening session is never swept. A store with no session files is removed only after 24 hours. Only directories carrying `atomcode-rewind-version` are touched.
- A store owed a crash recovery is never removed: a `<id>.rewind.txn` / `.rewind.txn.json` journal beside the session, or a `refs/atomcode/recovery/*` pin in the store (loose or packed). For a session's store this is checked under its lease, so no restore can start in between. Each check is three-valued: a journal, ref or session directory that cannot be read (permission or I/O error) counts as "cannot tell" and keeps the store; only "not found" lets it go, and an unreadable session directory never turns a store into an orphan.
- A candidate is re-checked before it is moved: under the session's lease (or, for an orphan, just before the move) its last write time — `index`, `packed-refs`, `refs/atomcode`, the store directory — must still equal what the scan ranked. A session resumed, captured and closed between scan and eviction is skipped and ranked again by the next sweep.
- Paths: only ids `WorkspaceCheckpoint::for_session` accepts (ASCII alphanumerics, `-`, `_`) become store paths; buckets are real directories (a symlinked bucket is never followed); before removal the store's bucket must canonicalize directly under the rewind root.
- Removal renames the store into `rewind/.trash/` (one rename under the session's lease, so a racing resume waits for that rename only) and deletes it after the lease is released. The trash is used and emptied only as a real directory under the canonical rewind root (a symlink or file there is refused), and emptying removes only real directories still carrying the store marker, finishing what an interrupted run left.

Effect: a swept session can still be resumed and its conversation rewound; only restoring its files is lost. On this machine the first sweep would drop about 478 old stores; walking all stores took 1.6 s, off the session's path.

Not done: one shared store per project (the root fix, as opencode does), a lower untracked-file cap, and a user-facing `gc` command (as omp has).

Verification: 16 retention tests (including a store written after the scan, a store owed a recovery, unreadable recovery state and session directories, a symlinked bucket, a symlinked trash, unmarked trash entries, ids that are not store names, an interrupted delete), `delete_removes_the_sessions_rewind_store_in_any_bucket`; atomcode-capabilities (session) 1835 passed; atomcode-coding 1387 passed; `bash gates/compile.sh` passed in an isolated worktree holding only this change (the shared worktree carries someone else's unfinished `worktree_status` edits that do not compile); fmt check and diff check clean.
