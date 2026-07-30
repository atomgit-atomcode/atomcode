# Provider Accounts and Model Profiles Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add curated provider selection, reusable provider accounts, multiple model profiles per account, legacy provider compatibility, and a redesigned `/provider` flow without breaking existing configurations.

**Architecture:** `atomcode-config` owns presets, accounts, model profiles, legacy projection, and resolution into one flattened runtime value. All drivers consume that resolved value; `CodingRuntime` retains provider reload ownership. The rollout preserves legacy provider APIs and configuration while introducing versioned account/model surfaces.

**Tech Stack:** Rust, Serde/TOML, `ConfigStore` CAS, Ratatui/crossterm TUI, Axum daemon API, existing AtomCode coding runtime and provider factory.

---

## Baseline and constraints

- Before implementation, record branch, SHA, worktree status, and recent history for every modified runtime/config file.
- Work in an isolated worktree because the current workspace may contain unrelated changes.
- Do not change release versions.
- Do not add provider/model lifecycle ownership to `atomcode-kernel`.
- Do not remove `ProviderConfig`, `default_provider`, or legacy `/providers` APIs in this plan.
- Run `cargo test` for an affected crate after each logical unit; do not repeat `cargo check` after equivalent test compilation.
- Honor the design's §14 integration constraints: single `default_model` selection with no direct `default_provider` reads (§14.1); per-model-profile `capable_model` tier resolution over the model catalog (§14.2); `evaluator_provider`/`vision_preprocessor_provider` as model-selection IDs (§14.3); WebUI stays on legacy flattened APIs in v1 (§14.4).

### Task 1: Add provider preset domain

**Files:**
- Create: `crates/atomcode-config/src/config/provider_preset.rs`
- Modify: `crates/atomcode-config/src/config/mod.rs`
- Test: `crates/atomcode-config/src/config/provider_preset.rs`

**Steps:**

1. Write failing tests for unique preset IDs, required stable fields, lookup by ID, and custom-compatible fallback.
2. Add `ProviderPreset`, `ProviderType`, `AuthKind`, and `ModelSource`.
3. Add initial presets: AtomGit, Alibaba, Volcengine, Xiaomi MiMo, DeepSeek, Zhipu, Moonshot, MiniMax, SiliconFlow, OpenRouter, OpenAI, Anthropic, Ollama, OpenAI-compatible, and Anthropic-compatible.
4. Keep model recommendation data out of this module.
5. Run:

   ```bash
   cargo test -p atomcode-config provider_preset --offline
   ```

6. Commit:

   ```bash
   git commit -m "feat(config): add provider preset registry"
   ```

### Task 2: Add account and model profile schema

**Files:**
- Modify: `crates/atomcode-config/src/config/provider.rs`
- Modify: `crates/atomcode-config/src/config/mod.rs`
- Test: `crates/atomcode-config/src/config/mod.rs`

**Steps:**

1. Write failing new-only TOML round-trip tests for `provider_accounts`, `models`, and `default_model`.
2. Add `ProviderAccountConfig` and `ModelProfileConfig` with `#[serde(default)]` integration into `Config`.
3. Validate account/model IDs, references, context windows, token limits, and preset/custom endpoint requirements.
4. Ensure serialization never emits runtime-only or ephemeral credentials.
5. Run:

   ```bash
   cargo test -p atomcode-config config::tests --offline
   ```

6. Commit:

   ```bash
   git commit -m "feat(config): add provider accounts and model profiles"
   ```

### Task 3: Implement legacy projection and mixed-schema loading

**Files:**
- Modify: `crates/atomcode-config/src/config/mod.rs`
- Modify: `crates/atomcode-config/src/store.rs`
- Test: `crates/atomcode-config/src/config/mod.rs`
- Test: `crates/atomcode-config/tests/config_store.rs`

**Steps:**

1. Add failing tests for old-only, mixed, malformed legacy, malformed new account, malformed model, collision, and no-rewrite-on-load cases.
2. Introduce a read-only logical catalog that projects each legacy `ProviderConfig` into one synthetic account/model.
3. Preserve raw legacy and quarantined tables during saves.
4. Define exact collision precedence and emit diagnostics rather than silently dropping entries.
5. Add an explicit CAS-backed `upgrade_legacy_provider` mutation; do not call it during load.
6. Test simultaneous `ConfigStore` updates and revision conflicts.
7. Run:

   ```bash
   cargo test -p atomcode-config --offline
   ```

8. Commit:

   ```bash
   git commit -m "feat(config): project legacy providers into model catalog"
   ```

### Task 4: Add the single model-resolution boundary

**Files:**
- Modify: `crates/atomcode-config/src/config/mod.rs`
- Modify: `crates/atomcode-config/src/config/provider.rs`
- Test: `crates/atomcode-config/src/config/mod.rs`

**Steps:**

1. Write failing tests for resolving preset defaults, account overrides, environment API keys, legacy entries, missing references, and secret-safe errors.
2. Add `ResolvedModelConfig` and `Config::resolve_model`. Include per-model dynamic `base_url` and `system_prompt` in the resolved value (§14.5).
3. Keep `active_provider` as a compatibility wrapper backed by the same resolution logic where possible.
4. Ensure resolved values contain everything provider construction needs, but no runtime state owner.
5. Make `default_model` the single canonical selection: reimplement `default_context_window()` as `resolve_model(None).context_window`; migrate `evaluator_provider`/`vision_preprocessor_provider` validation + lookups to `resolve_model(id)` (legacy names still accepted). Add a guard test rejecting new direct `default_provider` reads outside config resolution (§14.1, §14.3).
5. Run:

   ```bash
   cargo test -p atomcode-config resolve_model --offline
   ```

6. Commit:

   ```bash
   git commit -m "feat(config): resolve provider accounts and models"
   ```

### Task 5: Move provider construction consumers to resolved models

**Files:**
- Modify: `crates/atomcode-coding/src/config.rs`
- Modify: `crates/atomcode-coding/src/assemble.rs`
- Modify: `crates/atomcode-coding/src/parts.rs`
- Modify: `crates/atomcode-coding/src/provider_factory.rs`
- Modify: `crates/atomcode-coding/src/runtime.rs`
- Modify as required: `crates/atomcode-cli/src/main.rs`
- Modify as required: `crates/atomcode-cli/src/acp/engine.rs`
- Modify as required: `crates/atomcode-daemon/src/live_api.rs`
- Modify as required: `crates/atomcode-daemon/src/native_live.rs`
- Modify as required: `crates/atomcode-daemon/src/commands.rs`

**Steps:**

1. Enumerate current `ProviderConfig` production consumers and document which ones require connection, model, or display fields.
2. Add failing runtime tests proving model switch preserves session binding, working directory, approvals, generation isolation, and prior runtime on construction failure.
3. Pass `ResolvedModelConfig` through the existing coding runtime build/prepare/assemble seam. Migrate the ~230 direct provider-field reads across ~12 files in consumer clusters (runtime/factory, daemon API, TUI, CLI, codingplan) one cluster per commit, each with its own test run — not one big-bang change (§14.5).
4. Remove duplicated provider/model lookup from drivers; do not create a second runtime owner.
5. Rework subagent tiers: `resolve_tier_keys` ranks per-model-profile `capable_model` over the resolved model catalog (legacy projected), and `provider_factory` builds each tier via `resolve_model(<selection-id>)` instead of `config.providers.get` (§14.2). Add a test proving legacy configs keep the same fast/capable routing.
6. Verify CLI, TUI, daemon, ACP, headless/background, and subagent routing behavior where affected.
6. Run:

   ```bash
   cargo test -p atomcode-coding --offline
   cargo test -p atomcode --offline
   cargo test -p atomcode-daemon --offline
   ```

7. Commit:

   ```bash
   git commit -m "refactor(runtime): build providers from resolved models"
   ```

### Task 6: Add versioned daemon account/model APIs

**Files:**
- Create: `crates/atomcode-daemon/src/api_provider_account.rs`
- Create: `crates/atomcode-daemon/src/api_model.rs`
- Modify: `crates/atomcode-daemon/src/lib.rs`
- Modify: `crates/atomcode-daemon/src/api_config.rs`
- Preserve: `crates/atomcode-daemon/src/api_provider.rs`
- Update: `crates/atomcode-daemon/README.md`

**Steps:**

1. Write handler tests for list/create/update/delete, referential integrity, sanitized credentials, set-default, test connection, discovery errors, and CAS conflicts.
2. Implement `/provider-accounts` and `/models` resources.
3. Keep legacy `/providers` responses and mutations working.
4. Ensure account deletion cannot silently orphan model profiles.
5. Ensure connection tests do not persist drafts.
6. Run:

   ```bash
   cargo test -p atomcode-daemon api_provider --offline
   cargo test -p atomcode-daemon api_model --offline
   ```

7. Commit:

   ```bash
   git commit -m "feat(daemon): expose provider account and model APIs"
   ```

### Task 7: Redesign `/provider` around accounts

**Files:**
- Modify: `crates/atomcode-tuix/src/modals/provider_wizard.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`
- Modify: `crates/atomcode-tuix/src/render/mod.rs`
- Modify: `crates/atomcode-tuix/src/i18n/mod.rs` or the current i18n message owner

**Steps:**

1. Add state-machine tests for main account list, preset selection, credential input, advanced settings, test-before-save, model selection, legacy badge/upgrade, deletion confirmation, and cancellation.
2. Replace the flattened add/edit sequence with preset → account → model flow.
3. Keep custom template import as an advanced/custom-compatible path.
4. Never bind Tab inside the modal; verify mode switching behavior outside it remains unchanged.
5. Mask secrets and make blank edit input preserve existing credentials.
6. Run:

   ```bash
   cargo test -p atomcode-tuix provider_wizard --offline
   ```

7. Commit:

   ```bash
   git commit -m "feat(tui): manage provider accounts in provider wizard"
   ```

### Task 8: Change `/model` to model profiles

**Files:**
- Modify: `crates/atomcode-tuix/src/modals/model_picker.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`
- Test: `crates/atomcode-tuix/src/modals/model_picker.rs`

**Steps:**

1. Add failing tests for vendor/account/model search, legacy projection, current default ordering, and failed reload rollback.
2. List logical model profiles instead of provider map entries.
3. Persist `default_model` via `ConfigStore` only after runtime replacement succeeds, or roll back atomically using the existing reload contract.
4. Update status display to show vendor/model without exposing account secrets.
5. Run:

   ```bash
   cargo test -p atomcode-tuix model_picker --offline
   ```

6. Commit:

   ```bash
   git commit -m "feat(tui): select model profiles in model picker"
   ```

### Task 9: Add model recommendation and discovery abstraction

**Files:**
- Create: `crates/atomcode-config/src/config/model_catalog.rs`
- Modify: `crates/atomcode-config/src/config/provider_preset.rs`
- Modify: `crates/atomcode-daemon/src/api_provider_account.rs`
- Test: relevant config and daemon modules

**Steps:**

1. Define `ModelCatalogSource` with embedded, remote-discovery, and manual implementations.
2. Add a small embedded recommendation set with source/version metadata.
3. Treat discovery failure as recoverable and always retain manual model entry.
4. Cache remote discovery without making startup depend on network access.
5. Keep Models.dev integration optional and disabled until separately approved.
6. Run affected config and daemon tests.
7. Commit:

   ```bash
   git commit -m "feat(provider): add optional model discovery"
   ```

> **Deferred:** GitHub Copilot (OAuth-based special adapter) is intentionally out of
> scope for this plan; see design §9. The account/model schema is forward-compatible
> with it, so it can be added later as one more preset plus adapter without reworking
> this feature.

### Task 10: Cross-surface acceptance and documentation

**Files:**
- Update: `crates/atomcode-daemon/README.md`
- Update or create: `docs/testing/provider-accounts-model-profiles-acceptance.md`
- Update user-facing configuration documentation discovered during implementation

**Steps:**

1. Add fixtures for legacy-only, new-only, mixed, malformed, custom-compatible, and multiple-account configurations.
2. Verify CLI, TUI, daemon, headless/background, ACP, clix, session resume, provider reload, approval cancellation, and subagent routing where actually affected.
3. Verify no secret appears in logs, diagnostics, API payloads, snapshots, or telemetry.
4. Verify rollback to a previous AtomCode version leaves untouched legacy configs usable.
5. Run affected crate suites and the relevant workspace acceptance commands.
6. Record known unsupported providers and manual custom-endpoint fallback.
7. Commit:

   ```bash
   git commit -m "docs(provider): document account and model workflows"
   ```

## Delivery gates

The feature is ready only when:

- existing legacy configuration starts and selects the same model as before;
- one account can select at least two models without duplicated connection settings;
- malformed account/model entries do not disable unrelated providers;
- failed model reload preserves the previous live runtime and session binding;
- TUI and daemon writes are CAS-safe;
- no API or UI exposes credentials;
- `/provider` does not intercept Tab;
- legacy surfaces are explicitly reported as retained, not retired;
- switching `default_model` updates the footer context window through the single resolution path, and no display/runtime path reads `default_provider` directly (§14.1);
- legacy configs keep identical fast/capable subagent routing after the tier rework (§14.2);
- `evaluator_provider` and `vision_preprocessor_provider` resolve for both legacy names and new model-selection IDs (§14.3);
- WebUI continues to function unchanged on the legacy flattened `/providers` + `/models` APIs (§14.4).

