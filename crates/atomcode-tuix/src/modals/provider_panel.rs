// crates/atomcode-tuix/src/modals/provider_panel.rs
//
// `/provider` full-panel manager, in the style of `/plugin` (PluginManager).
// Two tabs — Accounts and Models — with in-panel forms; the main input box is
// hidden (MenuKind::Plugin). See docs/plans/2026-07-28-provider-panel-ui-design.md.

use anyhow::Result;
use atomcode_config::config::provider::{ModelProfileConfig, ProviderAccountConfig};
use atomcode_config::config::{provider_preset, Config};
use crossterm::event::{KeyCode, KeyModifiers};

use super::{tab_chip, Modal, ModalAction};
use crate::event_loop::{
    build_status, save_and_reload, set_default_provider_and_reload, Buffer, LoopCtx,
};
use crate::render::{MenuKind, MenuPayload, Renderer, UiLine};
use crate::state::UiState;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Accounts,
    Models,
}

/// A unique account id derived from a preset id, avoiding collisions with
/// existing accounts or legacy provider names.
fn unique_account_id(base: &str, ctx: &LoopCtx) -> String {
    let taken = |id: &str| {
        ctx.config.provider_accounts.contains_key(id) || ctx.config.providers.contains_key(id)
    };
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| base.to_string())
}

/// Which add-form field has focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FormField {
    Name,
    Preset,
    BaseUrl,
    ApiKey,
}

/// Add a provider ACCOUNT (name + protocol + endpoint + credential). Models are
/// added separately on the 模型 tab, so this form has no model field.
#[derive(Clone)]
struct AddForm {
    name: String,
    preset_idx: usize,
    base_url: String,
    api_key: String,
    focus: FormField,
}

/// The `PRESETS` index of the custom `openai-compatible` / `anthropic-compatible`
/// protocol preset — the only two the add form offers (fully-custom provider).
fn compat_preset_idx(anthropic: bool) -> usize {
    let id = if anthropic {
        "anthropic-compatible"
    } else {
        "openai-compatible"
    };
    provider_preset::PRESETS
        .iter()
        .position(|p| p.id == id)
        .unwrap_or(0)
}

impl AddForm {
    /// A fully-custom provider, protocol defaulting to OpenAI-compatible.
    fn new() -> Self {
        Self {
            name: String::new(),
            preset_idx: compat_preset_idx(false),
            base_url: String::new(),
            api_key: String::new(),
            focus: FormField::Name,
        }
    }

    /// Human protocol label for the toggle.
    fn protocol_label(&self) -> &'static str {
        if self.preset().id == "anthropic-compatible" {
            "Anthropic"
        } else {
            "OpenAI"
        }
    }

    fn preset(&self) -> &'static provider_preset::ProviderPreset {
        &provider_preset::PRESETS[self.preset_idx]
    }

    /// Field sequence: custom name, vendor preset, base_url (always editable),
    /// api key (only for keyed presets).
    fn fields(&self) -> Vec<FormField> {
        let mut v = vec![FormField::Name, FormField::Preset, FormField::BaseUrl];
        if !matches!(self.preset().auth_kind, provider_preset::AuthKind::None) {
            v.push(FormField::ApiKey);
        }
        v
    }

    fn advance_focus(&mut self, forward: bool) {
        let fields = self.fields();
        let cur = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (cur + 1) % fields.len()
        } else {
            (cur + fields.len() - 1) % fields.len()
        };
        self.focus = fields[next];
    }

    fn cycle_preset(&mut self, _forward: bool) {
        // Only two protocols (OpenAI-compatible ↔ Anthropic-compatible), so both
        // directions toggle. Neither ships a default endpoint — keep base_url.
        let to_anthropic = self.preset().id == "openai-compatible";
        self.preset_idx = compat_preset_idx(to_anthropic);
        if !self.fields().contains(&self.focus) {
            self.focus = FormField::Name;
        }
    }
}

/// Sanitize a user-typed account name into a TOML-key-safe id: keep
/// alphanumerics / `-` / `_` / `.`, collapse everything else to `-`, trim stray
/// dashes. Empty result ⇒ caller falls back to the preset id.
fn sanitize_account_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Edit an existing account's vendor/connection/credential. `api_key` blank keeps
/// the current secret; `base_url` and the vendor preset are pre-filled and
/// editable.
#[derive(Clone)]
struct EditForm {
    id: String,
    is_legacy: bool,
    preset_idx: usize,
    /// The preset the account started on — so save_edit rewrites the vendor ONLY
    /// when the user actually changed it (a no-op edit must not lossily normalize
    /// a `deepseek`/custom provider to the `openai` fallback).
    original_preset_idx: usize,
    /// CodingPlan (AtomGit) account: gateway-managed, so only base_url is editable
    /// — the protocol and api_key are locked (rewriting them breaks the gateway).
    vendor_locked: bool,
    api_key: String,
    base_url: String,
    focus: FormField,
}

impl EditForm {
    fn preset(&self) -> &'static provider_preset::ProviderPreset {
        &provider_preset::PRESETS[self.preset_idx]
    }

    fn protocol_label(&self) -> &'static str {
        if self.preset().id == "anthropic-compatible" {
            "Anthropic"
        } else {
            "OpenAI"
        }
    }

    /// Field sequence. A gateway-locked account only exposes base_url.
    fn fields(&self) -> Vec<FormField> {
        if self.vendor_locked {
            return vec![FormField::BaseUrl];
        }
        let mut v = vec![FormField::Preset, FormField::BaseUrl];
        if !matches!(self.preset().auth_kind, provider_preset::AuthKind::None) {
            v.push(FormField::ApiKey);
        }
        v
    }

    fn advance_focus(&mut self, forward: bool) {
        let fields = self.fields();
        let cur = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (cur + 1) % fields.len()
        } else {
            (cur + fields.len() - 1) % fields.len()
        };
        self.focus = fields[next];
    }

    fn cycle_preset(&mut self, _forward: bool) {
        if self.vendor_locked {
            return; // gateway-managed — protocol not editable
        }
        // Only two choices (OpenAI-compatible ↔ Anthropic-compatible), so both
        // directions just toggle. Neither ships a default endpoint, so base_url
        // stays as the user typed it.
        let to_anthropic = self.preset().id == "openai-compatible";
        self.preset_idx = compat_preset_idx(to_anthropic);
        if !self.fields().contains(&self.focus) {
            self.focus = FormField::Preset;
        }
    }
}

/// Which model-form field has focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ModelField {
    Account,
    ApiKey,
    Model,
    Window,
    MakeDefault,
}

/// True iff adding a model to `account_id` should prompt for the provider's
/// api_key: a non-CodingPlan account (CodingPlan uses the gateway signer) that
/// has no explicit api_key yet. Filled once, stored on the account.
fn account_needs_key(config: &Config, account_id: &str) -> bool {
    if atomcode_config::config::is_codingplan_provider_name(account_id) {
        return false;
    }
    match config.provider_accounts.get(account_id) {
        Some(a) => a.api_key.as_deref().unwrap_or("").trim().is_empty(),
        // Not yet configured (a preset-vendor quick-add) — needs a key iff the
        // preset is keyed (account_id == preset id).
        None => !matches!(
            provider_preset::preset_or_compatible(account_id).auth_kind,
            provider_preset::AuthKind::None
        ),
    }
}

/// Add a model to an EXISTING account (the 模型 tab's `a`). Optionally editing an
/// existing model in place (`edit_id` set → account is fixed, id preserved).
#[derive(Clone)]
struct ModelForm {
    account_ids: Vec<String>,
    /// Parallel to `account_ids`: whether that account still needs an api_key.
    needs_key: Vec<bool>,
    account_idx: usize,
    api_key: String,
    model: String,
    window: String,
    make_default: bool,
    focus: ModelField,
    /// When set, this is an edit of an existing model id (account locked).
    edit_id: Option<String>,
}

impl ModelForm {
    fn new_add(config: &Config, preferred: Option<&str>) -> Option<Self> {
        let account_ids = ProviderPanel::account_ids(config);
        if account_ids.is_empty() {
            return None;
        }
        // Preselect the drilled-into account (if any) so "add a model to THIS
        // account" is one keystroke, not a hunt through the ‹account› cycle.
        let account_idx = preferred
            .and_then(|p| account_ids.iter().position(|a| a == p))
            .unwrap_or(0);
        let needs_key = account_ids
            .iter()
            .map(|id| account_needs_key(config, id))
            .collect();
        Some(Self {
            account_ids,
            needs_key,
            account_idx,
            api_key: String::new(),
            model: String::new(),
            window: String::new(),
            make_default: true,
            focus: ModelField::Account,
            edit_id: None,
        })
    }

    fn new_edit(config: &Config, id: &str) -> Option<Self> {
        let m = config.logical_models().get(id).cloned()?;
        Some(Self {
            account_ids: vec![m.account.clone()],
            needs_key: vec![false], // account already exists; edit its key via 账号页
            account_idx: 0,
            api_key: String::new(),
            model: m.model.clone(),
            window: m.context_window.to_string(),
            make_default: config.effective_model_selection().as_deref() == Some(id),
            focus: ModelField::Model,
            edit_id: Some(id.to_string()),
        })
    }

    fn account_id(&self) -> &str {
        &self.account_ids[self.account_idx]
    }

    /// Whether the currently-selected account still needs an api_key.
    fn account_needs_key(&self) -> bool {
        self.needs_key
            .get(self.account_idx)
            .copied()
            .unwrap_or(false)
    }

    fn fields(&self) -> Vec<ModelField> {
        let mut v = Vec::new();
        if self.edit_id.is_none() {
            v.push(ModelField::Account);
            if self.account_needs_key() {
                v.push(ModelField::ApiKey);
            }
        }
        v.push(ModelField::Model);
        v.push(ModelField::Window);
        v.push(ModelField::MakeDefault);
        v
    }

    fn advance_focus(&mut self, forward: bool) {
        let fields = self.fields();
        let cur = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        let next = if forward {
            (cur + 1) % fields.len()
        } else {
            (cur + fields.len() - 1) % fields.len()
        };
        self.focus = fields[next];
    }

    fn cycle_account(&mut self, forward: bool) {
        let n = self.account_ids.len();
        if n == 0 {
            return;
        }
        self.account_idx = if forward {
            (self.account_idx + 1) % n
        } else {
            (self.account_idx + n - 1) % n
        };
        // The ApiKey field appears/disappears with the account.
        if !self.fields().contains(&self.focus) {
            self.focus = ModelField::Account;
        }
    }
}

enum Mode {
    List,
    Add(AddForm),
    EditAccount(EditForm),
    Model(ModelForm),
}

pub struct ProviderPanel {
    tab: Tab,
    selected: usize,
    mode: Mode,
    /// Search/filter query for the list (the plugin-style search box).
    query: String,
    /// When set (via drilling into an account with ↵), the Models tab shows only
    /// this account's models. Cleared by Tab / Esc.
    account_filter: Option<String>,
    /// The row armed by the first Ctrl+D. A second Ctrl+D deletes only when the
    /// same logical row is still selected; every other list action disarms it.
    pending_delete: Option<(String, bool)>,
}

/// Rows the List layout pushes before the first account/model row: the tab bar,
/// a blank, the reserved plugin search box (index 2), and a blank separator.
/// The selection offset MUST equal the number of these header pushes — keep this
/// in lockstep with the `items.push(...)` calls at the top of the List arm in
/// [`ProviderPanel::draw`].
const LIST_HEADER_ROWS: usize = 4;

/// Virtual last row on the 账号 tab: "+ 添加自定义 provider". Not a real id, so it
/// never collides with an account; selecting it opens the add-account form.
const ADD_PROVIDER_ROW: &str = "\u{1}add-provider";

impl ProviderPanel {
    pub fn open() -> Self {
        Self {
            tab: Tab::Accounts,
            selected: 0,
            mode: Mode::List,
            query: String::new(),
            account_filter: None,
            pending_delete: None,
        }
    }

    /// The 账号 tab list: configured accounts first (new-schema + folded
    /// CodingPlan, sorted by model-count DESC), then every unconfigured preset
    /// VENDOR (deepseek/openai/… — name only) so the user can pick one and add a
    /// model to it. Pure-legacy `[providers.*]` are excluded (they show flattened
    /// on the 模型 tab); the custom-endpoint presets are reached via the trailing
    /// "＋ 添加自定义 provider" row instead.
    fn account_ids(config: &Config) -> Vec<String> {
        let accounts = config.logical_accounts();
        let models = config.logical_models();
        let mut with_count: Vec<(String, usize)> = accounts
            .keys()
            .filter(|id| {
                config.provider_accounts.contains_key(*id)
                    || atomcode_config::config::is_codingplan_provider_name(id)
            })
            .map(|id| {
                let count = models.values().filter(|m| &m.account == id).count();
                (id.clone(), count)
            })
            .collect();
        with_count.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut ids: Vec<String> = with_count.into_iter().map(|(id, _)| id).collect();
        // Unconfigured preset vendors as quick-add rows. A vendor is only
        // quick-addable as a raw-key account when it has a concrete endpoint
        // that isn't the CodingPlan gateway: the compat presets are reached via
        // the trailing custom row; the AtomGit gateway (id "atomgit", matched
        // case-insensitively vs the CodingPlan "AtomGit" fold) must go through
        // the OAuth signer via /login; and presets without a default base_url
        // (e.g. xiaomi-mimo) have nothing to dispatch against.
        for p in provider_preset::PRESETS {
            let has_dispatchable_endpoint = p
                .default_base_url
                .is_some_and(|u| !atomcode_auth::gateway_crypto::is_atomgit_gateway(u));
            if !has_dispatchable_endpoint
                || matches!(p.id, "openai-compatible" | "anthropic-compatible")
                || atomcode_config::config::is_codingplan_provider_name(p.id)
                || ids.iter().any(|i| i == p.id)
            {
                continue;
            }
            ids.push(p.id.to_string());
        }
        ids
    }

    /// Human-facing account label. Stable account ids remain the selection and
    /// persistence keys; only preset-shaped accounts inherit the preset's
    /// display name so custom account ids are never relabelled as their wire
    /// provider.
    fn account_label(config: &Config, id: &str) -> String {
        if let Some(account) = config.provider_accounts.get(id) {
            if let Some(display_name) = account
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                return display_name.to_string();
            }
            if account.provider != id {
                return id.to_string();
            }
        }
        provider_preset::preset(id)
            .map(|preset| preset.display_name.to_string())
            .unwrap_or_else(|| id.to_string())
    }

    /// Model selection ids grouped by account (matches the /model order).
    fn model_ids(config: &Config) -> Vec<String> {
        let models = config.logical_models();
        let mut ids: Vec<String> = models.keys().cloned().collect();
        ids.sort_by(|a, b| {
            let key = |id: &String| {
                models
                    .get(id)
                    .map(|m| (m.account.clone(), m.model.clone()))
                    .unwrap_or_else(|| (id.clone(), String::new()))
            };
            key(a).cmp(&key(b))
        });
        ids
    }

    /// Ids for the current tab, filtered by the search query (matched against
    /// the id, vendor/preset, and model name).
    fn filtered_ids(&self, config: &Config) -> Vec<String> {
        let mut all = match self.tab {
            Tab::Accounts => Self::account_ids(config),
            Tab::Models => Self::model_ids(config),
        };
        let models = config.logical_models();
        // Drill-in: on the Models tab, restrict to a single account when the
        // user entered via ↵ on an account row (Tab / Esc clears it).
        if self.tab == Tab::Models {
            if let Some(acct) = &self.account_filter {
                all.retain(|id| models.get(id).is_some_and(|m| &m.account == acct));
            }
        }
        if self.query.trim().is_empty() {
            return all;
        }
        let q = self.query.to_lowercase();
        let accounts = config.logical_accounts();
        all.into_iter()
            .filter(|id| {
                if id.to_lowercase().contains(&q) {
                    return true;
                }
                match self.tab {
                    Tab::Accounts => accounts
                        .get(id)
                        .is_some_and(|a| a.provider.to_lowercase().contains(&q)),
                    Tab::Models => models.get(id).is_some_and(|m| {
                        m.model.to_lowercase().contains(&q) || m.account.to_lowercase().contains(&q)
                    }),
                }
            })
            .collect()
    }

    /// Switch to `tab`, resetting the selection and clearing both filters (the
    /// search query and the account drill-in) so the destination shows its full
    /// list.
    fn switch_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.selected = 0;
        self.query.clear();
        self.account_filter = None;
        self.pending_delete = None;
    }

    /// Keep the panel open after creating an account and drill directly into
    /// that account's model list.
    fn show_models_for_account(&mut self, account_id: &str) {
        self.tab = Tab::Models;
        self.selected = 0;
        self.mode = Mode::List;
        self.query.clear();
        self.account_filter = Some(account_id.to_string());
        self.pending_delete = None;
    }

    /// Arm a row on the first Ctrl+D and confirm it on the second. Returning
    /// true means the caller should perform the destructive operation.
    fn confirm_double_delete(&mut self, id: &str, is_account: bool) -> bool {
        let target = (id.to_string(), is_account);
        if self.pending_delete.as_ref() == Some(&target) {
            self.pending_delete = None;
            true
        } else {
            self.pending_delete = Some(target);
            false
        }
    }

    /// Selectable row count, including the trailing "+ add provider" row on the
    /// 账号 tab.
    fn current_len(&self, config: &Config) -> usize {
        self.filtered_ids(config).len() + usize::from(self.tab == Tab::Accounts)
    }

    fn selected_id(&self, config: &Config) -> Option<String> {
        let ids = self.filtered_ids(config);
        // The virtual add row sits just past the real accounts on the 账号 tab.
        if self.tab == Tab::Accounts && self.selected == ids.len() {
            return Some(ADD_PROVIDER_ROW.to_string());
        }
        ids.get(self.selected).cloned()
    }

    /// Persist the add form as one provider ACCOUNT (no model — models are added
    /// on the 模型 tab). Returns the new account id when saved so the caller can
    /// drill into its model list; `None` keeps the add form open.
    fn save_add(
        &self,
        form: &AddForm,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Option<String> {
        let preset = form.preset();
        // A fully-custom provider requires a name (it becomes the account id).
        let mut base_id = sanitize_account_name(form.name.trim());
        if base_id.is_empty() {
            return None;
        }
        // Don't let a user account land in the CodingPlan (`AtomGit*`) namespace,
        // or it'd be misclassified as gateway-managed (undeletable, never prompts
        // for a key).
        if atomcode_config::config::is_codingplan_provider_name(&base_id) {
            base_id = format!("custom-{base_id}");
        }
        let account_id = unique_account_id(&base_id, ctx);
        // base_url is pre-filled with the preset default and editable. Persist
        // only a genuine override; blank + no preset default = missing endpoint.
        let base_url = {
            let b = form.base_url.trim();
            if b.is_empty() {
                if preset.default_base_url.is_none() {
                    return None; // custom endpoint requires a URL
                }
                None
            } else if Some(b) == preset.default_base_url {
                None // equals the preset default — keep config clean
            } else {
                Some(b.to_string())
            }
        };
        let account = ProviderAccountConfig {
            provider: preset.id.to_string(),
            display_name: None,
            api_key: {
                let k = form.api_key.trim();
                (!k.is_empty()).then(|| k.to_string())
            },
            base_url,
            user_agent: None,
            skip_tls_verify: false,
            enterprise_url: None,
            ephemeral: false,
        };
        let mut desired = ctx.config.clone();
        desired
            .provider_accounts
            .insert(account_id.clone(), account);
        save_and_reload(
            ctx,
            desired,
            renderer,
            crate::i18n::t(crate::i18n::Msg::ProviderAdded { name: &account_id }).into_owned(),
            true,
        )
        .then_some(account_id)
    }

    /// Build an edit form pre-filled from the selected account.
    fn open_edit(config: &Config, id: &str) -> EditForm {
        let is_legacy =
            !config.provider_accounts.contains_key(id) && config.providers.contains_key(id);
        let (base_url, provider) = if is_legacy {
            let p = config.providers.get(id);
            (
                p.and_then(|p| p.base_url.clone()),
                p.map(|p| p.provider_type.clone()).unwrap_or_default(),
            )
        } else {
            let a = config.provider_accounts.get(id);
            (
                a.and_then(|a| a.base_url.clone()),
                a.map(|a| a.provider.clone()).unwrap_or_default(),
            )
        };
        // Map the stored provider to a protocol toggle (OpenAI/Anthropic
        // compatible). original == preset so a no-op edit leaves the real stored
        // provider (e.g. "deepseek"/"openai") untouched (see save_edit's guard).
        let anthropic = matches!(
            provider_preset::preset_or_compatible(&provider).provider_type,
            provider_preset::ProviderType::Anthropic
        );
        let preset_idx = compat_preset_idx(anthropic);
        let vendor_locked = atomcode_config::config::is_codingplan_provider_name(id);
        EditForm {
            id: id.to_string(),
            is_legacy,
            preset_idx,
            original_preset_idx: preset_idx,
            vendor_locked,
            api_key: String::new(),
            base_url: base_url.unwrap_or_default(),
            // Locked accounts start on the only editable field.
            focus: if vendor_locked {
                FormField::BaseUrl
            } else {
                FormField::Preset
            },
        }
    }

    /// Apply an account edit in place (blank fields keep the current value), save.
    fn save_edit(&self, form: &EditForm, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) -> bool {
        let api_key = form.api_key.trim();
        let base_url = form.base_url.trim();
        let preset = form.preset();
        // Only rewrite the vendor when the user actually changed it (and the
        // account isn't gateway-locked) — a no-op edit must not normalize a
        // `deepseek`/custom provider to the fallback preset, and a CodingPlan
        // account's wire must never change. When the new preset is keyless, drop
        // any stale api_key.
        let vendor_changed = !form.vendor_locked && form.preset_idx != form.original_preset_idx;
        let clear_key =
            vendor_changed && matches!(preset.auth_kind, provider_preset::AuthKind::None);
        let mut desired = ctx.config.clone();
        if form.is_legacy {
            if let Some(p) = desired.providers.get_mut(&form.id) {
                if vendor_changed {
                    // Legacy dispatches on the wire `type`; store the preset's wire.
                    p.provider_type = preset.provider_type.wire().to_string();
                }
                if clear_key {
                    p.api_key = None;
                } else if !api_key.is_empty() {
                    p.api_key = Some(api_key.to_string());
                }
                if !base_url.is_empty() {
                    p.base_url = Some(base_url.to_string());
                }
            }
        } else if let Some(a) = desired.provider_accounts.get_mut(&form.id) {
            if vendor_changed {
                // New-schema stores the preset id.
                a.provider = preset.id.to_string();
            }
            if clear_key {
                a.api_key = None;
            } else if !api_key.is_empty() {
                a.api_key = Some(api_key.to_string());
            }
            if !base_url.is_empty() {
                a.base_url = Some(base_url.to_string());
            }
        }
        save_and_reload(
            ctx,
            desired,
            renderer,
            crate::i18n::t(crate::i18n::Msg::ProviderUpdated { name: &form.id }).into_owned(),
            true,
        )
    }

    /// Add a model to an existing account, or edit an existing model's wire name
    /// + window in place (preserving its other fields), then save.
    fn save_model(&self, form: &ModelForm, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) -> bool {
        let account_id = form.account_id().to_string();
        let model_name = form.model.trim();
        if model_name.is_empty() {
            return false;
        }
        // For an unconfigured preset-vendor quick-add, the account id IS the
        // preset id, so fall back to it.
        let preset_id = ctx
            .config
            .logical_accounts()
            .get(&account_id)
            .map(|a| a.provider.clone())
            .unwrap_or_else(|| account_id.clone());
        let wire = provider_preset::preset_or_compatible(&preset_id)
            .provider_type
            .wire();
        let context_window = form
            .window
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|w| *w > 0)
            .unwrap_or_else(|| atomcode_config::config::provider::default_context_window_for(wire));
        let mut desired = ctx.config.clone();
        // Materialize a preset-vendor account on first use (quick-add from the
        // list): create the account with the preset's defaults; its api_key is
        // filled by the key-write block below.
        if form.edit_id.is_none()
            && !desired.provider_accounts.contains_key(&account_id)
            && !atomcode_config::config::is_codingplan_provider_name(&account_id)
        {
            let preset = provider_preset::preset_or_compatible(&account_id);
            desired.provider_accounts.insert(
                account_id.clone(),
                ProviderAccountConfig {
                    provider: account_id.clone(),
                    display_name: None,
                    api_key: None,
                    base_url: preset.default_base_url.map(str::to_string),
                    user_agent: None,
                    skip_tls_verify: false,
                    enterprise_url: None,
                    ephemeral: false,
                },
            );
        }
        let selection_id = if let Some(id) = &form.edit_id {
            // Edit in place — new-schema model or legacy provider.
            if let Some(m) = desired.models.get_mut(id) {
                m.model = model_name.to_string();
                m.context_window = context_window;
            } else if let Some(p) = desired.providers.get_mut(id) {
                p.model = model_name.to_string();
                p.context_window = context_window;
            }
            id.clone()
        } else {
            // Make the selection-id key unique so a slash in the model name (or a
            // repeat add) never silently overwrites a different model profile.
            let base = format!("{account_id}/{model_name}");
            let model_id = if desired.models.contains_key(&base)
                || desired.providers.contains_key(&base)
            {
                (2..)
                    .map(|n| format!("{base}-{n}"))
                    .find(|c| !desired.models.contains_key(c) && !desired.providers.contains_key(c))
                    .unwrap_or(base)
            } else {
                base
            };
            desired.models.insert(
                model_id.clone(),
                ModelProfileConfig {
                    account: account_id,
                    model: model_name.to_string(),
                    display_name: None,
                    system_prompt: None,
                    context_window,
                    max_tokens: None,
                    capable_model: None,
                    thinking_type: None,
                    thinking_keep: None,
                    reasoning_history: None,
                    reasoning_effort: None,
                    thinking_enabled: None,
                    thinking_budget: None,
                    pricing: None,
                },
            );
            model_id
        };
        // A deferred provider api_key entered here fills the account once — all
        // its models share it.
        if form.edit_id.is_none() && form.account_needs_key() {
            let key = form.api_key.trim();
            if !key.is_empty() {
                if let Some(a) = desired.provider_accounts.get_mut(form.account_id()) {
                    a.api_key = Some(key.to_string());
                }
            }
        }
        if form.make_default {
            desired.default_model = Some(selection_id.clone());
        }
        save_and_reload(
            ctx,
            desired,
            renderer,
            crate::i18n::t(crate::i18n::Msg::ProviderPanelModelSaved {
                model: &selection_id,
            })
            .into_owned(),
            true,
        )
    }

    /// Delete the account (and its models) or a single model, then save.
    fn commit_delete(
        &self,
        id: &str,
        is_account: bool,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> bool {
        let mut desired = ctx.config.clone();
        if is_account {
            desired.provider_accounts.remove(id);
            desired.providers.remove(id); // legacy projection
            desired.models.retain(|_, m| m.account != id);
        } else {
            desired.models.remove(id);
            desired.providers.remove(id); // legacy single-model provider
        }
        // Clear a now-dangling default (both the canonical `default_model` and
        // the legacy `default_provider`, so neither points at the deleted entry).
        if desired
            .default_model
            .as_deref()
            .is_some_and(|d| desired.resolve_model(Some(d)).is_err())
        {
            desired.default_model = None;
        }
        if desired
            .resolve_model(Some(&desired.default_provider))
            .is_err()
        {
            desired.default_provider.clear();
        }
        save_and_reload(
            ctx,
            desired,
            renderer,
            crate::i18n::t(crate::i18n::Msg::ProviderDeleted { name: id }).into_owned(),
            true,
        )
    }
}

impl Modal for ProviderPanel {
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // ── Add form ──
        if let Mode::Add(form) = &mut self.mode {
            match code {
                KeyCode::Esc => {
                    self.mode = Mode::List;
                }
                KeyCode::Tab | KeyCode::Down => form.advance_focus(true),
                KeyCode::BackTab | KeyCode::Up => form.advance_focus(false),
                KeyCode::Left if form.focus == FormField::Preset => form.cycle_preset(false),
                KeyCode::Right if form.focus == FormField::Preset => form.cycle_preset(true),
                KeyCode::Char(c) => match form.focus {
                    FormField::Name => form.name.push(c),
                    FormField::BaseUrl => form.base_url.push(c),
                    FormField::ApiKey => form.api_key.push(c),
                    _ => {}
                },
                KeyCode::Backspace => match form.focus {
                    FormField::Name => {
                        form.name.pop();
                    }
                    FormField::BaseUrl => {
                        form.base_url.pop();
                    }
                    FormField::ApiKey => {
                        form.api_key.pop();
                    }
                    _ => {}
                },
                KeyCode::Enter => {
                    let form = form.clone();
                    if let Some(account_id) = self.save_add(&form, ctx, renderer) {
                        self.show_models_for_account(&account_id);
                    } else {
                        // Save refused (missing endpoint): keep editing.
                        self.mode = Mode::Add(form);
                    }
                }
                _ => {}
            }
            self.draw(buf, state, ctx, renderer);
            return Ok(ModalAction::Continue);
        }

        // ── Edit account ──
        if let Mode::EditAccount(form) = &mut self.mode {
            match code {
                KeyCode::Esc => self.mode = Mode::List,
                KeyCode::Tab | KeyCode::Down => form.advance_focus(true),
                KeyCode::BackTab | KeyCode::Up => form.advance_focus(false),
                KeyCode::Left if form.focus == FormField::Preset => form.cycle_preset(false),
                KeyCode::Right if form.focus == FormField::Preset => form.cycle_preset(true),
                KeyCode::Char(c) => match form.focus {
                    FormField::ApiKey => form.api_key.push(c),
                    FormField::BaseUrl => form.base_url.push(c),
                    _ => {}
                },
                KeyCode::Backspace => match form.focus {
                    FormField::ApiKey => {
                        form.api_key.pop();
                    }
                    FormField::BaseUrl => {
                        form.base_url.pop();
                    }
                    _ => {}
                },
                KeyCode::Enter => {
                    let form = form.clone();
                    if self.save_edit(&form, ctx, renderer) {
                        return Ok(ModalAction::Close);
                    }
                    self.mode = Mode::EditAccount(form);
                }
                _ => {}
            }
            self.draw(buf, state, ctx, renderer);
            return Ok(ModalAction::Continue);
        }

        // ── Add / edit model ──
        if let Mode::Model(form) = &mut self.mode {
            match code {
                KeyCode::Esc => self.mode = Mode::List,
                KeyCode::Tab | KeyCode::Down => form.advance_focus(true),
                KeyCode::BackTab | KeyCode::Up => form.advance_focus(false),
                KeyCode::Left if form.focus == ModelField::Account => form.cycle_account(false),
                KeyCode::Right if form.focus == ModelField::Account => form.cycle_account(true),
                KeyCode::Char(' ') if form.focus == ModelField::MakeDefault => {
                    form.make_default = !form.make_default;
                }
                KeyCode::Char(c) => match form.focus {
                    ModelField::ApiKey => form.api_key.push(c),
                    ModelField::Model => form.model.push(c),
                    ModelField::Window if c.is_ascii_digit() => form.window.push(c),
                    _ => {}
                },
                KeyCode::Backspace => match form.focus {
                    ModelField::ApiKey => {
                        form.api_key.pop();
                    }
                    ModelField::Model => {
                        form.model.pop();
                    }
                    ModelField::Window => {
                        form.window.pop();
                    }
                    _ => {}
                },
                KeyCode::Enter => {
                    let form = form.clone();
                    if self.save_model(&form, ctx, renderer) {
                        return Ok(ModalAction::Close);
                    }
                    self.mode = Mode::Model(form);
                }
                _ => {}
            }
            self.draw(buf, state, ctx, renderer);
            return Ok(ModalAction::Continue);
        }

        // ── List mode (plugin-style: type filters, Ctrl+key acts) ──
        let len = self.current_len(&ctx.config);
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        match code {
            // Esc closes the panel outright. Clearing a filter is done with
            // ←→ / Tab (which also switch tabs and reset both filters).
            KeyCode::Esc => return Ok(ModalAction::Close),
            // ← / → jump to that tab; Tab / Shift-Tab toggle (cycle) so you're
            // never stuck. A manual tab switch drops the account drill-in filter
            // (show all) — the search box has no cursor, so arrows are free here.
            KeyCode::Left => self.switch_tab(Tab::Accounts),
            KeyCode::Right => self.switch_tab(Tab::Models),
            KeyCode::Tab | KeyCode::BackTab => {
                let next = match self.tab {
                    Tab::Accounts => Tab::Models,
                    Tab::Models => Tab::Accounts,
                };
                self.switch_tab(next);
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.pending_delete = None;
            }
            KeyCode::Down => {
                if self.selected + 1 < len {
                    self.selected += 1;
                }
                self.pending_delete = None;
            }
            // Ctrl+A: add. Letter keys are reserved for the search filter.
            KeyCode::Char('a') if ctrl => {
                self.pending_delete = None;
                match self.tab {
                    // New account (+ its first model).
                    Tab::Accounts => self.mode = Mode::Add(AddForm::new()),
                    // Add a model to an existing account; if none exist yet, fall
                    // back to creating an account first.
                    Tab::Models => {
                        self.mode =
                            match ModelForm::new_add(&ctx.config, self.account_filter.as_deref()) {
                                Some(f) => Mode::Model(f),
                                None => Mode::Add(AddForm::new()),
                            };
                    }
                }
            }
            // Ctrl+E: edit the selected row.
            KeyCode::Char('e') if ctrl => {
                self.pending_delete = None;
                if let Some(id) = self
                    .selected_id(&ctx.config)
                    .filter(|i| i != ADD_PROVIDER_ROW)
                {
                    self.mode = match self.tab {
                        Tab::Accounts => Mode::EditAccount(Self::open_edit(&ctx.config, &id)),
                        Tab::Models => match ModelForm::new_edit(&ctx.config, &id) {
                            Some(f) => Mode::Model(f),
                            None => Mode::List,
                        },
                    };
                }
            }
            // Ctrl+D twice: the first press arms the selected logical row; the
            // second deletes it without leaving the list for a confirmation UI.
            KeyCode::Char('d') if ctrl => {
                if let Some(id) = self
                    .selected_id(&ctx.config)
                    .filter(|i| i != ADD_PROVIDER_ROW)
                {
                    let is_account = self.tab == Tab::Accounts;
                    // The CodingPlan (AtomGit) provider is managed by /login and
                    // can't be deleted here.
                    if is_account && atomcode_config::config::is_codingplan_provider_name(&id) {
                        self.pending_delete = None;
                    } else if self.confirm_double_delete(&id, is_account)
                        && self.commit_delete(&id, is_account, ctx, renderer)
                    {
                        return Ok(ModalAction::Close);
                    }
                } else {
                    self.pending_delete = None;
                }
            }
            // Type to filter.
            KeyCode::Char(c) if !ctrl => {
                self.query.push(c);
                self.selected = 0;
                self.pending_delete = None;
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.pending_delete = None;
            }
            KeyCode::Enter => {
                self.pending_delete = None;
                if let Some(id) = self.selected_id(&ctx.config) {
                    match self.tab {
                        // Set default + switch session.
                        Tab::Models => {
                            if set_default_provider_and_reload(ctx, &id, renderer) {
                                return Ok(ModalAction::Close);
                            }
                        }
                        Tab::Accounts if id == ADD_PROVIDER_ROW => {
                            self.mode = Mode::Add(AddForm::new());
                        }
                        // Drill into the account: switch to the Models tab
                        // filtered to just this account. Manual Tab / Esc clears
                        // the filter to show all models again.
                        Tab::Accounts => {
                            self.tab = Tab::Models;
                            self.account_filter = Some(id);
                            self.query.clear();
                            self.selected = 0;
                        }
                    }
                }
            }
            _ => self.pending_delete = None,
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, _buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let mut items: Vec<(String, String)> = Vec::new();
        let t0 = tab_chip(
            &crate::i18n::t(crate::i18n::Msg::ProviderPanelTabAccounts),
            self.tab == Tab::Accounts,
        );
        let t1 = tab_chip(
            &crate::i18n::t(crate::i18n::Msg::ProviderPanelTabModels),
            self.tab == Tab::Models,
        );
        items.push((format!("{t0}   {t1}"), String::new()));
        items.push((String::new(), String::new()));

        let mut selected = items.len(); // nothing highlighted by default
        let hint: String; // assigned once per match arm below
                          // Forms use the box-less `PluginInfo` layout; the list uses the `Plugin`
                          // layout whose reserved index-2 slot is rendered as the search box.
        let mut kind = MenuKind::PluginInfo;
        let mut buf = String::new();

        match &self.mode {
            Mode::List => {
                kind = MenuKind::Plugin;
                buf = self.query.clone();
                // Reserved search box (index 2) + blank separator (index 3): the
                // plugin menu renders index 2 as the bordered input field. With
                // the tab bar + blank already pushed, list rows start at
                // LIST_HEADER_ROWS.
                items.push((self.query.clone(), String::new()));
                items.push((String::new(), String::new()));
                let cur = ctx.config.effective_model_selection().unwrap_or_default();
                let accounts = ctx.config.logical_accounts();
                let models = ctx.config.logical_models();
                let default_account = models.get(&cur).map(|m| m.account.clone());
                match self.tab {
                    Tab::Accounts => {
                        let ids = self.filtered_ids(&ctx.config);
                        for id in &ids {
                            let a = accounts.get(id);
                            let count = models.values().filter(|m| m.account == *id).count();
                            // 0-model providers show just the name; configured
                            // ones show "vendor · N 模型 [默认]".
                            let desc = if count == 0 {
                                String::new()
                            } else {
                                let vendor = a.map(|a| a.provider.clone()).unwrap_or_default();
                                let mark = if default_account.as_deref() == Some(id) {
                                    format!(
                                        "  [{}]",
                                        crate::i18n::t(crate::i18n::Msg::ProviderPanelDefaultBadge)
                                    )
                                } else {
                                    String::new()
                                };
                                let model_count =
                                    crate::i18n::t(crate::i18n::Msg::ProviderPanelModelCount {
                                        count,
                                    });
                                format!("{vendor} · {model_count}{mark}")
                            };
                            items.push((Self::account_label(&ctx.config, id), desc));
                        }
                        // Trailing "+ 添加自定义 provider" affordance (also Ctrl+A).
                        items.push(("＋ 添加自定义 provider".to_string(), String::new()));
                        hint = crate::i18n::t(crate::i18n::Msg::ProviderPanelAccountsHint)
                            .into_owned();
                    }
                    Tab::Models => {
                        let ids = self.filtered_ids(&ctx.config);
                        if ids.is_empty() {
                            let msg = if self.query.trim().is_empty() {
                                crate::i18n::t(crate::i18n::Msg::ProviderPanelEmptyModels)
                            } else {
                                crate::i18n::t(crate::i18n::Msg::ProviderPanelNoMatchingModels)
                            };
                            items.push((msg.into_owned(), String::new()));
                        }
                        for id in &ids {
                            let m = models.get(id);
                            let mark = if *id == cur {
                                format!(
                                    "  ● [{}]",
                                    crate::i18n::t(crate::i18n::Msg::ProviderPanelDefaultBadge)
                                )
                            } else {
                                String::new()
                            };
                            let desc = m
                                .map(|m| {
                                    let name = m.display_name.as_deref().unwrap_or(&m.model);
                                    format!("{} · {}{}", m.account, name, mark)
                                })
                                .unwrap_or_default();
                            items.push((id.clone(), desc));
                        }
                        hint = if let Some(acct) = &self.account_filter {
                            crate::i18n::t(crate::i18n::Msg::ProviderPanelFilteredModelsHint {
                                account: acct,
                            })
                            .into_owned()
                        } else {
                            crate::i18n::t(crate::i18n::Msg::ProviderPanelModelsHint).into_owned()
                        };
                    }
                }
                // List rows begin at LIST_HEADER_ROWS (tab bar, blank, search
                // box, blank).
                if self.current_len(&ctx.config) > 0 {
                    selected =
                        (self.selected + LIST_HEADER_ROWS).min(items.len().saturating_sub(1));
                }
            }
            Mode::Add(form) => {
                let p = form.preset();
                let field_row = |label: &str, value: String, focused: bool| {
                    let marker = if focused { "▸ " } else { "  " };
                    (format!("{marker}{label}: {value}"), String::new())
                };
                items.push((
                    crate::i18n::t(crate::i18n::Msg::ProviderPanelAddTitle).into_owned(),
                    String::new(),
                ));
                items.push((String::new(), String::new()));
                let name = if form.name.is_empty() {
                    "(必填)".to_string()
                } else {
                    form.name.clone()
                };
                items.push(field_row("名称", name, form.focus == FormField::Name));
                items.push(field_row(
                    "协议",
                    format!(
                        "‹ {} ›   ({})",
                        form.protocol_label(),
                        crate::i18n::t(crate::i18n::Msg::ProviderPanelSwitchHint)
                    ),
                    form.focus == FormField::Preset,
                ));
                items.push(field_row(
                    &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldBaseUrl),
                    form.base_url.clone(),
                    form.focus == FormField::BaseUrl,
                ));
                if !matches!(p.auth_kind, provider_preset::AuthKind::None) {
                    let masked = "•".repeat(form.api_key.chars().count());
                    let env_hint = p
                        .api_key_env
                        .map(|e| {
                            format!(
                                "   ({})",
                                crate::i18n::t(crate::i18n::Msg::ProviderPanelEnvHint { env: e })
                            )
                        })
                        .unwrap_or_default();
                    items.push(field_row(
                        &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldApiKey),
                        format!("{masked}{env_hint}"),
                        form.focus == FormField::ApiKey,
                    ));
                }
                // Account-only form — model/window/default moved to the 模型 tab.
                hint =
                    "Tab 下一项  ←→ 切协议  ↵ 保存  Esc 返回  （名称必填；模型到模型页加）".into();
            }
            Mode::EditAccount(form) => {
                let field_row = |label: &str, value: String, focused: bool| {
                    let marker = if focused { "▸ " } else { "  " };
                    (format!("{marker}{label}: {value}"), String::new())
                };
                items.push((
                    crate::i18n::t(crate::i18n::Msg::ProviderPanelEditAccountTitle {
                        account: &form.id,
                    })
                    .into_owned(),
                    String::new(),
                ));
                items.push((String::new(), String::new()));
                let p = form.preset();
                if form.vendor_locked {
                    // Gateway-managed: protocol read-only, no api_key.
                    items.push((
                        format!("  协议: {} (锁定)", form.protocol_label()),
                        String::new(),
                    ));
                } else {
                    items.push(field_row(
                        "协议",
                        format!(
                            "‹ {} ›   ({})",
                            form.protocol_label(),
                            crate::i18n::t(crate::i18n::Msg::ProviderPanelSwitchHint)
                        ),
                        form.focus == FormField::Preset,
                    ));
                }
                items.push(field_row(
                    &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldBaseUrl),
                    form.base_url.clone(),
                    form.focus == FormField::BaseUrl,
                ));
                if !form.vendor_locked && !matches!(p.auth_kind, provider_preset::AuthKind::None) {
                    let masked = "•".repeat(form.api_key.chars().count());
                    items.push(field_row(
                        &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldApiKey),
                        format!(
                            "{masked}   ({})",
                            crate::i18n::t(crate::i18n::Msg::ProviderPanelKeepOriginal)
                        ),
                        form.focus == FormField::ApiKey,
                    ));
                }
                hint = if form.vendor_locked {
                    "Tab 下一项  ↵ 保存  Esc 返回  （CodingPlan 仅可改 base_url）".into()
                } else {
                    "Tab 下一项  ←→ 切协议  ↵ 保存  Esc 返回".into()
                };
            }
            Mode::Model(form) => {
                let field_row = |label: &str, value: String, focused: bool| {
                    let marker = if focused { "▸ " } else { "  " };
                    (format!("{marker}{label}: {value}"), String::new())
                };
                let title = if form.edit_id.is_some() {
                    crate::i18n::t(crate::i18n::Msg::ProviderPanelEditModelTitle)
                } else {
                    crate::i18n::t(crate::i18n::Msg::ProviderPanelAddModelTitle)
                };
                items.push((title.into_owned(), String::new()));
                items.push((String::new(), String::new()));
                if form.edit_id.is_some() {
                    // Account locked on edit — show it, not editable.
                    items.push((
                        format!(
                            "  {}: {}",
                            crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldAccount),
                            form.account_id()
                        ),
                        String::new(),
                    ));
                } else {
                    items.push(field_row(
                        &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldAccount),
                        format!(
                            "‹ {} ›   ({})",
                            form.account_id(),
                            crate::i18n::t(crate::i18n::Msg::ProviderPanelSwitchHint)
                        ),
                        form.focus == ModelField::Account,
                    ));
                    // This provider has no api_key yet — collect it once here.
                    if form.account_needs_key() {
                        let masked = "•".repeat(form.api_key.chars().count());
                        items.push(field_row(
                            "api_key",
                            format!("{masked}   (该 provider 尚未配置)"),
                            form.focus == ModelField::ApiKey,
                        ));
                    }
                }
                items.push(field_row(
                    &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldModel),
                    form.model.clone(),
                    form.focus == ModelField::Model,
                ));
                let win = if form.window.is_empty() {
                    format!(
                        "({})",
                        crate::i18n::t(crate::i18n::Msg::ProviderPanelDefaultValue)
                    )
                } else {
                    form.window.clone()
                };
                items.push(field_row(
                    &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldWindow),
                    win,
                    form.focus == ModelField::Window,
                ));
                items.push(field_row(
                    &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldMakeDefault),
                    if form.make_default { "[✓]" } else { "[ ]" }.to_string(),
                    form.focus == ModelField::MakeDefault,
                ));
                hint = crate::i18n::t(crate::i18n::Msg::ProviderPanelModelFormHint).into_owned();
            }
        }

        items.push((format!("— {hint} —"), String::new()));

        let payload = MenuPayload {
            items,
            selected,
            kind,
        };
        let cursor_byte = buf.len();
        renderer.render(UiLine::InputPrompt {
            buf,
            cursor_byte,
            menu: Some(payload),
            status: build_status(state, ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
    }

    fn handle_paste(
        &mut self,
        text: &str,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        let clean = text.trim().lines().next().unwrap_or("").trim();
        match &mut self.mode {
            Mode::Add(form) => match form.focus {
                FormField::ApiKey => form.api_key.push_str(clean),
                FormField::BaseUrl => form.base_url.push_str(clean),
                FormField::Name => form.name.push_str(clean),
                _ => {}
            },
            // Paste into the search filter.
            Mode::List => {
                self.query.push_str(clean);
                self.selected = 0;
                self.pending_delete = None;
            }
            _ => {}
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_form_is_custom_provider_with_protocol_toggle() {
        let mut f = AddForm::new();
        // Fully-custom: name, protocol, base_url, api_key; base_url starts blank.
        assert_eq!(
            f.fields(),
            vec![
                FormField::Name,
                FormField::Preset,
                FormField::BaseUrl,
                FormField::ApiKey,
            ]
        );
        assert!(f.base_url.is_empty());
        assert_eq!(f.protocol_label(), "OpenAI");
        // ←→ toggles between the two protocols only (never a vendor list).
        f.cycle_preset(true);
        assert_eq!(f.protocol_label(), "Anthropic");
        assert_eq!(f.preset().id, "anthropic-compatible");
        f.cycle_preset(true);
        assert_eq!(f.protocol_label(), "OpenAI");
    }

    #[test]
    fn sanitize_account_name_makes_toml_safe_ids() {
        assert_eq!(sanitize_account_name("Xiaomi MiMo"), "Xiaomi-MiMo");
        assert_eq!(sanitize_account_name("my/vendor@v1"), "my-vendor-v1");
        assert_eq!(sanitize_account_name("  --keep_me.1--  "), "keep_me.1");
        assert_eq!(sanitize_account_name("！！！"), "");
    }

    #[test]
    fn open_edit_prefills_and_detects_legacy() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "providers": { "leg": { "type": "openai", "base_url": "https://legacy/v1", "model": "m", "context_window": 8000 } },
            "provider_accounts": { "acc": { "provider": "deepseek", "base_url": "https://mirror/v1" } },
            "models": { "acc/m": { "account": "acc", "model": "x", "context_window": 8000 } }
        }))
        .unwrap();
        let leg = ProviderPanel::open_edit(&cfg, "leg");
        assert!(leg.is_legacy);
        assert_eq!(leg.base_url, "https://legacy/v1");
        // Protocol toggle pre-filled from the wire (openai → OpenAI-compatible),
        // and original == preset so a no-op edit won't rewrite the real provider.
        assert_eq!(leg.protocol_label(), "OpenAI");
        assert_eq!(leg.preset_idx, leg.original_preset_idx);
        let acc = ProviderPanel::open_edit(&cfg, "acc");
        assert!(!acc.is_legacy);
        assert_eq!(acc.base_url, "https://mirror/v1");
        assert!(acc.api_key.is_empty()); // blank = keep existing
                                         // deepseek is openai-wire → OpenAI-compatible toggle.
        assert_eq!(acc.protocol_label(), "OpenAI");
    }

    #[test]
    fn model_form_add_vs_edit() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": { "acc": { "provider": "deepseek" } },
            "models": { "acc/m": { "account": "acc", "model": "deepseek-chat", "context_window": 131072 } }
        }))
        .unwrap();
        // Add: account is a selectable field, defaults to an existing account.
        let add = ModelForm::new_add(&cfg, None).unwrap();
        assert!(add.fields().contains(&ModelField::Account));
        assert_eq!(add.account_id(), "acc");
        // A preferred (drilled-into) account is preselected.
        let cfg2: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": { "a1": { "provider": "deepseek" }, "z9": { "provider": "openai" } },
            "models": { "a1/m": { "account": "a1", "model": "x", "context_window": 8000 } }
        }))
        .unwrap();
        assert_eq!(
            ModelForm::new_add(&cfg2, Some("z9")).unwrap().account_id(),
            "z9"
        );
        // Edit: account locked; model + window pre-filled; id preserved.
        let edit = ModelForm::new_edit(&cfg, "acc/m").unwrap();
        assert!(!edit.fields().contains(&ModelField::Account));
        assert_eq!(edit.model, "deepseek-chat");
        assert_eq!(edit.window, "131072");
        assert_eq!(edit.edit_id.as_deref(), Some("acc/m"));
    }

    #[test]
    fn model_list_groups_by_account() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": { "acc": { "provider": "deepseek" } },
            "models": {
                "acc/z": { "account": "acc", "model": "z", "context_window": 8000 },
                "acc/a": { "account": "acc", "model": "a", "context_window": 8000 }
            }
        }))
        .unwrap();
        assert_eq!(
            ProviderPanel::model_ids(&cfg),
            vec!["acc/a".to_string(), "acc/z".to_string()]
        );
    }

    #[test]
    fn query_filters_accounts_by_id_and_vendor() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "openai-main": { "provider": "openai" },
                "deep": { "provider": "deepseek" }
            },
            "models": {
                "openai-main/gpt": { "account": "openai-main", "model": "gpt", "context_window": 8000 },
                "deep/chat": { "account": "deep", "model": "chat", "context_window": 8000 }
            }
        }))
        .unwrap();
        let mut p = ProviderPanel::open();
        // Empty query → configured accounts + all unconfigured preset vendors.
        let all = p.filtered_ids(&cfg);
        assert!(all.contains(&"openai-main".to_string()) && all.contains(&"deep".to_string()));
        assert!(all.len() > 2, "preset vendors are also listed");
        // Match by id substring: the "deep" account AND the "deepseek" preset.
        p.query = "deep".into();
        let d = p.filtered_ids(&cfg);
        assert!(d.contains(&"deep".to_string()) && d.contains(&"deepseek".to_string()));
        // Match by vendor: "openai-main" (provider openai) surfaces for "openai".
        p.query = "openai".into();
        assert!(p.filtered_ids(&cfg).contains(&"openai-main".to_string()));
        // No match → empty.
        p.query = "zzznomatch".into();
        assert!(p.filtered_ids(&cfg).is_empty());
    }

    #[test]
    fn account_ids_lists_unconfigured_preset_vendors() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": { "AtomGit": { "provider": "openai", "base_url": "https://llm-api.atomgit.com/v1" } }
        }))
        .unwrap();
        let ids = ProviderPanel::account_ids(&cfg);
        assert!(
            ids.first() == Some(&"AtomGit".to_string()),
            "configured first"
        );
        assert_eq!(
            ids.get(1).map(String::as_str),
            Some("taotoken"),
            "TaoToken should be the first quick-add vendor below AtomGit"
        );
        assert!(
            ids.contains(&"deepseek".to_string()),
            "unconfigured vendor listed"
        );
        // Custom-endpoint presets are reached via the add-custom row, not listed.
        assert!(!ids.contains(&"openai-compatible".to_string()));
        assert!(!ids.contains(&"anthropic-compatible".to_string()));
        // The lowercase "atomgit" gateway preset must NOT be quick-addable as a
        // raw-key account — it has to go through the CodingPlan OAuth signer.
        assert!(!ids.contains(&"atomgit".to_string()));
        // A preset without a default endpoint (nothing to dispatch against) is
        // not listed either.
        assert!(!ids.contains(&"xiaomi-mimo".to_string()));
        // A keyed preset vendor prompts for a key when you add its first model.
        assert!(account_needs_key(&cfg, "deepseek"));
        assert!(!account_needs_key(&cfg, "AtomGit"));
    }

    #[test]
    fn account_label_uses_preset_display_name_without_replacing_custom_ids() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "custom-openai": { "provider": "openai" },
                "named": { "provider": "openai", "display_name": "My Gateway" },
                "taotoken": { "provider": "taotoken" }
            }
        }))
        .unwrap();

        assert_eq!(ProviderPanel::account_label(&cfg, "taotoken"), "TaoToken");
        assert_eq!(
            ProviderPanel::account_label(&Config::default(), "taotoken"),
            "TaoToken"
        );
        assert_eq!(
            ProviderPanel::account_label(&cfg, "custom-openai"),
            "custom-openai"
        );
        assert_eq!(ProviderPanel::account_label(&cfg, "named"), "My Gateway");
    }

    #[test]
    fn edit_codingplan_account_locks_vendor_and_key() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "AtomGit": { "provider": "openai", "base_url": "https://llm-api.atomgit.com/v1" },
                "custom": { "provider": "openai-compatible", "base_url": "https://x/v1", "api_key": "sk-1" }
            }
        }))
        .unwrap();
        let locked = ProviderPanel::open_edit(&cfg, "AtomGit");
        assert!(locked.vendor_locked);
        // Only base_url is editable — no protocol toggle, no api_key.
        assert_eq!(locked.fields(), vec![FormField::BaseUrl]);
        // A user account is not locked.
        assert!(!ProviderPanel::open_edit(&cfg, "custom").vendor_locked);
    }

    #[test]
    fn model_form_prompts_for_key_on_keyless_provider() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": {
                "custom": { "provider": "openai-compatible", "base_url": "https://x/v1" },
                "keyed": { "provider": "openai-compatible", "base_url": "https://y/v1", "api_key": "sk-1" }
            }
        }))
        .unwrap();
        assert!(account_needs_key(&cfg, "custom"));
        assert!(!account_needs_key(&cfg, "keyed"));
        // CodingPlan uses the gateway signer — never prompt.
        assert!(!account_needs_key(&cfg, "AtomGit"));
        // The model form shows an api_key field only for the keyless provider.
        assert!(ModelForm::new_add(&cfg, Some("custom"))
            .unwrap()
            .fields()
            .contains(&ModelField::ApiKey));
        assert!(!ModelForm::new_add(&cfg, Some("keyed"))
            .unwrap()
            .fields()
            .contains(&ModelField::ApiKey));
    }

    #[test]
    fn account_filter_restricts_models_tab_to_one_account() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": { "AtomGit": { "provider": "openai" }, "other": { "provider": "openai" } },
            "models": {
                "AtomGit-a": { "account": "AtomGit", "model": "a", "context_window": 8000 },
                "AtomGit-b": { "account": "AtomGit", "model": "b", "context_window": 8000 },
                "other/x": { "account": "other", "model": "x", "context_window": 8000 }
            }
        }))
        .unwrap();
        let mut p = ProviderPanel::open();
        p.tab = Tab::Models;
        // No filter → all models.
        assert_eq!(p.filtered_ids(&cfg).len(), 3);
        // Drill into AtomGit → only its two models.
        p.account_filter = Some("AtomGit".into());
        assert_eq!(
            p.filtered_ids(&cfg),
            vec!["AtomGit-a".to_string(), "AtomGit-b".to_string()]
        );
        // A typed query narrows further, within the account.
        p.query = "b".into();
        assert_eq!(p.filtered_ids(&cfg), vec!["AtomGit-b".to_string()]);
        // The account filter only applies to the Models tab; the Accounts tab
        // lists both configured accounts (plus preset vendors).
        p.query.clear();
        p.tab = Tab::Accounts;
        let acc = p.filtered_ids(&cfg);
        assert!(acc.contains(&"AtomGit".to_string()) && acc.contains(&"other".to_string()));
    }

    #[test]
    fn added_account_stays_open_on_its_models_page() {
        let mut panel = ProviderPanel::open();
        panel.selected = 3;
        panel.query = "stale".into();
        panel.pending_delete = Some(("old".into(), true));

        panel.show_models_for_account("taotoken");

        assert!(panel.tab == Tab::Models);
        assert_eq!(panel.selected, 0);
        assert!(matches!(panel.mode, Mode::List));
        assert_eq!(panel.account_filter.as_deref(), Some("taotoken"));
        assert!(panel.query.is_empty());
        assert!(panel.pending_delete.is_none());
    }

    #[test]
    fn query_filters_models_by_name_and_account() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": { "acc": { "provider": "deepseek" } },
            "models": {
                "acc/chat": { "account": "acc", "model": "deepseek-chat", "context_window": 8000 },
                "acc/reason": { "account": "acc", "model": "deepseek-reasoner", "context_window": 8000 }
            }
        }))
        .unwrap();
        let mut p = ProviderPanel::open();
        p.tab = Tab::Models;
        // Match by model name substring.
        p.query = "reason".into();
        assert_eq!(p.filtered_ids(&cfg), vec!["acc/reason".to_string()]);
        // Account name matches both models.
        p.query = "acc".into();
        assert_eq!(p.filtered_ids(&cfg).len(), 2);
    }

    #[test]
    fn delete_requires_two_presses_on_the_same_logical_row() {
        let mut panel = ProviderPanel::open();

        assert!(!panel.confirm_double_delete("account-a", true));
        assert_eq!(panel.pending_delete, Some(("account-a".to_string(), true)));
        assert!(panel.confirm_double_delete("account-a", true));
        assert!(panel.pending_delete.is_none());

        assert!(!panel.confirm_double_delete("account-a", true));
        assert!(!panel.confirm_double_delete("account-b", true));
        assert_eq!(panel.pending_delete, Some(("account-b".to_string(), true)));

        // An account and model with the same id are still distinct targets.
        assert!(!panel.confirm_double_delete("account-b", false));
    }
}
