# AtomGit platform error feedback

Baseline: release/v5.2.5 at 7d6b291dc; existing session-reference and changelog
changes are outside this task. The shared AtomgitClient owns HTTP error handling;
the model tool descriptions own instructions for safe follow-up actions.

Expose platform 429 as distinct from model rate limiting. Preserve allowlisted
Retry-After, rate-limit and request-ID headers and bounded response evidence;
unknown quota scope or recovery time must remain unknown. Do not add automatic
write retries or silently downgrade review replies to ordinary comments.

Transport failures, server errors and successful HTTP responses whose write result
cannot be decoded are uncertain writes: query the target before any resend.
Read failures must not claim a write happened. Preserve existing auth/access/not-found
messages. Remove request URLs from transport/decode errors to avoid leaking query
credentials. This feedback guides the model; it is not a persistent cooldown or
an idempotency mechanism, and does not increase platform quota.

Verify with local mocked HTTP responses for 429 with/without headers, malformed
successful reads/writes, server failures and explicit validation rejections.
