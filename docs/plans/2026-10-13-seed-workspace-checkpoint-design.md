# Seed new checkpoint indexes from ordinary repositories

A new session has a separate shadow store. Incremental capture fixes later turns but its first capture still hashes every selected file. Seed the shadow stat cache from the ordinary repository index before the first capture, then use the existing incremental reconciliation and safety filters before writing any checkpoint tree.

WorkspaceCheckpoint remains the owner. Seeding runs inside existing capture locks and disk/budget checks. It reads one open source index, copies it to index.seed, preserves its modification time for Git's racily-clean detection, validates readability with Git, and installs it as the shadow index. The source index, branch and HEAD are never changed. Existing object alternates supply the ordinary repository's objects.

Restrict seeding to ordinary .git directories with a regular index. Worktrees, split indexes, sparse checkouts, unmerged entries, assume-unchanged/skip-worktree flags and unavailable copies retain full capture. Copy failures are best effort; capture failures remain explicit. A copied cache-tree cannot bypass filtering: disallowed entries are force-removed before write-tree. No tree is published during seeding, no cross-session cache is introduced.

INFO phase seed_index indicates a successful seed; DEBUG index_changes shows how many paths actually need updating. Per-phase timings remain INFO only at one second or more; file-count details are DEBUG. Windows must be tested by starting a new session each time. Mac session tests cover staged vs working contents, tracked sensitive paths, user index preservation, modification time preservation and split-index fallback. No Windows latency claim precedes measurement.
