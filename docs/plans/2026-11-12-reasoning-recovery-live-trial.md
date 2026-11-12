# Reasoning recovery live gateway trial — 2026-11-12

Tested existing signed `target/release/atomcode`, SHA-256 `237e3cea7003b638ea6d915a880b598d76b28d9296805d153ee647e5515ca2e3` (delivered with 38d39be83). Source HEAD at test start was `4251ef8c95da8fac81a6df951019c36ca34a5091`; this was not a rebuild incorporating that later todo commit or concurrent browser edits. Executable hash stayed unchanged throughout.

## Method

Four real AtomGit lumen-alpha/high calls used synthetic arithmetic prompts, no tools, ephemeral sessions, no review, updates or telemetry. Every temporary model config capped output at 4096 tokens. Existing authentication was used in place; no real credential was copied. Environment settings applied only to the child processes; user defaults were unchanged. Process guards were 210 seconds for the normal case, 120 seconds for the two short stress cases and 150 seconds for the longer case; none fired.

The first three prompts requested only the decimal integer for `12345*6789 + 2468*1357`, independently checked with Python. The fourth added `98765*4321 + 31415*9265` and requested two independent checks before emitting only the integer. Correctness checks compared the entire stripped visible reply with Python's exact integer.

| Case | First reasoning limit s | Recovery limit s | Process wall s | Provider requests | Result |
| --- | ---: | ---: | ---: | ---: | --- |
| normal-enabled | 120 | 60 | 29.241 | 1 | correct answer |
| stress-enabled | 1 | 60 | 33.502 | 1 | correct answer |
| stress-disabled | 1 | 0 | 17.815 | 1 | Timeout |
| long-stress-enabled | 1 | 60 | 35.728 | 1 | correct answer |

All four ran zero tools. The three enabled calls completed normally with correct answers; none emitted an automatic-recovery notice or sent a lower-effort request. The disabled stress case received reasoning but no answer, then stopped at its one-second reasoning limit without another request. This is not evidence that enabled recovery outperformed disabled recovery: request timing and output differed.

## Observed boundary

- Normal enabled call: first reasoning and first text both observed at 27596 ms.
- Short enabled stress: both observed at 32973 ms.
- Longer enabled stress: reasoning at 34980 ms, text at 35166 ms (186 ms apart).
- Disabled stress: first reasoning at 15754 ms; dropped at 16757 ms with no text.

These timings explain why a one-second reasoning-only guard did not fire in the enabled calls. They are consistent with batching somewhere upstream/in transport; they do not identify where buffering happens or prove the model spent that whole time reasoning. Prefill remains governed separately. A long wait before any reasoning content arrives is not bounded by the reasoning-only recovery timer.

## Conclusion

This live trial verifies that opting in did not disturb these normal requests, that answers were correct and no extra calls occurred. It did **not** naturally trigger automatic lower-effort recovery, so it does not establish a live recovery success rate or justify permanent default enablement. The prior six controlled actual-executable SSE cases remain the evidence for high→medium fallback, recovery time limits and no third request. One-second limits were test-only and are not recommended for daily use.

Recommendation remains optional per-launch trial with recovery=60 and the normal first reasoning limit=120. The feature contains continuous reasoning after it becomes observable; it cannot guarantee an answer or eliminate prefill latency. No production file was changed in this test follow-up.

Evidence: `/var/folders/f_/yb9v7zss01sbpj531mbvd7q00000gn/T/atomcode-auto-live-0s9ioacl` (`summary.json`, metadata-only `provider-metadata.json`, synthetic outputs and non-secret temporary configs). Upstream correlation IDs:

- normal-enabled: `0744515db2363093a245bba84165c6ba`
- stress-enabled: `07445184881c47faaca40d9cf6158519`
- stress-disabled: `07445195c3a7cb386964b7c196c224ca`
- long-stress-enabled: `07445208ea7c794501bdd1505fcd9cd3`
