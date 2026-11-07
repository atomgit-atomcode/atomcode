# WebUI Git branch display

The daemon provides authenticated, read-only `/project/git-branch` metadata for an absolute checkout directory. Filesystem work runs off the async executor. A bounded symbolic-HEAD reader in capabilities is shared with TUI; no process, runtime command, session storage or protocol owner is added. Detached HEAD, malformed/unreadable metadata and non-repositories have no label.

WebUI places a compact branch icon/name beside the composer directory breadcrumb. Requests are cancelled when switching directories/unmounting, stale responses cannot change another checkout, and a two-second poll pauses while hidden and refreshes on focus. Long names ellipsize on narrow displays.

Validation: shared reader fixtures (nested directories, worktrees, detached/invalid HEAD), read-only daemon endpoint, authenticated API query, browser refresh/race/visibility/mobile checks, frontend typecheck/test/build, affected Rust crate tests, compile gate and formatting.
