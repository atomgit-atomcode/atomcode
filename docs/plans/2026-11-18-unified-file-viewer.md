# Unified workspace file viewer implementation plan

**Goal:** All local file entry points use the workspace viewer: desktop side panel, mobile full screen, content/current Git diff switching, session-scoped read-only history. Remove the standalone file preview dialog.

**Architecture:** App owns panel visibility and session-scoped open requests. A neutral FileViewerContext routes local Markdown and mutation-card clicks to the displayed session workspace. Workbench retains active-session editing/terminal ownership; history gets a read-only instance with separate presentation state. Shared WorkbenchFileViewer and GitDiff rendering/read logic provide content (text, Markdown, supported images, download) and current working-tree/index differences. Backend canonical containment and 2 MiB limits stay authoritative.

**Tech stack:** Preact, TypeScript, existing CSS tokens, existing daemon workbench read endpoints, Node tests and Playwright.

## Baseline and scope

- Branch release/v5.2.8, baseline f8f63650f97aea71700066ef2c8bc3d8afb59128.
- Preserve the eight uncommitted files from the earlier card-width and workspace-open change, building on them rather than reverting.
- Frontend-only; do not alter runtime/session ownership, backend access policy, versions, dependencies, or unrelated files.
- Claude Code implements production files only. Codex owns test changes, runs verification and independent review. Neither commits/pushes without the next explicit user request.

## Implementation stages

1. Introduce a neutral file-open context and one App session-scoped request route. Every request carries originating session and path; ignore mismatched stale requests. Route Markdown from chat and nested workspace documents through it. Cards share the same route and keep full-width presentation. Remove FilePreviewDialog production consumers and obsolete dialog/CSS once unused.
2. Extend Workbench to show historical sessions read-only without activating them. Explicit read-only banner/name; no editor, save, terminal creation, terminal mounting or mutating calls for history. Isolate read-only presentation state from active mutable drafts. Preserve active instance while browsing, and do not reveal a historical panel after returning to another session. Repeated open requests and close/reopen must not replay consumed requests.
3. Extract shared file content and Git diff viewer pieces; both file tree and Git selection use them. Content supports loading/error/retry/download, supported image media only, Markdown sanitization and relative links, code/text reading. File viewer has Content / Current Git diff plus working tree/index selection where applicable; no repo/no changes/loading/error are distinct and do not claim historical snapshots. Existing Git selection/staged-only/untracked/deleted-file behavior stays functional. Downloads come from the session-validated preview blob. Abort/ignore stale reads and revoke object URLs.
4. Desktop keeps resizable/maximizable workspace; mobile <=850px fills viewport, provides close/back and focus restoration, leaves chat scroll intact. Light/dark tokens and long paths stay readable. Keyboard Escape closes/restores appropriately; no modal focus trap on desktop side panel.

## Validation owned by Codex

- Unit path/routing tests: session mismatch, Unix/Windows paths, containment boundary, nested Markdown links and consumed request behavior.
- Update file-links Playwright fixture for all former modal entry points, content/diff/index/no repo/no changes, images/download/error/slow response, historical identity/read-only/no rebind, nested Markdown links, close/reopen, desktop and 390px mobile width, dark theme, no chat scroll jump.
- Run npm run typecheck, npm test, npm run build; run file-links, workbench, workbench-refresh, session-browse, session-fork and image regression scripts as appropriate. Inspect screenshots.
- Run git diff --check and cargo fmt --all -- --check; Rust compile gate only if Rust boundaries are changed or before requested merge per AGENTS.md. No Rust production changes planned.
- Failure semantics: retain last safe session binding/drafts; display honest load errors and retry; out-of-root paths remain denied by server; stale responses never replace current selection. No fallback to independent modal.

## Implemented and validated

- Completed unified file-open routing, separate read-only history workspace, shared FileContent/GitDiff/FileGitDiff, removal of FilePreviewDialog and its CSS, download/retry/loading states, current-diff labelling, and full-screen compact mobile layout.
- Kept current-session drafts and terminal lifetime in the existing Workbench; read-only history uses separate non-persisted presentation cache.
- Codex validation: TypeScript check and production build passed; Node suite 491 passed; path-boundary/nested-path cases passed; file-links browser regression passed at 1200px and 390px in light/dark themes; refresh, session-browse, message-images and packaged-assets regressions passed.
- Real daemon + real PTY workbench regression passed: authentication, canonical workspace boundary, preview limit, images/Markdown, directory refresh, terminal session isolation/reconnect/close, editing/save/Ctrl+S, typing during save, external conflict, dirty-close protection, per-session drafts and panel width, CRLF, caret/scroll restoration, mobile editor and working/staged/untracked Git diff.
- `cargo fmt --all -- --check` and `git diff --check` passed. No Rust production changes belong to this task; concurrent session/todo changes are separate.
- Visual artifacts: `/tmp/atomcode-unified-content-{1200,390}.png`, `/tmp/atomcode-unified-diff-{1200,390}.png`, `/tmp/atomcode-unified-history-{1200,390}.png`. Real-daemon result: `/tmp/unified-viewer-workbench5.log`; fixture result: `/tmp/unified-viewer-qa-e2e.log`.
- Browser tests use Chrome desktop/mobile viewport; physical iOS Safari is not verified. History previews read current disk/current Git state, not historical file snapshots.
