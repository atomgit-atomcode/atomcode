# Read-only queries for explicitly referenced sessions

Keep the native session log as the authority. Add `session_reference` through a
normal coding harness tool row, gated by the existing tool catalog and opt-out.
The tool derives allowed source IDs from the latest current-turn human
`UserMessage` in the scoped agent log; assistant, peer and historical excerpt
text cannot grant access. No separate grant registry or session owner is added. Delegated members are refused: their initial task prompt may be model-authored. The lookup copies only the relevant user text, not the entire live event log.

Reuse the bounded reference reader. Query-aware text windows retain literal hits
inside oversized messages before the canonical projector removes superseded or
rewound content. Default queries use the effective projection and summary.
Explicit `historical=true` returns raw user/assistant records with a warning that
they may be superseded or undone; it is not a fallback when effective search is
empty. Tool output, tool arguments and reasoning are never returned.

Prefer recent matching windows so old proposals do not crowd out newer corrections. Present each page chronologically. Return matching message positions plus neighboring dialogue, bounded by 10 rows
and 16,000 text characters per call. Pagination offsets index that query's result
positions. A captured event sequence is required after page zero and is checked
against the source on every continuation; source changes require a fresh query.
Positions are message positions, not fabricated turn numbers. Detailed queries
require native event sessions; older snapshots remain excerpt-only.

Reuse the 64 MiB scan, 16 MiB record, 8 MiB retained-payload guards and five-second
request timeout/cancellation. Do not generate new model summaries or expand
nested references. Source contents remain untrusted reference data.

Verify source authorization (including forged assistant links and new turns),
query hits in oversized multilingual messages, context windows and output caps,
pagination/staleness, historical/effective distinction, tool opt-out, and the
existing session reference regressions. Run the full compile and formatting gates.

Verification completed: 264 session tests, 108 runtime criteria (including a real
mounted tool invocation and a subsequent unauthorized turn), two tool/runtime
reference unit tests, and 15 wiring/catalog differential checks passed (389 total).
The complete compile gate also passed. The historical engine's golden files were
not rerecorded: the differential decision table records the new row-only query
tool explicitly. Current source matching and pagination remain literal/bounded,
not semantic or exhaustive history search. No real model or Windows device was
used for these deterministic tests.
