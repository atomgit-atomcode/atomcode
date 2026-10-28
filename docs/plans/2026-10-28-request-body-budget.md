# AtomGit request body budget

Baseline: release/v5.2.5 at d5a9a9d46. Existing changelog edits belong to other work.

The provider owns the final serialized wire body. Check its byte length before opening
an HTTP request, covering Chat Completions, Responses and Anthropic messages. Reuse the
same serialized bytes for sending and retries. The known llm-api.atomgit.com endpoint
has a conservative decimal 20,000,000-byte limit; unrelated providers remain unchanged.

An oversized request returns a non-retryable structured request_body_too_large error
with actual and allowed byte counts and actionable guidance. Status 413 keeps the
existing image-budget recovery seam working. It is not token-context overflow and
does not trigger unconditional compaction. Existing tool output artifacts remain the
first line of protection; user instructions and tool-call pairs are not silently cut.

Tests cover UTF-8/JSON escaping, exact boundaries, all three endpoint paths, unrelated
hosts and lookalike hosts, and exclusion of 413 from normal HTTP retries.
