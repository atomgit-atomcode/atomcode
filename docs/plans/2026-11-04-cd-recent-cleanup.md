# Recent directory cleanup

Local skill probes created four temporary workspaces and five real user sessions.
Archive the identified probe sessions before removing them through SessionManager,
under its exclusive lease. Preserve live sessions and unrelated history. Keep test
workspace files intact; session cleanup is enough to remove these recent entries.

The TUI's recent_places reads the native session catalog. Filter entries whose
working directories are no longer directories before applying its five-place cap.
Keep existing directory order, bookmark exclusion and deduplication. Tests cover
removed directories, regular files, valid empty directories and filling the cap.
Do not hide all temporary directories: users may legitimately work there.

Future ad hoc agent probes must isolate product directories as well as cwd.

Validation: 1902 TUI tests passed, including both recent-directory criteria;
compile gate and rustfmt checks passed. The five original probe sessions and one newly created empty session were backed
up and deleted using SessionManager under leases. The last live session was kept
until its process released the lease. No probe indexes remain in the user catalog.
The two ad hoc probe launchers now require explicit fixture configuration and
set an isolated ATOMCODE_HOME. Probe workspace files remain available on disk.
