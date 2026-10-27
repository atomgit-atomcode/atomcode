# Inline history implementation plan

Goal: remove the separate full-history card and navigation tab. Read older turns
in the normal timeline, loading detail in small batches on upward scroll.

Ownership: the existing native transcript endpoint is a read-only projection.
Live messages, runtime binding, approvals and composer state stay with Chat.
No session facts or persistence formats change. Reuse normal message/tool views.

1. Add tested pure helpers for bounded history windows, transcript conversion and
   identifying the live boundary. Prefer timestamps; never join ambiguous repeated
   prompts by guessing. Preserve undone turn identity.
2. Replace CompactionHistory with an inline paging controller. Fetch detail for
   at most six turns per batch, abort on navigation/unmount, preserve a DOM anchor
   when prepending, and guard late responses. A distant navigation loads its own
   window; downward scrolling fills the gap before the live timeline is joined.
3. Make the navigation rail a single list of all recorded turns plus live inputs.
   First-turn navigation loads the first window directly, not all intermediate
   detail. Current turns navigate directly to existing message nodes.
4. Verify helper regressions, frontend tests/typecheck/build, format and compile
   gate. Browser-check scrolling, first-turn navigation and return to live where
   the available browser environment permits.

## Implemented behavior and validation

The transcript query now accepts a bounded `turn_ids` batch (one to six ids),
reading/projecting the native log once per batch. Existing `turn_id` and `detail`
requests retain their behavior. `summary=true` removes reasoning/tool payloads
and caps prompt/reply previews at 160 Unicode characters; truncated prompts are
explicitly marked so they cannot be mistaken for full legacy input identities.

Inline detail reuses UserMessageView and AssistantMessageView. It retains at most
60 archived turns, evicting the opposite edge while keeping both edges pageable.
Timestamp identity joins the live suffix; ambiguous legacy prompts stay separate
rather than replacing live content by text guesswork. The latest-message arrow
always restores the live view. Sending an actual message also restores it;
merely queueing input does not interrupt reading. Reading to the live bottom
resumes normal output following. Loading status does not take layout space.

Validation: 337 frontend tests, typecheck and production build passed. A headless
Chromium component harness exercised direct first-turn loading (six details only),
downward continuation to the live boundary without duplicate live turns, the
60-turn mount limit, prepend anchor preservation, return to live and superseded
slow requests. Real Safari/mobile relay behavior still requires user testing.

374 daemon tests and `bash gates/compile.sh` passed on an isolated checkout of
c0cd7dea2 with only this task's changes. The initial main-worktree compile was
blocked by concurrent LSP edits (with_policy/config initializer errors); those
edits were preserved and excluded from the isolated validation. No session
storage, runtime lifecycle, approval or task behavior was changed.

## Pre-push audit

Detail completion now derives the join boundary from the latest live messages,
not the array captured before the HTTP request. This prevents a compaction or
reconnect during loading from applying stale message indices to the replacement
suffix. A fresh direct navigation also ignores the previous window's overlap.
The deferred latest-message scroll checks session generation, viewport identity
and attachment before moving it.

The browser harness reproduced an in-flight compaction that removes the retained
prefix while detail is delayed; the displayed live suffix correctly begins at
its new index zero. Resetting history during a delayed request leaves no late
record or deferred position restoration. The existing first-turn, anchor,
60-turn bound, live-boundary and superseded-navigation scenarios still pass.
All 337 frontend tests, typecheck and build passed after these audit fixes.
The Rust API/projection is unchanged from the isolated 374-test and compilation
validation above; its successful validation is retained.
