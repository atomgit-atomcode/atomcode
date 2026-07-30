# Provider Accounts and Model Profiles Design

**Status:** Proposed  
**Date:** 2026-07-26  
**Scope:** AtomCode configuration, TUI `/provider` and `/model`, daemon provider API, runtime provider resolution  
**Reference implementations:** OpenCode provider registry and Models.dev integration; Codex `model_providers` registry and independent model selection

## 1. Problem

AtomCode currently stores one flattened `ProviderConfig` per entry:

```toml
default_provider = "MyDeepSeek"

[providers.MyDeepSeek]
type = "openai"
base_url = "https://api.deepseek.com/v1"
api_key = "sk-..."
model = "deepseek-chat"
context_window = 128000
```

This makes a provider entry simultaneously represent:

1. a service/vendor and wire protocol;
2. an account and credential;
3. one model and its limits.

Consequently, using two models from the same account requires duplicating `type`, `base_url`, and usually `api_key`. The current `/provider` wizard also requires users to understand low-level fields even for well-known services.

The desired experience is:

```text
select vendor → configure/login account → select or enter models → choose default model
```

## 2. Goals and non-goals

### Goals

- Provide curated presets for common vendors, initially AtomGit, Alibaba Cloud Model Studio, Volcengine Ark, Xiaomi MiMo, DeepSeek, Zhipu, Moonshot, MiniMax, SiliconFlow, OpenRouter, OpenAI, Anthropic, and Ollama.
- Separate stable connection defaults from user credentials and model-specific limits.
- Allow multiple accounts per vendor and multiple models per account.
- Keep custom OpenAI-compatible and Anthropic-compatible endpoints first-class.
- Preserve every existing `[providers.*]` configuration without requiring manual migration.
- Keep provider/model resolution owned by `atomcode-config` and pass one fully resolved runtime configuration to coding runtime and drivers.
- Keep API keys and OAuth credentials out of sanitized APIs and logs.

### Non-goals

- Maintaining an exhaustive global model catalog in the first release.
- Making Models.dev a required runtime dependency.
- Automatically rewriting `config.toml` at startup.
- Adding provider-specific business behavior to `atomcode-kernel`.
- OAuth-based provider adapters, including GitHub Copilot. The first release covers API-key and endpoint-based providers only; Copilot is deferred to a separate follow-up (see §9).

## 3. Recommended domain model

The persistent and runtime concepts must be distinct.

### 3.1 Provider preset

A compiled, read-only definition of stable vendor behavior:

```rust
pub struct ProviderPreset {
    pub id: &'static str,
    pub display_name: &'static str,
    pub provider_type: ProviderType,
    pub default_base_url: Option<&'static str>,
    pub auth_kind: AuthKind,
    pub api_key_env: Option<&'static str>,
    pub model_source: ModelSource,
}
```

Presets improve UX but are not persistence owners. A small curated registry is preferable to a comprehensive list. Every preset field that may change remains overridable by an account.

### 3.2 Provider account

A user-controlled connection and credential identity:

```rust
pub struct ProviderAccountConfig {
    pub provider: String,
    pub display_name: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub user_agent: Option<String>,
    pub skip_tls_verify: bool,
    pub enterprise_url: Option<String>,
}
```

`provider` references a preset ID or a custom protocol preset. An account owns the default credential. Multiple accounts can reference the same preset. A later enhancement may allow a model profile to override the account credential, but this should not be exposed in the first UI unless a real vendor requires it.

### 3.3 Model profile

A selectable model and its model-specific behavior:

```rust
pub struct ModelProfileConfig {
    pub account: String,
    pub model: String,
    pub display_name: Option<String>,
    pub context_window: usize,
    pub max_tokens: Option<usize>,
    pub capable_model: Option<i64>,
    pub thinking_type: Option<String>,
    pub thinking_keep: Option<String>,
    pub reasoning_history: Option<String>,
    pub reasoning_effort: Option<String>,
    pub thinking_enabled: Option<bool>,
    pub thinking_budget: Option<u32>,
}
```

Model IDs are stable keys, recommended as `<account>/<model-or-alias>`. The wire model name remains a separate value so aliases and deployment names are supported.

### 3.4 Resolved runtime configuration

All consumers receive one flattened immutable value:

```rust
pub struct ResolvedModelConfig {
    pub selection_id: String,
    pub account_id: String,
    pub provider_id: String,
    pub provider_type: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: String,
    pub context_window: usize,
    pub max_tokens: Option<usize>,
    // existing thinking, TLS, routing, and prompt fields
}
```

Only `atomcode-config` resolves presets, accounts, models, environment variables, and legacy entries. Coding runtime, CLI, TUI, daemon, ACP, and clix must not implement their own compatibility branches.

## 4. Proposed configuration format

```toml
default_model = "aliyun-default/qwen3-coder-plus"

[provider_accounts.aliyun-default]
provider = "aliyun"
api_key = "$DASHSCOPE_API_KEY"

[models."aliyun-default/qwen3-coder-plus"]
account = "aliyun-default"
model = "qwen3-coder-plus"
context_window = 131072

[models."aliyun-default/qwen3-max"]
account = "aliyun-default"
model = "qwen3-max"
context_window = 131072
```

Custom endpoints use the same structure:

```toml
[provider_accounts.corp]
provider = "openai-compatible"
base_url = "https://llm.example.com/v1"
api_key = "$CORP_LLM_KEY"

[models."corp/code"]
account = "corp"
model = "company-code-model"
context_window = 200000
```

Secrets may remain environment references. A future credential store can replace literal persistence without changing account/model relationships.

## 5. Backward compatibility

Existing `[providers.<name>]` entries remain valid and are projected in memory as one synthetic account plus one model profile. `default_provider` maps to the corresponding synthetic model selection.

Compatibility rules:

1. Parse both schemas independently and quarantine only malformed entries, matching current tolerant provider loading.
2. Do not rewrite legacy configuration during startup or ordinary reads.
3. New-format IDs take precedence only on an exact ID collision; emit a visible diagnostic for collisions.
4. Legacy entries remain selectable in `/provider`, `/model`, CLI overrides, daemon APIs, ACP, and runtime reload.
5. Editing a legacy entry offers an explicit “Upgrade to multi-model configuration” operation.
6. Upgrade writes the account and model atomically through `ConfigStore` revision/CAS.
7. Keep legacy reading for at least one full major release cycle; no removal is part of this feature.
8. Preserve quarantined raw provider tables and unknown compatible fields on save.

The first release should continue serializing untouched legacy entries in their original schema. New entries use the new schema. This avoids a risky all-file migration and makes rollback possible.

## 6. Preset catalog strategy

AtomCode should follow a hybrid approach:

- Like Codex, compile a small set of stable provider definitions into the binary.
- Like OpenCode, separate provider metadata from model metadata and allow custom extensions.
- Unlike OpenCode, do not require Models.dev for the first release.

Preset entries contain only stable connection/auth defaults. Model lists may come from:

- a provider model-discovery API;
- a small embedded recommendation list;
- manual model entry.

Unknown providers always use a custom compatible preset. Preset overrides ensure vendor URL changes do not block users waiting for an AtomCode release.

## 7. `/provider` interaction

`/provider` becomes the account management center. It must not consume `Tab`, because TUI uses Tab for agent mode switching.

### Main view

```text
Provider accounts

● Alibaba Cloud · aliyun-default       3 models
○ DeepSeek · personal                  2 models

  Add provider…
  Custom compatible endpoint…
```

Keys: Up/Down navigate, Enter opens details, `a` adds, `e` edits, `d` disconnects/deletes, Esc closes.

### Add flow

1. Select a curated provider or custom compatible endpoint.
2. Configure account name and authentication.
3. Test authentication/connection without persisting partial configuration.
4. Discover models when supported; otherwise show recommendations and manual entry.
5. Configure model limits, with advanced fields collapsed.
6. Save, optionally making the model default.

Official preset URLs and protocol types are hidden by default but overridable under Advanced Settings. An empty API key during edit preserves the current secret.

### Account detail

The detail view shows sanitized connection status, credential presence, endpoint source, configured models, and actions to add a model, update credentials, test, or delete. Deleting an account with referenced models requires explicit confirmation and explains which profiles will be removed.

Legacy entries are labeled “Legacy configuration” and remain usable. Their detail view offers editing in place and explicit upgrade.

## 8. `/model` interaction

`/model` remains the fast selection surface. It lists model profiles rather than flattened providers:

```text
Models

● Alibaba Cloud / Qwen3 Coder Plus
  Alibaba Cloud / Qwen3 Max
  DeepSeek / DeepSeek V4
```

Search matches vendor, account, display name, and wire model name. Enter changes `default_model` through the existing runtime reload boundary. A failed rebuild must preserve the previous runtime and persisted default, following current fail-closed reload semantics.

## 9. GitHub Copilot (deferred, out of scope for this feature)

GitHub Copilot is intentionally **excluded** from this feature. It is an OAuth-based
special adapter (GitHub device flow, Copilot token exchange/refresh, dynamic
discovery, Enterprise URL, Copilot-specific headers, and credential isolation from
the existing Copilot MCP OAuth) whose novelty and auth-lifecycle risk are
disproportionate to the rest of this refactor.

The domain model here is forward-compatible with it: `AuthKind` reserves an OAuth
variant, and an account may carry a credential reference instead of a literal key.
When Copilot is taken up as a separate follow-up, it slots in as one more preset plus
adapter without changing the account/model schema. Until then, the first release
ships API-key and endpoint-based providers only.

## 10. Resolution, lifecycle, and failure semantics

`Config::resolve_model(selection)` is the single resolution boundary. Resolution validates:

- selected model exists;
- referenced account exists;
- preset/custom protocol is supported;
- base URL and credential requirements are satisfied;
- context and output token limits are valid.

Provider/model reload remains owned by `CodingRuntime`. The driver submits the resolved selection; runtime prepares a replacement provider and swaps only after successful construction. Failed auth, discovery, or client construction must not leave a noop handle, empty session, or partially persisted default.

Config persistence uses `ConfigStore` CAS. Tests must cover concurrent TUI/WebUI writes. API keys, OAuth tokens, and authorization headers must be redacted from diagnostics, telemetry, daemon responses, and debug representations.

## 11. API evolution

Add versioned account/model resources while preserving existing provider endpoints:

- `GET/POST/PATCH/DELETE /provider-accounts`
- `GET/POST/PATCH/DELETE /models`
- `POST /provider-accounts/:id/test`
- `POST /provider-accounts/:id/discover-models`
- `POST /models/:id/default`

Existing `/providers` endpoints continue serving legacy-compatible flattened views. New clients use the account/model APIs. Sanitized responses expose `has_api_key` or auth status, never secret values.

## 12. Testing and rollout

Required coverage:

- old-only, new-only, and mixed TOML parsing;
- malformed-entry quarantine isolation;
- deterministic precedence and diagnostics;
- no startup rewrite;
- explicit atomic legacy upgrade;
- account deletion referential integrity;
- model selection and failed runtime rollback;
- API redaction and concurrent revision conflicts;
- TUI navigation without Tab interception;
- provider discovery unavailable/offline fallback.

Rollout should be staged: domain/resolver first, compatibility projection second, daemon APIs third, TUI account/model UX fourth, and provider presets last.

## 13. Decisions

- Provider credentials default to account scope.
- Model-level credential override is deferred until a demonstrated requirement.
- Presets are curated and overridable, not exhaustive.
- Models.dev is optional future catalog enrichment, not a first-release dependency.
- Legacy provider format remains supported and is never automatically rewritten on load.
- `/provider` manages accounts; `/model` performs fast model selection.
- `default_model` is the single canonical selection; no display or runtime path reads `default_provider` directly (§14.1).
- `capable_model` is a per-model-profile rank; subagent tiers resolve over the model catalog, not the provider map (§14.2).
- `evaluator_provider` and `vision_preprocessor_provider` reference model-selection IDs (legacy provider names still accepted) (§14.3).
- WebUI stays on the legacy flattened `/providers` + `/models` API in v1; new account/model APIs are additive (§14.4).

## 14. Codebase-grounded integration constraints

The following constraints were validated against the current implementation. They
close concrete couplings the domain model above does not, by itself, resolve.
Each must be honored for the feature to be reliable; the numbering is by risk.

### 14.1 Single selection field, single resolution path (supersedes the `default_provider` dual source)

**Grounding.** Today "which model" has two divergent readers: the runtime resolves
through `active_provider()`, while display/lifecycle paths read `default_provider`
directly — e.g. `Config::default_context_window()` (`config/mod.rs`) looks up
`self.providers.get(&self.default_provider)`, and the footer, WebUI `is_default`,
and TUI respawn read the raw key. This split has already produced shipped bugs
(the footer context window not following a model switch; see the note in
`atomcode-cli/src/main.rs` `apply_cli_runtime_overrides`).

**Resolution.**

1. `default_model` is the **only** canonical selection. Introduce it; do **not**
   also let any display/runtime path read `default_provider` directly.
2. `Config::resolve_model(None)` resolves the active selection (new `default_model`,
   or a legacy `default_provider` projected per §5). Every consumer that needs the
   model, window, or provider — footer context window, runtime build, CLI
   `--provider`/`--model` overrides, WebUI `is_default`, session respawn — reads
   from the **same** `ResolvedModelConfig`.
3. Reimplement `default_context_window()` as `resolve_model(None).context_window`
   (or delete it and migrate call sites). `default_provider` is retained **only**
   as a legacy input to projection (§5), never read directly by display or runtime.
4. Add a guard test that greps the tree for direct `default_provider` reads outside
   `atomcode-config`'s projection/resolution code and fails on new ones, plus a
   behavioral test that switching `default_model` updates the footer window through
   the single path.

### 14.2 Subagent tier resolution over accounts + model profiles

**Grounding.** `resolve_tier_keys()` (`atomcode-coding/src/subagent_tiers.rs`) ranks
entries in the flat `config.providers` map by `capable_model`; `provider_factory.rs`
`resolve_subagent_tier_thunks()` builds the fast/capable tier providers from those
keys; the daemon create handler hardcodes `capable_model: None`. Splitting providers
into accounts + profiles breaks this scan.

**Resolution.**

1. `capable_model` is a **per-model-profile** rank (as already placed in
   `ModelProfileConfig` §3.3), not per-account.
2. `resolve_tier_keys` operates over the **resolved model catalog** (legacy + new,
   projected uniformly per §5), ranking model-selection IDs by `capable_model`. Host
   model = the current `default_model`; fast = lowest-rank capable profile, capable =
   highest. Legacy providers project to one profile carrying their existing
   `capable_model`, so current tier behavior is unchanged.
3. `provider_factory` builds each tier via `resolve_model(<selection-id>)` instead of
   `config.providers.get(key)`.
4. The new model-profile create/patch API accepts `capable_model` per profile; the
   legacy `/providers` create handler keeps its current default.

### 14.3 Provider-name reference fields (`evaluator_provider`, `vision_preprocessor_provider`)

**Grounding.** Both are top-level `Option<String>` fields that reference a provider
**by name** and are validated with `config.providers.contains_key(...)`
(`config/mod.rs` vision validation; `runtime.rs` goal-evaluator lookup;
`atomcode-cli/src/vision.rs`). The domain model §3–§5 does not mention them; if the
ID space changes they silently break.

**Resolution.**

1. Both fields' values become **model-selection IDs** (the same ID space as
   `default_model`), resolved through `resolve_model(id)`.
2. Backward compatibility: a value that matches a legacy provider name resolves via
   the legacy projection (§5), so existing `config.toml` keeps working with no edit.
3. Validation changes from `providers.contains_key` to "`resolve_model(id)` succeeds";
   the two lookup sites read the resolved model. Field names are kept to avoid churn;
   document that the value is now a model-selection ID (legacy names still accepted).

### 14.4 WebUI and the legacy API surface (v1 scope)

**Grounding.** The daemon `/providers` and `/models` handlers, and WebUI
(`webui/src/api.ts` `ModelInfo` / `ProviderInfo` / `ConfigInfo`), assume a flat
`providers` map with the provider name as identity. WebUI is not in the plan's file
list.

**Decision.** v1 does **not** migrate WebUI. The daemon keeps serving the legacy
flattened `/providers` and `/models` views (backed by the same resolved catalog), and
WebUI keeps its current provider-name-as-identity contract unchanged. The new
`/provider-accounts` and model-profile resources (§11) are **additive**, consumed only
by the TUI and future clients. A later phase migrates WebUI to the account/model
resources. This bounds blast radius and keeps WebUI shippable without a coordinated
frontend change.

### 14.5 Resolved value completeness and incremental migration

1. `ResolvedModelConfig` (§3.4) must also carry a **per-model dynamic `base_url`**
   (CodingPlan selects a model-specific endpoint from the server rather than the
   account's static base URL) and `system_prompt`, beyond the fields listed there.
2. Task 5's blast radius is real: ~230 direct provider-field reads across ~12 files
   (notably `atomcode-tuix/src/event_loop/mod.rs`, `atomcode-coding/src/{runtime,
   config,provider_factory,parts}.rs`, `atomcode-daemon/src/api_provider.rs`,
   `atomcode-codingplan/src/setup.rs`). Migrate **incrementally** behind an
   `active_provider()`-compatible wrapper (plan Task 4 step 3), converting consumer
   clusters one at a time with their own test runs — not one big-bang commit.

### 14.6 Corrected assumption: `/provider` and `Tab`

The concern that `/provider` "must not consume `Tab`" (§7, delivery gates) is weaker
than stated: while a modal is active the event loop routes **all** keys to
`modal.handle_key()` before the global agent-mode toggle, and neither the current
`provider_wizard` nor `model_picker` binds `Tab`. Honoring "do not bind `Tab`" is
still fine, but it is a style preference, not a correctness risk — the modal cannot
leak `Tab` to mode switching regardless.

