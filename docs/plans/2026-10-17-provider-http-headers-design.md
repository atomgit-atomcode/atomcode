# Provider HTTP headers

Account-owned optional headers flow through the existing config resolution,
provider factory and protocol adapters. No runtime owner or session format changes.
Old configs resolve to an empty header set. Legacy provider projections preserve it.

The existing ProviderBook document transaction writes headers with account edits.
An omitted headers field leaves it alone; an object replaces the set; an empty
object clears it. Null preserves a named stored value under the write lock.
The UI reads only names, uses password fields, and supports add/remove rows.

Names are case-insensitive and normalized. Invalid names, control characters,
duplicates and transport/proxy routing headers are rejected. Values never appear
in Debug or account summaries. Custom headers override ordinary defaults;
managed signed requests retain their authenticator's authority. Empty API keys
send no empty auth header. Requests do not follow redirects with credentials.
HTTP error details redact configured values before becoming user-visible errors.

Validation covers old configuration compatibility, atomic save/keep/remove,
secret-safe summaries/errors, header-only authentication and actual requests for
OpenAI chat, Responses, Anthropic, Ollama, model listing and endpoint probes.

## Validation

- Config: 323 tests passed, including old config compatibility and atomic edits.
- Provider HTTP integration: 3 tests passed (four chat wires, discovery/probe,
  credential redaction and redirect refusal).
- Coding + daemon library tests: 859 passed, 8 skipped by the existing test policy.
- WebUI: 273 tests passed; TypeScript checks and production build passed.
- Isolated `gates/compile.sh`, final daemon all-targets check, rustfmt and diff checks passed.
