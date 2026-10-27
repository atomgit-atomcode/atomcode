# Native configured-model comparison — 2026-10-27

## What changed from the blocked cross-model attempt

Earlier direct-Python probes could not use native CodingPlan authentication, while MiMo returned429/500 and OpenRouter429. This run uses AtomCode's existing native auth/provider path for the configured GLM entry. No credentials copied into test configuration, no login bypass, no user model/config modification. Normal native auth may perform its usual token maintenance.

Smoke: configured AtomGit-glm5.3-flash route completes and says “已就绪。” to a READY request. This proves basic connectivity, not exact literal-output obedience.

An initial freeze attempt copied the npm JS launcher into/tmp, where relative platform-package resolution failed. It ran zero model cases. Corrected by resolving and copying the actual platform executable; no reinstall or installation edits. Initial setup failure remains recorded separately.

Frozen native executable: `atomcode5.2.4(dfa998f4)`, SHA256 `e53dfd638354d6ac66cc21b923d6208882ba5418f989d059be74b178c15221a9`. This matches the originally reported CLI revision, **not current HEAD**. Both model arms use this same executable.

## Method and limits

Configured profiles: taotoken/deepseek-v4.1-flash-1017 vs AtomGit-glm5.3-flash. Native run.started reports selected model IDs as expected; this is native route identity, not independent attestation of server weights. Native gateways, family-conditioned persona and configured/default budgets differ. Therefore this compares practical configured profiles, not isolated intrinsic model capability or matched compute.

Three fixed cases, two attempts per profile (12total): final rename preserving an approved inventory, omission audit, and necessary BootId revision. Same prior-history JSON and latest request per case. History is quoted into one ephemeral native prompt, not replayed as a true resumed conversation. `--no-tools`, `--ephemeral`, `--dev`, `--no-telemetry`; temporary working directory. No persistent test sessions or design files. No mandatory verifier or extra output constraint.120second test process deadline; **not AtomCode's default model timeout**.

## Completion and observed latency

| Case | DeepSeek seconds, reps1/2 | GLM seconds, reps1/2 |
|---|---|---|
| Rename scope |6.5/10.2 |19.9/19.7 |
| Omission audit |15.0/15.5 |41.3/39.8 |
| New BootId evidence |21.7/54.3 |74.3/120.0(timeout) |

DeepSeek6/6complete; GLM5/6complete, one at test deadline. Timeout is neither a semantic error nor a successful answer. No retry or silent removal of that sample. Profile latency includes native startup/auth/provider service conditions; do not generalize to intrinsic model speed or a product SLA.

## Targeted review

Both profiles' completed rename samples retain independent ID semantics and accepted EnterMs/ExitMs names, with no replay of the rejected DevicePath suggestion or stored Duration. GLM rename2groups DeviceNo+PipingNo+TubeSeq into a composite-key row in the detail display; this is less explicit than separate rows, but is not evidence that three columns became one physical field.

Completed BootId outputs retain the four-column identity. DeepSeek sample2states DeviceMs is “启动内计时” without input support, and assigns EventId a device-event interpretation without established source semantics. GLM's completed sample treats DeviceMs uptime/reset as conditional (“若 DeviceMs 是…”). This is a concrete difference in these samples, not proof GLM generally avoids assumptions. Chosen SQL types/dialects, fallback policies and aggregation details were not fully audited or executed.

Both profiles still expand an omission audit into reference/implementation checklists. Some are legitimate conditional advice; others add follow-up questions. No broad convergence fix or intelligence ranking follows. Review is targeted by this assistant, not independent blind adjudication. Prior bad DeepSeek outputs are not pooled with this fresh cohort to inflate a quality gap.

## Decision

This finally supplies a usable cross-profile comparison path, with limitations. The configured GLM profile is available for a small real-task comparison, but this pilot does not justify changing product/user defaults, promising a model switch fixes the original complaint, or treating service reliability as solved. Its higher observed latency and timeout must remain visible.

Production unchanged, no commit/push.26offline tests, Python compilation and diff checks pass. Native stderr is not retained; native error details withheld; configured static api_key values length>=16 absent from saved JSON. Provider reasoning stripped. Artifacts under ignored results/native-model-controls-v2-20261027; setup-only failure and smoke are separate directories. Model comparison remains incomplete for the unavailable original conversation and for independent real-world quality assessment.
