# Provider connection implementation plan

**Goal:** Official providers require only a credential and model selection; discovered capabilities populate configuration automatically.

**Baseline:** release/v5.2.5, ea08e7a5d795bf32e32ac86e8cec5aabec2097a2. Existing dirty context-maintenance/datalog work and generated artifacts are preserved. Recent provider history includes shared ProviderBook, discovery, clickable actions, and opt-in DeepSeek Messages.

**Tech Stack:** Rust, serde, the existing terminal frame renderer, ProviderBook document patches, and Preact/TypeScript for shared discovery metadata.

**Architecture:** TUI owns connection forms, filtering and request identity; the CLI port owns credentials and translates discovered capabilities; ProviderBook owns configuration patches; CodingRuntime remains the sole live owner. Model activation uses the existing /model command. No kernel lifecycle or session storage changes.

**Failure semantics:** Credentials never enter Moment or Debug. Saving an account is explicit and separate from activating a model. Failed discovery retains the saved account and offers retry/manual entry. Closing/replacing a picker invalidates old responses. A failed model write retains its form. Runtime switch failures use existing command reporting and preserve the previous runtime.

1. Extend shared discovery with optional vision, effort levels/default; retain context/output/name. Cover rich and minimal listings. Propagate to TUI and WebUI consumers without guessing absent values.
2. Add a preset connection form: Key focused, advanced endpoint/protocol hidden, vendor identity preserved. Enter on an unconfigured provider connects; configured accounts still drill into models. Separate connected/available lists visually.
3. After account creation, open model discovery automatically. Show capability summaries; selecting fills a compact model confirmation with advanced overrides. Default action saves and uses; save-only remains available, default-model setting stays independent. Tag asynchronous discovery requests to discard stale results.
4. Test realistic connect, manual fallback, capability persistence, cancellation/stale responses and failed writes. Run affected crate nextest suites, workspace compile gate, WebUI checks, cargo fmt and fmt check. Review diff and lock without disturbing unrelated work.

## Implemented behavior

- Unconfigured vendor rows open a credential-first connection form. Vendor identity and default endpoint are retained; Ctrl+O exposes advanced settings. Existing accounts still support editing and model management.
- Saving a connection automatically opens discovery. Picking a model fills its declared name, vision, reasoning levels/default, context and output limit. Confirmation separates capabilities into readable rows; Enter saves and dispatches one /model command, Ctrl+S saves only.
- Discovery supports filtering, mouse selection and scrolling. Retrying, changing tabs or closing a form invalidates old results. Failed writes restore input without placing secrets in Moment or Debug. Failed command dispatch leaves the saved model available rather than restoring an add form.
- WebUI discovery also adopts declared vision and reasoning metadata. No API keys, capability guesses or new runtime owner were introduced.

## Validation

- WebUI typecheck and all 291 tests passed.
- All 1,811 TUI tests passed after fixing section-height accounting and clearing discovered metadata when a model ID is changed or pasted.
- Initial cross-crate suite passed 3,131 tests before a daemon MCP test stopped the run. The remaining daemon tests passed in the follow-up run.
- `atomcode-daemon::tests::chat_offers_the_mcp_tools_on_its_first_turn` failed twice: the captured first chat request lacked `mcp__t__echo`. That path does not use provider discovery. It has not been changed as part of this task; the overall cross-crate suite is not fully green.
- Workspace/default and feature-gated compilation, including the gate's negative controls, passed. The final provider-focused run passed all 484 tests, and the final compilation gate passed on the completed changes. `cargo fmt --all -- --check` and `git diff --check` passed; Cargo.lock is unchanged and contains none of the three prohibited package names.
- No live DeepSeek credential was used; integration coverage uses API fixtures, configuration files and the real TUI dispatch path.

## Follow-up: preset priority and domains

User supplied the display priority: DeepSeek; Zhipu API/Coding Plan; OpenRouter; TaoToken; MiniMax API/Token Plan; Moonshot AI; Alibaba; Xiaomi; LongCat; Volcengine Ark API/Coding Plan; SiliconFlow; remaining vendors. PRESETS remains the shared ordered catalog for TUI and WebUI, with configured accounts kept ahead of offers. Add missing MiniMax Token Plan and Volcengine Ark Coding Plan presets with dedicated credentials; retain all existing endpoint defaults. Display/search actual endpoint host beside each TUI account and show hosts in WebUI preset choices. Host display strips userinfo, paths and queries.

Endpoint evidence: https://platform.minimax.cn/docs/api-reference/text-openai-api and https://platform.minimax.cn/docs/m-plan/intro; https://docs.volcengine.com/docs/ark/coding-plan-personal-get-started. MiniMax Token Plan uses the current documented domestic endpoint; existing MiniMax API accounts retain their original endpoint. Volcengine Coding Plan uses /api/coding/v3 instead of the separately billed /api/v3.

Follow-up validation: all 823 config/provider/discovery tests passed, all 1,812 TUI tests passed, WebUI typecheck and all 292 tests passed, and the final default/feature compilation gate plus negative controls passed. Format and diff checks passed. Updated the old materialisation fixture to choose an offer absent from the fixture's logical accounts rather than whichever vendor happened to be first; otherwise the new DeepSeek priority selected an already-present legacy account and tested a different behavior. The separately recorded daemon MCP failure and lack of live-key testing remain unchanged.

User follow-up: TaoToken is retained and now immediately precedes MiniMax in the shared preset catalog, with its existing https://taotoken.net/api/v1 endpoint unchanged. The configured-account/offer ordering regression covers this placement.

User follow-up: OpenRouter now immediately precedes TaoToken; the order is DeepSeek, Zhipu AI, Zhipu AI Coding Plan, OpenRouter, TaoToken, MiniMax, MiniMax Token Plan, Moonshot AI, followed by the remaining previously requested vendors.

## Pre-push audit

Reviewed the final provider/UI/discovery diff, credential data flow, cancellation identity, persistence failure recovery and existing /model runtime dispatch. Audit fixes clear a typed key when changing protocol/account targets, prevent keyless protocols from saving hidden key input, expose a missing credential in compact model forms without asking keyless local models for one, and tolerate unknown optional discovery metadata shapes instead of rejecting the entire listing. Added regressions for cross-target credentials and partially malformed metadata. No new runtime owner or kernel/session lifecycle changes were introduced.

Final audit regression: all 2,556 selected config, TUI, provider and discovery tests passed, including the cross-target credential and optional-metadata compatibility regressions. The final priority begins DeepSeek, Zhipu AI, Zhipu AI Coding Plan, OpenRouter, TaoToken, MiniMax and MiniMax Token Plan. WebUI typecheck/all 292 tests were already verified with no subsequent WebUI changes. Only task-related files are included in the commit; untracked previews, browser artifacts and unrelated directories remain outside it.

Pre-commit workspace/default and feature compilation gates passed, including both negative controls. cargo fmt --all -- --check and git diff --check passed; Cargo.lock is unchanged and free of the prohibited crypto packages.
