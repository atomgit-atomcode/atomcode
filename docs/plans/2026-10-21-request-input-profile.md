# Request input sizing and MCP discovery pilot

Goal: Measure actual outgoing request components without logging prompt content, and evaluate the existing MCP discovery path before changing default policy.

Findings: MCP search already exists (`17586a88e`), including permission-filtered discovery and request-only projection. Auto mode triggers when MCP JSON exceeds approximately 10% of the model context (bytes/4 estimate). A 1M-token model therefore admits roughly 400KB eagerly. Do not create another discovery implementation or silently lower that threshold.

Implementation: Add shared provider-wire diagnostics, at the existing post-projection request seam used by all adapters. Record exact JSON byte counts by tool/core/MCP and message role, with marker-based text subdivisions for project instructions, skills, memory and startup hooks. Text subdivisions are diagnostic only, never used for permissions or prompt selection. Record sizes only, no prompt bodies, credentials or tool arguments. Add MCP projection diagnostics with mode, threshold, admitted/offered counts and schema sizes.

Pilot: Test the existing forced-on mode against a 1M-context, moderate-size MCP catalog. Verify the first request retains core tools, the subsequent request exposes the searched original schema, and off/auto retain their documented behavior. Leave global user settings and auto defaults unchanged. Real-session diagnostics provide the evidence for a later policy/default decision.

Validation: Exact byte-accounting tests for OpenAI, Responses, Anthropic and Ollama request shapes; text classification and non-content logging checks; MCP pilot regression; affected crate tests, formatting and compile gate. JSON byte counts are not tokenizer or billing measurements. Native first-round prompt bytes and later tool-result bytes are reported separately by request.
