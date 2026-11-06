# File mention indexes

TUI and WebUI use the same gitignore-aware file index. Repository rules apply
when searching through absolute paths, relative paths or directory symlinks.
The metadata subtree `.git` is pruned before descending; configured agent
folders retain their bounded allowlist behavior.

The TUI builds a shallow view first and finishes in the background. Resetting or
dropping an index cancels its old walk cooperatively at the next yielded entry.
A filesystem operation already in progress cannot be interrupted by this flag.

WebUI's first search for a root collects the complete index on a blocking thread.
Further prefixes reuse those entries. The daemon keeps at most eight canonical
search roots, partitioned by product directory name; evicted indexes are dropped.
Initial builds are serialized per root, without holding the global cache lock
through a filesystem walk.

Both views use a three-second freshness window and background refresh on demand.
A query after expiry starts a refresh while serving the existing entries; a
later query picks up the completed result. This is not a promise that every file
change becomes visible within exactly three seconds. No cache is persisted.

Local debug measurements on the development checkout showed successive uncached
searches at 86/73/76ms and warm cached filters at 0.60/0.36ms, with the same match
counts. Initial cache construction still requires a complete walk (165ms in that
run). These are core-path observations, not release or end-to-end UI benchmarks.
