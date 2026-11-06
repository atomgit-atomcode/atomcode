# Todo guidance source references

Baseline: release/v5.2.6 at 8834d0ce8, clean tree. Source authority remains the
harness event log; todo state remains a projection of successful tool calls.
No patch copy, second store, protocol leaf or kernel business state is added.

A full todowrite plan may explicitly carry source_seqs (up to four original user
message sequence numbers in the same agent session). They identify the batch,
not mutable task positions. Incremental edits retain that batch; replacing or
clearing the plan starts a new one. Validate source membership, ordering and
rewind visibility server-side. Never guess the latest message or execute source
text. Missing references preserve old behavior.

Capabilities owns neutral reference validation/projection and paged source reads.
The existing tool-todo row supplies its scoped agent log and mounts todo_sources
(list/read); its schema and result guidance explain how to select exact sources.
Extend the existing TodoHook request anchor with active batch references rather
than adding another compaction lifecycle hook. Raw text stays in UserMessage.

Driver read-only source views use native log reads. WebUI offers collapsed source
links beside the plan and reads bounded pages on demand; TUI /todo source lists or
reads originals without submitting them as new user instructions. Completed plans
leave no active source anchor, while original facts remain available for audit.

Verify original byte/line endings, pagination, multi-message batches, failed calls,
reused provider call IDs, rewind, compaction/resume, invalid/cross-session sources,
completion/clear, UI reset/late responses, relevant crate nextest, WebUI checks,
compile gate, formatting and lock hygiene. Do not add raw source text to config,
metadata, todo sidecars, or every model request.

## Usage and limits

`todo_sources {"action":"list"}` returns the latest 32 retained original user
messages with short previews. `next_before_seq` pages older entries. The model
selects exact IDs and creates a plan, for example:

```json
{"todos":[{"content":"apply the requested boundary fix","status":"in_progress"}],"source_seqs":[2,8]}
```

`todo_sources {"action":"read","source_seq":2,"offset":0,"limit":4096}`
retrieves an original page; `next_offset` retrieves the next one. Offsets count
Unicode characters. A response contains the total length and never presents a
truncated page as the whole original. The maximum page is 16,384 characters;
there is no new raw-patch storage limit because the existing session message is
the source. This does not run or apply a patch.

The initial implementation deliberately does not classify arbitrary code fences
or guess a source from message length. Explicit IDs support multiple messages
and prevent a later “continue” message from replacing the original patch. Old
plans remain valid without IDs; historical plans are not retroactively linked.

The existing `.todos` sidecar caches reference IDs alongside task state so the
existing request reminder can retain them after model compaction without scanning
the complete event log on every request. It contains no source text and is not
authoritative for source reads: tools and drivers validate references against
retained native events. No new event vocabulary or raw-text persistence store is
introduced. A person’s cancel retires the linked batch, as the existing todo reducer does.
Kept original inputs remain historical references; inputs from an undone turn are
not readable. Clearing or replacing the list also retires the linked batch, and
completing all items removes its active reminder. Sources remain historical reference, subject to newer user
instructions and normal permission checks.

WebUI: expand the task panel, then “Original guidance”; select a message and page
its original text. TUI: `/todo source` lists references and
`/todo source <seq> [offset]` displays a page. Both read without submitting a new
user instruction. The HTTP reader pins both the full-plan event and its call
index, rejecting reads if a later plan replaced that batch. Delegated session
sources are scoped to that agent's tool context; the public display endpoint does
not independently expose member sessions.
