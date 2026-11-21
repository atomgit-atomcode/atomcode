# Response-header wait policy

The old 90-second timeout bounds `reqwest::RequestBuilder::send()` until HTTP
response headers, not overall model reasoning or response-body streaming.

## Accepted behavior

- Default per-attempt response-header wait: 300 seconds for Anthropic,
  OpenAI-compatible and Responses adapters. Connect timeout remains unchanged.
- `response_header_timeout_secs` is a positive integer on `[models.<id>]` or
  legacy `[providers.<id>]`. It overrides `ATOMCODE_RESPONSE_HEADER_TIMEOUT_SECS`;
  absent/invalid environment values fall back to 300 seconds. Configured zero
  is rejected rather than disabling liveness protection.
- OPEN attempts and backoff share a cumulative allowance of twice the configured
  header timeout: 600 seconds by default. Native retries reuse the existing
  request recovery allowance, so an outer retry cannot refill that time.
- Response-body streaming and inference after headers do not consume the OPEN
  allowance. Existing first-content and idle-progress protections still apply.
- Exhaustion is terminal; cancellation still drops the active operation. Retry
  presentation remains quiet, with diagnostics available on demand.

## Ownership

Coding resolves model/provider config and constructs all protocol adapters.
Adapters measure OPEN duration and enforce the transport timeout. The existing
kernel `RequestRecoveryBudget` holds neutral shared duration counters and limits,
without owning a clock, timer or coding lifecycle. Harness still owns request
retries, backoff and cancellation. A reserved backoff that would cross the
transport allowance is refused before sleeping; authoritative Retry-After is
never shortened. No new runtime owner or persistence format is introduced.

```toml
[models."example/lumen-alpha"]
account = "example"
model = "lumen-alpha"
response_header_timeout_secs = 600
```

The example permits 600 seconds for one header wait and 1200 cumulative seconds
for OPEN plus backoff. A provider/model switch uses the selected entry's setting,
including clearing the previous override when the new entry leaves it unset.

## Verification

Use paused Tokio time to verify a response after 120 seconds succeeds, consecutive
native attempts and backoff exhaust the shared 600-second allowance, another
attempt cannot begin after exhaustion, and streaming time is excluded. Verify
config round trips/resolution, factory defaults and override precedence, then run
affected crate tests and the complete compile gate.
