# Configurable, bounded file discovery

Baseline: release/v5.2.4 eb5ced1f8. Only untracked preview artifacts are present; preserve them. Code/brainstorming/writing-plans workflow: the approved design separates direct reads from discovery, adds independent ignore/hidden/link switches, and bounds scans.

Owner: capabilities FileSystem implements traversal and containment; Glob/Grep own matching and presentation. Config owns persisted [tools.search], coding maps it into the existing tool-search row, and harness mounts configured instances. Driver/runtime/session ownership is unchanged. Keep existing walk consumers (including search_replace) unchanged; add an options-aware traversal with an explicit unsupported-policy error for worlds that cannot honor alternate filters.

1. Add shared discovery options and scan reports, bounded local traversal, per-entry containment, cancellation, cycle/error accounting and throttled progress. A completed empty result and an incomplete scan are different outcomes.
2. Expose respect_gitignore/include_hidden/follow_symlinks per invocation, preserving defaults. When ignore processing is disabled allow build/dependency directories but continue excluding VCS metadata and product internals. Preserve credential-path protection on canonical link targets.
3. Persist defaults and budgets in tools.search; carry through CodingRuntimeConfig/config_rows and the existing harness search plugin. Document direct read first for known filenames and scoped expansion after an empty filtered search.
4. Test ignored/hidden files, valid and broken links, loops, fenced external targets, entry limits, timeout/cancellation, credentials, config round trips and real assembly. Windows symlink/junction tests should run conditionally; lack of symlink privileges must not hide non-link tests.
5. Run affected crate tests, formatting and compile gate. Do not change version/lock/private crypto overlay.
