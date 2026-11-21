# Model-default reasoning effort for auxiliary calls

Baseline: release/v5.2.8 at fd8f5be33. Unrelated runtime.rs/told.rs edits remain
untouched. A loopback capture reproduced high main replies and omitted title
requests in both dd97065f and the existing debug executable.

The coding provider factory owns a model selection's configured defaults. Wrap
its existing adapter with an immutable default-effort decorator, forwarding all
provider capabilities and session binding. Missing per-call effort inherits the
model default; explicit per-call effort wins. An API-default model configuration
has no default, including after rebuild. The kernel contract's None means no
opinion; no new public protocol/configuration field or mutable runtime owner is
introduced. Utility/child selections built by the same factory use their own
resolved defaults, not the parent's. Standalone raw harness rows have no model
profile default and retain their own semantics.

Defaults apply before adapter capability checks or the OpenAI gateway-rejection
latch, so a rejected field is not reintroduced. Add metadata-only provenance to
provider request spans and actual OpenAI wire disposition (sent, API default,
capability disabled, or gateway rejected). Do not change rejection/retry policy,
compaction lifecycle, credentials, session storage, or UI controls.

Test direct calls with default/override/no default, capability and binding
forwarding, and actual factory-to-HTTP requests including 400 rejection and
provider rebuild. Re-run the original mock CLI/title/tool reproduction using a
corrected build. Run affected crate tests, workspace compile gate and fmt checks.

## Explicit title policy (approved after comparing other agents)

Title generation remains owned by the existing harness session-title-model row.
After resolving its utility/live provider, select the lowest advertised effort
representable by the existing kernel enum, in low/medium/high/xhigh/max order.
An empty or unknown-only capability list leaves the model default intact;
`off`/`none` are not representable and must not be treated as omission disabling
reasoning. Log purpose=session_title, effort_policy, and requested_effort.
The coding decorator continues to resolve defaults, and adapter capability and
rejection rules remain authoritative. No output cap, title timeout, lifecycle,
or persistence change is needed. Test shuffled and restricted capabilities,
unknown capabilities, and the actual title call's options.

OpenCode explicitly chooses a small model or lower title variant; OMP explicitly
sets disableReasoning for titles; deepseek-harness selects the route's lowest
supported effort and logs it. Codex shares reasoning resolution between main
sampling and compaction (not evidence that all auxiliary tasks must use high).
