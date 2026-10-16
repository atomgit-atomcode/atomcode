# Provider latency diagnostics

The Windows comparison shows 2.8 seconds for v5.2.1 versus 31.3 seconds for v5.2.3 on deepseek-flash. Existing accumulated logs show headers but cannot distinguish main replies from utility calls or locate the first content event. Start with v5.2.3 only, as requested.

Keep request construction, retry policy, stream delivery and runtime ownership unchanged. Instrument the OpenAI-compatible provider with local request IDs and monotonic timings for request start, headers, first nonempty raw body chunk, first nonempty reasoning/text event and terminal/drop. All elapsed timings are measured from request start. Request IDs are scoped to process IDs. Reopens carry the stream attempt. This measures provider delivery, not terminal paint time.

Caller tracing spans label main replies, session titles and side samples (next prompt suggestions/recap). Request IDs are local metadata and never go on the wire. Log model ID, actual effort/thinking fields, request size and counts; never log headers, credentials, URLs, prompts, response contents or error messages. Drop before terminal is deliberately not called cancellation: consumers may stop early or time out. Existing wire dumps remain separate and must not be enabled for this diagnostic.

Verify counters/terminal classification and existing real HTTP/SSE adapter tests, affected crate tests, formatting and compile gate. Windows execution needs a Windows build environment; local macOS verification does not establish Windows performance. No release upload or distribution is performed as part of these source edits.
