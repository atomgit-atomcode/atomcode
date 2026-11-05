# WebUI workbench implementation plan

Goal: a session-scoped right workbench with lazy file tree, bounded read-only
text/Markdown/JSON/image previews and interactive PTY terminals, verified using Playwright.

Architecture: daemon owns file access and terminal processes, outside the coding
runtime. Native session metadata or the live binding supplies cwd; the browser
never supplies an authoritative root. Canonical paths must remain within that
root. Terminal IDs are daemon-local, checked against their owning session.
Existing API authentication protects all endpoints. No new agent lifecycle or
session persistence format. Browser tabs/layout are session-scoped UI state.

1. Add bounded workspace listing/preview APIs with traversal/symlink/size tests.
2. Add portable PTY manager and authenticated HTTP create/read/input/resize/close
   APIs. HTTP polling preserves relay compatibility. Bound sessions, output and
   input; clean up on idle timeout and daemon shutdown, close kills process.
3. Add lazy right panel with file tree, read-only preview and lazy xterm terminal;
   retain tabs on session switches and reject stale responses. Narrow screens use
   an overlay. Preserve existing chat layout when closed.
4. Run daemon/frontend tests, typecheck/build, compile/fmt gates. Use Playwright
   against the real daemon to check files, traversal, terminal interaction,
   resizing, session isolation and closure; capture desktop/mobile screenshots.

No file writing, Git mutation or side conversations in this first release.

## Verified result

- Daemon: 379 tests passed; frontend: 354 tests passed.
- Typecheck and production UI build passed; xterm is a separate lazy chunk.
- `bash gates/compile.sh`, formatting and diff checks passed.
- Real Playwright/Chrome run passed with isolated daemon/home/workspaces. It
  verifies HTTP authentication, workspace containment, size limits, text/JSON/
  Markdown/image preview, initial landing-page creation, PTY output, resize,
  terminal/input limits, session isolation, reconnect, mobile layout and shell/
  background-job cleanup. Artifacts: /tmp-equivalent atomcode-workbench-tKnori.
- Windows ConPTY and a real remote relay were not manually tested.
- Changes remain uncommitted for review.
