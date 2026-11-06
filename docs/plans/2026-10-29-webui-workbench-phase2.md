# WebUI workbench phase 2

Goal: edit/save existing UTF-8 files, preserve session-scoped drafts and positions,
warn before discarding unsaved work, and show read-only Git status and diffs.

Owner: daemon-local filesystem/Git services, outside CodingRuntime and kernel.
The existing native session/live binding supplies the workspace. No new agent
lifecycle, model calls, session persistence format or Git mutation API.

1. Add bounded versioned text reads and optimistic saves. Serialize browser
   writes in this process, compare original SHA-256 immediately before replacing,
   preserve permissions and write via a same-directory temporary file. Return
   409 on conflicts and leave the draft intact. This detects observed external
   changes, not an atomic cross-process compare-and-swap against arbitrary writers.
2. Add bounded, timed read-only Git status/diff commands with explicit argv,
   no external diff/textconv/fsmonitor, and literal workspace-scoped paths.
3. Add editing controls, Ctrl/Cmd+S, dirty indicators, draft retention, close /
   reload warnings, file positions, and per-session resizable panel width.
4. Add Git status, staged/worktree diff selection and refresh. No stage/commit/
   discard actions.
5. Verify save/conflict/traversal/permissions and Git parsing in daemon tests;
   extend real-daemon Playwright smoke for saves, conflicts, dirty-file close,
   draft/session restoration and read-only Git diffs. Run frontend tests,
   typecheck/build and compile/fmt gates. Leave changes uncommitted for review.

## Verification evidence

- Daemon: 385 tests passed (optimistic conflicts, workspace-bound revisions,
  read-only files, permission bits, Git parsing/literal paths and disabled helpers).
- Frontend: 357 tests passed; typecheck and production build passed.
- Real Playwright/Chrome checks cover save/shortcut, continued typing during a
  delayed save response, external-write conflicts, close cancellation, draft and
  width restoration across sessions, CRLF, caret/scroll restore, mobile editor,
  and working/staged/untracked Git diff. Index bytes and HEAD are unchanged by
  the Git view. First-phase file and real PTY checks are also rerun.
- Windows, extended ACL behavior, and real relay deployment remain unverified.
- All changes remain uncommitted, including phase 1.

Final UI review fixed a desktop positioning override that clipped the narrow
screen editor. Playwright now checks the whole panel, close button and save
button stay within the viewport. A separate cache regression verifies a late
save response reaches a remounted view without losing newer typing or notifying
another session. The real-daemon Playwright suite remains fully passing.
