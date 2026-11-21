# Native marker live gateway follow-up — 2026-11-12

## Scope and baseline

Read-only diagnosis at source HEAD `32a774d6fed2c5f909121676ffda32f5f79333b7`. Concurrent MCP edits in host_rows, mcp_instructions, parts and tests were preserved. No production file, model default, retry policy, session format or runtime owner was changed.

Tested the existing signed `target/release/atomcode`, SHA-256 `8bf6ed9075a2db4439624869318486e5053d3bc89d755858da0a5fcef0e90f8e` (the executable delivered with `3f6957ed5`; not a rebuild of the newer source HEAD). Its hash stayed unchanged throughout all three calls.

## Experiment

Real AtomGit lumen-alpha requests, synthetic prompts, tools disabled, ephemeral sessions, no review, telemetry or update. Each call used a separate temporary config with max_tokens=1024, a 15-second reasoning-output limit and a 75-second process guard. Existing local authentication was used in place; credentials were neither copied into the evidence folder nor printed. Main response generation was exercised, not the separate session-title helper.

High and low prompts: `Write a concise conversation title for this request: fix a repeated truncated reply. Reply with only a plain title, no tags or tools.`

Literal control prompt: `Reply with exactly this documentation example, no commentary: ]<]minimax[>[<tool_call>EXAMPLE</tool_call>`

| Case | Effort | Visible characters | Turn ms | Reasoning characters | Upstream response ID |
| --- | --- | ---: | ---: | ---: | --- |
| short-high | high | 28 | 4199 | 260 | `0744478298a80850a2beddf58ea35a88` |
| short-low | low | 30 | 6174 | 129 | `074447882a499e95593b5aa45f0a450e` |
| literal-control | low | 43 | 8775 | 1218 | `0744478e40e51ae452e52933a703e2b0` |

All three exited 0, completed one round, issued one main provider request and ran zero tools. No malformed events or errors occurred. High and low returned ordinary short text without the native marker. Only the deliberate literal control emitted it, exactly as requested. Its one diagnostic identified the original `choices[0].delta.content` field; no text-to-tool conversion occurred. No session-title request was issued.

## Interpretation and remaining boundary

This sample did not reproduce spontaneous marker leakage, repeated consumption or an empty answer. One call per effort level cannot establish an anomaly rate or justify changing the default effort. The 1024-token cap belongs only to this bounded experiment, not a proposed production default.

The earlier spontaneous session-title observation remains upstream evidence (response `07436cf0790967a0654b197c2377dc66`, recorded in `2026-11-11-provider-format-diagnostics.md`). The decoder observed the reserved token at the original content field before inline-think routing. This establishes that it arrived from upstream; it cannot distinguish model generation from the gateway's serving/template processing. The literal control must not be reported as spontaneous corruption.

Next useful evidence is gateway-side trace attribution for that spontaneous response ID (including serving/template version and whether structured tools were enabled), or another spontaneous response with the same correlation fields. No additional speculative parser rewrite or retry is justified by this result. No upstream support message was sent.

Evidence: `/var/folders/f_/yb9v7zss01sbpj531mbvd7q00000gn/T/atomcode-upstream-probe-dig4nq3k` (`summary.json`, metadata-only `provider-metadata.json`, synthetic outputs and temporary non-secret configs). Documentation-only follow-up: no build/test gate rerun was needed; executable behavior was checked live as described.
