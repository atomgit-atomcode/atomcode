# Explicit high effort and omitted side calls

The report concerns macOS TUI, AtomCode 5.2.7 dd97065f, the selection
`magpie/gdtokenhub/deepseek-v4.1-flash-1017`, a 1,048,576-token window,
`reasoning_effort_levels = ["low", "high", "max"]`, and default effort `high`.
No original full log was available. Aggregate counts alone cannot identify
whether omitted fields belonged to main replies, side calls, other sessions,
older settings, or a gateway-rejection latch.

## Local capture reproduction

A local HTTP mock accepted OpenAI streaming requests and returned a deterministic
`read_file` tool call followed by a final answer. The temporary configuration used
exactly the reported model name, levels and default, but a loopback endpoint and
an inert fixture key. Each build had a separate product/session directory. No
external model service, real credentials or project source were used.

Tested `/usr/local/bin/atomcode` (dd97065f) and the existing debug executable
(eab22d60d+dirty, built during the earlier cache investigation). This is a CLI
headless capture through the shared coding runtime, not a TUI interaction test.
Both emitted:

| Call | Reported build | Existing debug build |
|---|---|---|
| Main reply, tool call | high | high |
| Concurrent session title, same model | omitted | omitted |
| Main reply after tool result | high | high |

Request ordering can vary because title generation is concurrent. The capture
stores only model, effort, tool count, output cap and whether the session header
was bound; it contains no request bodies or authentication headers. See results.json.

## Code path

- `coding/src/on_harness.rs::ChatOptions` fills missing main `AgentRequest` options
  from the configured model/session defaults. This also existed in dd97065f.
- `harness/src/plugins/session_title.rs::ModelTitle::ask` borrows the utility or
  current model but directly calls `provider.chat_stream` with default ChatOptions
  apart from its own output cap. It bypasses the main request waterfall.
- `capabilities/src/compaction.rs::summarize_window` (older build's summary call)
  likewise constructs its own options without a reasoning default. This path was
  identified by source review, not exercised by this capture.
- `openai_compat.rs` serializes effort only when the incoming options contain it
  and endpoint support is configured. The supplied explicit levels/default enable
  that support. It does not itself inherit the model's default from coding config.
- After a gateway rejects `reasoning_effort`, the existing provider instance latches
  it unsupported. The first rejection returns a visible actionable error; later
  calls omit it. This is another possible source of omission, not evidence that it
  occurred in the user's incident. Within an instance the latch is monotonic.
- Mode overrides and explicitly approved truncation recovery can select a different
  effort. Those cases must be correlated with their lifecycle/user-action context.

## Conclusion and boundary

A same-model high/omitted mixture is reproduced without any main request losing
its configured high effort. There is a concrete default-inheritance inconsistency
for direct auxiliary calls, so this is not merely the API-default UI setting
mapping to an omitted field. It also does not prove the original 495 omitted
requests or its three timeout requests were auxiliary; their complete call context
was not supplied. The reported medium request is not explained by this experiment.
No claim links absent effort to the separate timeout incident causally.

A fix should resolve per-model default effort at a shared provider/call boundary,
preserve explicit per-call effort overrides and model-level API-default semantics,
and record requested/effective effort plus the reason for an omission or override.
It must cover main replies, titles, compaction, child agents, mode/reload behavior,
and the existing gateway-rejection fallback. An extra generic passthrough switch
is not justified solely by these counts: endpoint capability is already declared
by the configured effort levels/default. The reproduction itself did not modify production code; the subsequent fix is described below.


## Implemented correction and verification

The coding provider factory now applies the selected model's default effort to
all calls through that provider, including direct title and summary calls.
Explicit incoming values win; model configuration with no default continues to
omit effort. The decorator forwards session affinity, vision, window and effort
levels, and does not own a second runtime or alter session storage. Adapter
capability checks and the existing OpenAI rejection latch still run afterward.

The same local tool-call capture with a newly built debug executable produced
high main -> high title -> high main after the tool result. The installed
reported build remained high -> omitted -> high as the control. Raw request
capture and the new diagnostic fields agree. Title logs report
`effort_source="model_default"`, `requested_effort="omitted"`,
`model_default_effort="high"`, `resolved_effort="high"`, and actual
`reasoning_effort="high"` with disposition `sent`. See fixed-results.json.

Other diagnostic dispositions are `api_default`, `endpoint_capability_disabled`,
and `gateway_rejected`. `request_value` means the incoming value equals the
model default; `call_override` means the incoming value differs. These are
metadata-only file logs, not new conversation notifications. They do not claim
a main middleware-filled value was a deliberate user override.

Verification: 3,888 coding/capabilities tests passed, 18 skipped; workspace compile
gate passed. The provider-default regressions cover missing values, explicit
low/max overrides, API-default rebuild, independent defaults and metadata/session
binding forwarding. Existing adapter tests retain capability omission and 400
rejection behavior. The mock CLI proves actual factory-to-wire behavior for the
reported config; it makes no claim to identify the user's original 495 omissions,
medium request or the root cause of the separate timeout incident.

## Subsequent explicit title policy

The capture in fixed-results.json documents the intermediate default-inheritance
fix, before the following approved title policy. It is not a capture of the final
title policy: titles now explicitly select the lowest supported representable
effort (low/medium/high/xhigh/max), irrespective of advertised ordering. Thus the
reported low/high/max model now requests low for titles while main replies keep
high. Empty/unknown-only capability lists preserve model defaults; omission is
never treated as off. Existing title timeout, output allowance and fallback stay
unchanged. The title policy logs purpose, selected effort and reason. Tests verify
capability selection and the options delivered by an actual title invocation.

Long-thinking title requests retain the existing independent 15-second total
budget, including provider resolution/opening and stream consumption. A timeout
falls back to the first prompt rather than blocking the main turn. Existing
virtual-time tests cover both a pending stream and a stream continuously yielding
metadata; the cooperative yield ensures the timeout is polled in the latter case.
