# Requirement review and bounded tool hints

Approved scope: strengthen coding guidance and unknown-tool diagnostics. Do not add
an acceptance evaluator, completion veto, lifecycle hook, persisted state, or protocol.

Baseline: `release/v5.2.5`, `6a0916a643edc3c12739edf4a87cea975b739124`.
The checkout already contains unrelated user changes; preserve them. Running benchmark
processes keep their frozen binaries and configuration.

## Ownership and behavior

- Coding's existing persona owns task guidance. Its workflow asks for hard constraints,
  targeted counterexamples independent of the implementation, and evidence-based final
  review. This is advisory; user restrictions on verification still take precedence.
  Simple tasks need no added checklist, and completed checks need not be repeated.
- Harness tool execution owns unknown-tool results. Read only the currently visible
  catalog, offer at most three nearby names, and retain the unknown-tool error. Use
  deterministic, bounded typo matching, including MCP leaf names; unrelated requests
  get no candidates. Never repair a name, execute a candidate, or expose hidden tools.
- Runtime state, approval gates, cancellation, event pairing and termination retain
  their existing owners and behavior. No dependency or persistent format changes.

## Validation

Exercise the real tool path to show that an unknown write remains unexecuted even
with approval enabled, hidden tools never appear in hints, and unrelated requests
do not dump the catalog. Check candidate bounds, deterministic ordering, and malformed
names. Run affected harness/coding tests and formatting. Prompt quality and benchmark
improvement require a later controlled comparison; compilation does not establish them.

Validation on this checkout:

- Tool hint unit tests: 2 passed; harness integration binary: 11 passed.
- Additional tool-policy, subagent, todo and self-knowledge checks: 35 passed.
- Coding suite: 1174/1175 passed initially. The existing prompt line-boundary test
  rejected the placement of the new guidance; repositioning it preserved the old
  boundary without changing the assertion. All 52 persona tests then passed.
- Harness suite stopped at an existing architecture check:
  `row_contributions::every_tool_mount_goes_through_the_door`, pointing to direct
  registration in `mcp_tool_search::builtin_search_uses_current_child_policy_and_rediscovers_after_compaction`.
  Those direct registrations are also present at HEAD; this task does not change them.
- `bash gates/compile.sh`, `cargo fmt --all -- --check`, and `git diff --check` passed.
  Cargo.lock is unchanged. The ongoing three-agent comparison keeps its frozen packages.

Subsequent validation used a separately frozen, user-built package for one fresh Anko
attempt with the same model and reasoning effort, without failure hints or hidden tests
in the solving context. Independent grading passed 2/2 new tests and 119/119 regression
tests. The run still invoked goyacc despite the task's parser-generation constraint.
This is evidence of one passing candidate, not complete constraint adherence or an
isolated causal measurement of these two changes; the user build also contains other edits.
