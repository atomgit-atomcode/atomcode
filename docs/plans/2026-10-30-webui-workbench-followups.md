# Workspace follow-ups

Keep the existing bottom task panel. Extend only the file/terminal workbench:

- Refresh the current directory and mounted Git view every ten seconds while the
  page is visible, and on window focus/visibility return. Do not reload drafts.
- Route conversation artifact buttons to the current session's file pane. Keep
  daemon canonical path containment as the authority; no model calls are added.
- Report terminal child exit status and offer an explicit restart (remove the
  old terminal before creating another). A reaped process ID must never be
  signalled during cleanup.

Validate frontend types/tests, daemon tests and real-browser PTY exit/restart,
then run the compilation and formatting gates. Windows remains unverified.

## Pre-commit audit

The integrated review confirmed authenticated routes, canonical workspace bounds,
optimistic save conflicts, bounded PTY I/O and read-only Git helper suppression.
It also corrected two UI races: the editor is read-only during an explicit
reload (typing during save remains supported), and conversation file-open
requests are consumed once instead of overriding later selections on remount.
The browser smoke explicitly delays reload responses to check draft protection.
