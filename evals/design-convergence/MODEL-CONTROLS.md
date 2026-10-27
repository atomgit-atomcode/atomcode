# Cross-model attempt — 2026-10-26

Fixed history, same frozen DeepSeek-rendered persona, same loaded-skill replay, requested max effort, cap32768. Three fixtures (explicit pipe-local scope, identifier-renamed scope, rename after a rejected proposal), two independent responses per model/provider cell. Original histories and shuffled schedule are preserved in ignored results/model-controls-20261026. This is a direct API probe, not native product execution or automatic model-specific prompt selection. Different gateways prevent isolating intrinsic model effects.

| Configured entry | Completed / scheduled | Observation |
|---|---|---|
| taotoken/deepseek-v4.1-flash-1017 | 6/6 | Completed; not6 semantic passes |
| amd/MiMo-V2.6-Flash | 0/6 | 2 HTTP429,4 HTTP500; no valid answers |
| OpenRouter Nemotron3.5Lightning free | 0/1 smoke | HTTP429; no quality evaluation attempted |

The evaluation helper initially lacked OpenRouter's built-in endpoint default. Added the endpoint already present in atomcode-config/src/config/provider_preset.rs, then attempted one smoke request; no user config changed. Other configured AtomGit entries require a gateway auth path this direct helper does not support; did not copy credentials, bypass signing, or treat them as an OpenAI static-key provider. No unbounded retry or repeated free-model rotation. HTTP codes do not establish whether the cause is quota, provider capacity, request compatibility or another backend fault.

Outcome: **cross-model quality comparison remains incomplete**. Cannot recommend switching to MiMo/Nemotron from this evidence, or count service errors as poor reasoning. The6 new DeepSeek replies retain core fields/rename and four-column new-evidence identities in targeted assistant review, but one again says “设备毫秒计时器通常随启动归零，所以DeviceMs只在同一(DeviceNo,BootId)内可比较” without protocol evidence. This extends baseline evidence, not a model ranking. No independent blind review or full SQL validation.

Production prompts/code unchanged. Evaluation-only helper/tests/scripts added; configured credential scan of saved JSON passed. To complete the comparison requires an alternate model/provider that returns valid answers; rerun the same frozen fixtures after its availability changes. Raw failures retained, no hidden successful-only denominator.

Follow-up2026-10-27: native CodingPlan authentication now permits a configured GLM comparison; see NATIVE-MODEL-CONTROLS.md. This does not turn the earlier MiMo/OpenRouter errors into successful samples or establish a pure-model ranking.
