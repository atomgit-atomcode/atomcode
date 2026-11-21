# Preset protocol coverage

Review baseline: `release/v5.2.8`, `8675d399ccfc84e0b3a9ac7d9342218a8965f01c`.
The protocol-selection entry points already exist in `da15b3478`. This change
extends their shared catalog; unrelated chat UI and experiment files are excluded.

The config preset catalog remains the owner of protocol/address choices. CLI and
daemon supply it to TUI and WebUI; ProviderBook persists the existing selected
protocol and explicit address. No account migration, runtime lifecycle change or
new persisted schema. Switching a known address maps to its corresponding route;
an unknown custom address is retained. Failed discovery leaves manual model entry
available; supporting Messages does not imply supporting a Models endpoint.

## Official sources checked on 2026-11-16

| Preset | Anthropic base URL | Source / boundary |
| --- | --- | --- |
| Zhipu | `https://open.bigmodel.cn/api/anthropic` | [Claude API compatibility](https://docs.bigmodel.cn/cn/guide/develop/claude/introduction) |
| Zhipu Coding Plan | `https://open.bigmodel.cn/api/anthropic` | [Coding Plan setup](https://docs.bigmodel.cn/cn/coding-plan/tool/claude): shared route, separate product keys |
| MiniMax | `https://api.minimaxi.com/anthropic` | [Official Mini-Agent config](https://github.com/MiniMax-AI/Mini-Agent/blob/main/mini_agent/config/config-example.yaml); retain existing default domain, pair `.io` and current `.cn` addresses too |
| MiniMax Token Plan | `https://api.minimax.cn/anthropic` | [Subscription quickstart](https://platform.minimax.cn/docs/m-plan/quickstart); subscription key differs from pay-as-you-go |
| Moonshot | `https://api.moonshot.cn/anthropic` | [Messages API](https://platform.kimi.com/docs/api/messages), [Models API](https://platform.kimi.com/docs/api/list-models) |
| Xiaomi MiMo | `https://api.xiaomimimo.com/anthropic` | [Integration](https://mimo.mi.com/docs/tokenplan/integration); explicitly entered `token-plan-cn.xiaomimimo.com` maps within that host |
| LongCat | `https://api.longcat.chat/anthropic` | [API overview](https://longcat.chat/platform/docs/APIDocs.html); shared `/v1/models` catalog |
| Volcengine Ark | `https://ark.cn-beijing.volces.com/api/compatible` | [Messages API](https://docs.volcengine.com/docs/ark/messages-api) |
| Volcengine Coding Plan | `https://ark.cn-beijing.volces.com/api/coding` | [Coding Plan Claude Code](https://console.volcengine.com/ark/region%3Acn-beijing/docs/coding-plan-personal-ai-claude-code); preserve dedicated billing route |
| SiliconFlow | `https://api.siliconflow.cn` | [Messages API](https://docs.siliconflow.cn/docs/api/messages-post) |
| OpenRouter | `https://openrouter.ai/api` | [Claude Code integration](https://openrouter.ai/docs/guides/coding-agents/claude-code-integration) |

TaoToken and Alibaba retain the previously verified pairs. DeepSeek retains its
separate Messages adapter/preset; this change does not replace its dialect with
generic Anthropic.

LongCat, Moonshot, SiliconFlow, OpenRouter and Ark Coding Plan document Bearer authentication.
Discovery and Messages requests share an exact-host/exact-route auth rule for
these endpoints; native Anthropic and unknown/custom endpoints retain `x-api-key`.
Account header overrides still apply last. Shared catalog routing is same-origin;
discovery still refuses credential-bearing URLs and follows no redirects.

## High-priority preset additions

Approved follow-up: add seven separately selectable products to the same catalog.
Existing accounts remain untouched. Each new product has its own credential env
name; subscription presets never inherit a pay-as-you-go key. The TUI offers and
WebUI config response consume the catalog without new driver-specific lists.

| Preset | Default OpenAI base | Anthropic base | Evidence |
| --- | --- | --- | --- |
| `aliyun-token-plan` | `https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1` | same host `/apps/anthropic` | [Alibaba endpoints](https://help.aliyun.com/zh/model-studio/base-url); supplied China account URL; OpenCode snapshot |
| `aliyun-coding` | `https://coding.dashscope.aliyuncs.com/v1` | same host `/apps/anthropic` | [Coding Plan](https://help.aliyun.com/zh/model-studio/coding-plan) |
| `xiaomi-token-plan` | `https://token-plan-cn.xiaomimimo.com/v1` | same host `/anthropic` | [MiMo integration](https://mimo.mi.com/docs/tokenplan/integration) |
| `kimi-code` | `https://api.kimi.com/coding/v1` | same host `/coding` | [Kimi Code overview](https://www.kimi.com/code/docs/en/), [API-key auth](https://www.kimi.com/code/docs/en/third-party-tools/claude-code.html) |
| `tencent-tokenhub` | `https://tokenhub.tencentmaas.com/v1` | `https://tokenhub.tencentmaas.com` | [Messages API](https://cloud.tencent.com/document/product/1823/135874), [Models API](https://cloud.tencent.com/document/product/1823/130078) |
| `tencent-token-plan` | `https://tokenhub.tencentmaas.com/plan/v3` | same host `/plan/anthropic` | [Current Token Plan](https://cloud.tencent.com/document/product/1823/130060), [Bearer auth](https://cloud.tencent.com/document/product/1823/130070) |
| `tencent-coding` | `https://api.lkeap.cloud.tencent.com/coding/v3` | same host `/coding/anthropic` | [Coding Plan](https://cloud.tencent.com/document/product/1823/130092), [Bearer auth](https://cloud.tencent.com/document/product/1823/130097) |

Tencent Token Plan uses the current official TokenHub host, rather than copying
OpenCode's older `api.lkeap.cloud.tencent.com/plan/v3` snapshot. TokenHub's ordinary
product and both Tencent subscriptions offer OpenAI and Anthropic protocols.
Ordinary TokenHub Messages uses `x-api-key`; its shared `/v1/models` uses Bearer.
Tencent subscription Messages endpoints use Bearer; Kimi uses `x-api-key`.
Model listing remains best-effort and manual entry remains available. No static
model IDs or undocumented cross-product discovery fallback are introduced.


## TokenHub authentication

The ordinary TokenHub product also supports Anthropic Messages. The preset now
pairs OpenAI `https://tokenhub.tencentmaas.com/v1` with Anthropic
`https://tokenhub.tencentmaas.com`, producing POST `/v1/messages` exactly once.
The [protocol reference](https://cloud.tencent.com/document/product/1823/130079)
requires `x-api-key` for Messages; the
[shared catalog reference](https://cloud.tencent.com/document/product/1823/130078)
requires Bearer for GET `/v1/models`. Discovery applies that distinction only to
the exact TokenHub host/path; the subscription routes retain their Bearer policy.

## Verification

- Config, capabilities, daemon and CLI suites: 4,164 passed, 21 skipped.
- Validated at the review baseline in `/tmp/atomcode-priority-presets-verify`;
  all eight changed code/test files match the main checkout byte for byte.
- Coverage includes preset-to-adapter request URL/auth, catalog URL/auth,
  custom/lookalike routes, plan/key separation, TUI offer ordering and actual
  daemon account save/reload with omitted URLs.
- WebUI: 458 unit tests, typecheck and production build passed in the isolated
  review checkout.
- Desktop (1200 px) and mobile (390 px) Chrome cover the seven new choices,
  paired protocol switches, key retention, discovery/save and horizontal overflow.
- `bash gates/compile.sh` passed, including feature compilation and both canaries.
  Formatting and whitespace checks passed. The public lockfile contains none of
  `hkdf`, `hmac` or `zeroize_derive`.
- No live paid provider inference was performed. Model availability and supported
  features still depend on the service and account.
