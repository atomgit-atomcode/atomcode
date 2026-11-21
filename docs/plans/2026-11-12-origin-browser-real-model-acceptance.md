# Origin browser real-model acceptance

Baseline release/v5.2.7 6b0abf0500730bd2219286d73865357838ed8f4a; main and isolated delivery worktree were clean. The binary was rebuilt from that checkout with the existing isolated target. This is new-backend acceptance, separate from the earlier stock Playwright MCP model run.

The actual configured taotoken/deepseek-v4.1-flash-1017 model ran through the ordinary interactive AtomCode TUI. A fresh private product directory and empty project held only the selected account/model configuration. Its API key was supplied through a child environment variable; the written TOML contains the variable placeholder, not the key. No personal browser profile, cookies or OS grants were used. Shell, file mutation, delegation and alternate web tools were denied in the test configuration. Browser-use was explicitly selected; no auto mode, Always grant or autoApprove entry was used.

Two loopback HTTP fixtures bound distinct ephemeral ports before managed setup. The allowed origin was saved through browser enable with explicit origin-backend/backend-root paths, and the required atomcode/browser-origin-scope version 1 capability was independently checked in saved MCP configuration. The second origin was deliberately absent from policy. Both fixtures and the TUI were owned temporary processes.

## Observed result

Session 2697ca07-79f9-4728-9688-d35ead5804ff contains ten actual model tool calls: skill load, allowed navigation, explicit snapshot, form fill, another snapshot, one submit click, saved-result snapshot, screenshot, one forbidden navigation and browser close. The snapshot and screenshot were parallel calls in the same model round. Five risky calls received ordinary Allow once decisions: the two navigations, fill, click and close. All five answered facts are allow; no persistent grant was created.

The server independently persisted exactly one submission with name ATOMCODE_SCOPED_MODEL_OK and zero HTTP requests at the forbidden fixture. The forbidden navigation was approved once at the ordinary tool gate and then refused by the execution backend with MCP code -32602. This demonstrates that the ordinary approval does not override saved origin policy. The model did not retry or weaken the policy.

The native tool-result fact includes one image. Independent visual inspection of the generated PNG confirms the saved heading, submission-count=1 and exact marker. This does not prove that the model visually interpreted image pixels; its final report can also be supported by the structured snapshot. The close result is Closed the owned isolated browser; a process-tree observation at that point found no descendants of the owned backend PID. The native event log ends with a normal turn_end. TUI /exit returned 0; both captured AtomCode/backend PIDs were then absent. The fixture server was explicitly stopped with Ctrl-C (its KeyboardInterrupt exit is expected harness shutdown).

## Evidence and limits

Private local artifacts remain under /tmp/atomcode-origin-real-model: launch.py, serve.py, ports.json, http-evidence.json, close-evidence.json, real-model-evidence.json, the native session .events file and generated PNG. The machine-readable summary records event-log/screenshot SHA-256, model tool arguments, approvals, counters, terminal and process observations. Assertions checked the exact counter record, one click, one error/refusal, one native image, five Allow once decisions, required capability, close and turn terminal. No credential value is included in the summary or committed documentation.

Recorded environment remains macOS 14.5 Apple Silicon, Node 22.22.0, Chrome 157.0.8103.39, external Playwright MCP 0.0.85 and playwright-core 1.66.0-alpha-2026-11-09. Forbidden-fixture evidence counts HTTP requests, not TCP connections; the separate proxy verifier supplies its own TCP evidence. This successful model workflow does not prove full CodingRuntime turn cancellation, daemon/Web/ACP operation, native GUI actions, other operating systems or the complete per-origin access/download/upload/CDP matrix.

No production code or dependency changed in this acceptance delivery. Validation is the rebuilt binary and recorded actual TUI/model run, independent native-log/counter/image/process checks, documentation diff review and format check. Existing compile-gate evidence at the unchanged production baseline remains applicable; this documentation-only update does not rerun it. The overall delivery objective remains active.
