# 0029 — Explicit task specification retention

Status: implemented.

## Context

Lossless event storage does not guarantee lossless model context. A later user message can be summarized, rewritten, or folded even when it contains exact task constraints. Todo source references allow historical reads but are selected by the model; they do not represent a user's explicit retention decision.

## Decision

Users review whole original user messages with TUI `/spec` or WebUI message pin actions beside Copy, then pin or unpin them. WebUI has no `/spec` command; its attachment menu opens the management list. The native event log records a neutral `ContextRetention` fact referencing the source sequence, UTF-8 byte count and SHA-256. The source `UserMessage` is the only text store. Format version 14 identifies this event vocabulary. Older readers refuse unknown facts rather than silently losing retention state.

The last retained decision for each source determines its state. Conversation rewind removes decisions and sources using existing sequence semantics; compaction does not. Decisions survive cancellation of the earlier turn under which an idle edit was filed. Withdrawn or corrupt active sources fail validation; explicit unpin remains available for recovery.

Projection retains active original text once in source order, with historical-user framing at user priority. Fixed text bypasses compaction folds and rewrites; the net-reduction check still includes its real bytes. The actual request is checked after request middleware and before provider dispatch for exact source content, provenance and hash integrity. Failure is nonretryable and visible through the existing turn error path. No additional compaction lifecycle or runtime owner is introduced.

The coding runtime owns session- and generation-addressed operations. Harness owns the shared command and final request guard; TUI and WebUI own selection and full-text review. No model tool may change retention state. Mutations are idle-only. Existing approvals and newer user instructions retain precedence.

## Budget and limits

At most eight messages may be pinned. The aggregate original UTF-8 text allowance is `min(65,536 bytes, floor(provider context tokens / 4))`. This deliberately conservative admission limit is not an exact tokenizer estimate; most capacity remains available for system instructions, tools, history and completion. The UI reports the bytes and allowance before pinning. Unknown capacity rejects pinning. A smaller model triggers a request-time allowance check. Ordinary overflow recovery can compact other content, but fixed text cannot be silently clipped to make a request fit; normal provider overflow failure remains possible.

The guarantee covers original message text, not exact retention of image/media payloads or guaranteed model obedience. Full review is capped at 1 MiB; messages too large to review cannot be pinned. Pinned records are always listed before ordinary paginated messages, including damaged references that need unpinning. WebUI requires the intended session to be bound to the live runtime (Sync).

## Alternatives

Retaining only the latest request may preserve a later “continue” instead of the actual specification. Model-selected todo sources cannot become user authority without explicit selection. Asking the summary model to quote exact text is probabilistic. Treating pins as memory would give them replacement and precedence semantics they do not have. Storing copies in artifacts or sidecars would duplicate authority already available in the event log.

## Verification

Tests cover repeated compaction, rewrite bypass, model and model-free summaries, ordered multiple sources, corruption and missing sources, actual-loop refusal, budget and busy refusal, rewind and event serialization, native runtime session scoping and resume, TUI review before mutation, and desktop/mobile WebUI pin/unpin workflows. These are integrity guarantees; model compliance requires separate behavioral evaluation.

Real event-writer failure injection also verifies that pin/unpin cannot report success after an uncertain commit. Idle cancellation returns immediately so synchronous persistence listeners cannot reenter the idle-edit status lock.
