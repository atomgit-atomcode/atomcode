# /provider Panel Redesign

**Status:** Approved
**Date:** 2026-07-28
**Scope:** Replace the scrollback Q&A `/provider` wizard with a full-panel modal
in the style of `/plugin` (`PluginManager`), on the provider-accounts/model-
profiles branch.

## 1. Problem

The current `/provider` runs as a scrollback Q&A: each step pushes a prompt line
and the answer is typed in the bottom input box. Managing accounts + multiple
models per account this way is clumsy — there is no persistent list, and adding
an api key / model is a linear prompt chain that can't be reviewed or corrected
in place.

## 2. Goals

- A persistent full-panel modal (input box hidden, like `PluginManager`), so the
  provider config is visible and navigable.
- Views as tabs (`账号` / `模型`), actions as key bindings (matching `/plugin`).
- In-panel form fields for adding/editing (api key, model, window) — captured
  inside the panel, not the main input box.
- Account-centric: one account (connection + credential) exposes several model
  profiles; legacy `[providers.*]` still appear (badged) and stay usable.
- Reuse the resolution/persistence already built (`build_preset_entry`,
  `commit_preset_account`, `resolve_model`-aware `set_default_provider_and_reload`,
  `logical_accounts`/`logical_models`).

## 3. Non-goals

- Model discovery / recommendations (deferred, Task 9 of the parent plan).
- OAuth providers (GitHub Copilot deferred).
- Rewriting the legacy edit semantics — legacy entries keep in-place editing.

## 4. Module

New `crates/atomcode-tuix/src/modals/provider_panel.rs` — a `ProviderPanel`
modal, `PluginManager`-shaped. Renders through a `MenuKind::Plugin`-style sticky
footer so the main input box is hidden. The old `ProviderWizard` is retired once
`/provider` points at the panel; its pure helpers (`build_preset_entry`,
`unique_account_id`, `commit_preset_account`, `DraftProvider::into_config`/
`apply_onto`) move to or are shared with the panel.

### State

```
enum Tab { Accounts, Models }
enum Mode { List, Form(FormState), DeleteConfirm { target } }
struct ProviderPanel {
    tab: Tab,
    selected: usize,        // row in the current list
    mode: Mode,
    // form field state (focused field + text buffers + cursor) live in FormState
    close_requested: bool,
}
```

Text input is captured into the focused `FormState` field (mirroring
`PluginManager.url_input` / `url_cursor`).

## 5. Layout & keys

```
┌─ Provider 管理 ────────────────────────┐
【 账号 】 模型                    Tab/←→ 切换
─────────────────────────────────────────
 (current tab body)
─────────────────────────────────────────
 <context-specific key legend>          Esc
```

- `Tab` / `←` `→` switch tabs. `↑` `↓` move the selection.
- **账号 tab:** `a` add · `e` edit · `d` delete · `↵` expand (show the account's
  models) · `Esc` close. Empty state: `按 a 添加第一个 provider`.
- **模型 tab:** `a` add · `e` edit · `d` delete · `↵` set default + switch this
  session · `Esc` close. Rows grouped by account; the active default is marked
  `● [默认]`.
- In a Form: `Tab` next field · `↵` save · `Esc` back to the list.
- In DeleteConfirm: `y` / `n`.

## 6. Accounts tab

Lists the unified account catalog (`logical_accounts()`): new-schema accounts
plus legacy providers (badged `[旧]`). Each row shows the account id, its
vendor/preset, and its model count; the account owning the active `default_model`
is marked.

- `a` add → the add **Form** (§8).
- `e` edit → Form pre-filled. New-schema account → edit account fields
  (display_name, api_key, base_url, skip_tls). Legacy provider → edit in place
  with the existing field set (reusing `apply_onto`); a blank api key keeps the
  current secret.
- `d` delete → DeleteConfirm; deleting an account explains which model profiles
  go with it. Deleting a legacy provider removes the `[providers.*]` entry.
- `↵` expand → switch to the 模型 tab filtered to that account (or an inline
  sub-list) so its models are visible.

## 7. Models tab

Lists all model profiles (`logical_models()`), grouped by account and ordered
like `/model`. Each row: `<account> · <model>` (+ `[默认]`).

- `a` add model → a shorter Form: pick account (cycler over existing accounts) →
  model name → context_window (optional) → 设为默认`[ ]`.
- `e` edit model → limits form (model name, context_window, max_tokens,
  capable_model, thinking/reasoning under advanced).
- `d` delete model → DeleteConfirm.
- `↵` set default → `set_default_provider_and_reload(selection_id)` (resolve-
  aware; persists `default_model`, reloads, switches the session).

## 8. Add form (账号 tab)

One in-panel form that creates one account + its first model and (by default)
makes it the active default:

```
【添加账号】
厂商:   ‹ DeepSeek ›        (←→ 切预设 / 15 个)
api_key: sk-█________________  (留空则用 $DEEPSEEK_API_KEY)
模型:   ________________
窗口:   131072 (默认)
设为默认: [✓]
 Tab 下一项   ↵ 保存   Esc 取消
```

- `厂商` is a `←`/`→` cycler over `provider_preset::PRESETS`. For a preset with a
  built-in endpoint the URL is hidden; a custom-compatible preset
  (`*-compatible`) reveals a required `base_url` field.
- On save: build the account + model via `build_preset_entry`; insert; if
  `设为默认` is checked, set `default_model` to the new model id; `save_and_reload`.
  Account-id uniqueness via `unique_account_id`.
- Keyless presets (Ollama) hide the api_key field.

## 9. Persistence, reuse, failure

- All mutations go through `ConfigStore` (`save_and_reload`) — same CAS-safe path
  the current wizard uses. A failed reload preserves the previous runtime.
- Secrets: the api-key field renders masked; a blank edit keeps the existing key.
- Reuses `build_preset_entry`, `unique_account_id`, `commit_preset_account`,
  `logical_accounts`/`logical_models`, and the resolve-aware
  `set_default_provider_and_reload`.

## 10. Rollout (tasks)

1. Panel skeleton: modal struct, tab header + list rendering, `MenuKind::Plugin`
   input-box hiding, key routing, `/provider` → `ProviderPanel`. Old wizard kept
   behind the scenes until parity.
2. Accounts tab list (accounts + legacy badge, model count, default marker) +
   empty state.
3. Add form (preset cycler, fields, save via `build_preset_entry` +
   `set-default`), custom-endpoint base_url field.
4. Edit + delete for accounts (new-schema + legacy in-place).
5. Models tab: list + set-default (`↵`) + add/edit/delete model.
6. Retire the scrollback wizard; keep shared pure helpers.

Each task compiles + tests (`cargo test -p atomcode-tuix provider_panel`) and is
its own commit. Live interaction needs real-machine verification.
