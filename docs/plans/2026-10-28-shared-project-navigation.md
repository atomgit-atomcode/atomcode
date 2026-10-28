# Shared WebUI/App project navigation

Baseline: release/v5.2.5 at d528d34d8. Worktree initially clean.
The ordinary TUI /resume remains project-scoped (2026-10-27-resume-project-scope.md).
WebUI lists all projects, but shared navigation used to prepare its candidate in the
old working directory; the candidate's /resume then rejected the foreign bucket and
the browser reverted to the old session.

EmbeddedSessions::switch is explicit project navigation. Under the existing host
operation lock, resolve its catalog target and prepare the existing candidate runtime
in the target project directory. Check the directory's bucket before preparation;
never pin the old directory to a foreign bucket. Current-project sessions keep the
current directory, including renamed/aliased projects. Newer formats and unavailable
or mismatched target directories fail before changing the foreground. Existing runtime
lease handoff, busy-session fork, sharing follow, generation fences, cancellation and
candidate shutdown remain the owners of the actual transition.

The person clicks a session once; no separate directory-changing request is sent, so
failure cannot strand the user on a newly created blank session. Standalone daemon
navigation already restores the target directory; no HTTP schema changes are needed.

Regression: cross-project WebUI entry switches both session and directory and shows
the target history in TUI; switch back; leased foreign target forks within its own
project; missing target directory preserves the old foreground without preparation.
Existing ordinary TUI cross-project rejection must remain unchanged.
