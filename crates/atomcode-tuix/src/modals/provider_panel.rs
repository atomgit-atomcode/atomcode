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

impl AddForm {
    fn new(preset_idx: usize) -> Self {
        Self {
            name: String::new(),
            preset_idx,
            // Pre-fill with the preset's default endpoint (editable — override
            // for a custom deployment).
            base_url: provider_preset::PRESETS[preset_idx]
                .default_base_url
                .map(str::to_string)
                .unwrap_or_default(),
            api_key: String::new(),
            focus: FormField::Name,
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

    fn cycle_preset(&mut self, forward: bool) {
        let n = provider_preset::PRESETS.len();
        self.preset_idx = if forward {
            (self.preset_idx + 1) % n
        } else {
            (self.preset_idx + n - 1) % n
        };
        // Re-prefill the endpoint with the newly-selected preset's default.
        self.base_url = self
            .preset()
            .default_base_url
            .map(str::to_string)
            .unwrap_or_default();
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

/// Edit an existing account's connection/credential. `api_key` blank keeps the
/// current secret; `base_url` is pre-filled and editable.
#[derive(Clone)]
struct EditForm {
    id: String,
    is_legacy: bool,
    api_key: String,
    base_url: String,
    focus: FormField,
}

/// Which model-form field has focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ModelField {
    Account,
    Model,
    Window,
    MakeDefault,
}

/// Add a model to an EXISTING account (the 模型 tab's `a`). Optionally editing an
/// existing model in place (`edit_id` set → account is fixed, id preserved).
#[derive(Clone)]
struct ModelForm {
    account_ids: Vec<String>,
    account_idx: usize,
    model: String,
    window: String,
    make_default: bool,
    focus: ModelField,
    /// When set, this is an edit of an existing model id (account locked).
    edit_id: Option<String>,
}

impl ModelForm {
    fn new_add(config: &Config) -> Option<Self> {
        let account_ids = ProviderPanel::account_ids(config);
        if account_ids.is_empty() {
            return None;
        }
        Some(Self {
            account_ids,
            account_idx: 0,
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
            account_idx: 0,
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

    fn fields(&self) -> Vec<ModelField> {
        if self.edit_id.is_some() {
            vec![
                ModelField::Model,
                ModelField::Window,
                ModelField::MakeDefault,
            ]
        } else {
            vec![
                ModelField::Account,
                ModelField::Model,
                ModelField::Window,
                ModelField::MakeDefault,
            ]
        }
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

    /// Account ids sorted (new-schema + legacy projected), stable.
    fn account_ids(config: &Config) -> Vec<String> {
        let mut ids: Vec<String> = config.logical_accounts().into_keys().collect();
        ids.sort();
        ids
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

    fn current_len(&self, config: &Config) -> usize {
        self.filtered_ids(config).len()
    }

    fn selected_id(&self, config: &Config) -> Option<String> {
        self.filtered_ids(config).get(self.selected).cloned()
    }

    /// Persist the add form as one provider ACCOUNT (no model — models are added
    /// on the 模型 tab). Returns true when saved (caller closes), false to stay.
    fn save_add(&self, form: &AddForm, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) -> bool {
        let preset = form.preset();
        // Custom name → sanitized unique id; blank → derive from the preset.
        let base_id = {
            let sanitized = sanitize_account_name(form.name.trim());
            if sanitized.is_empty() {
                preset.id.to_string()
            } else {
                sanitized
            }
        };
        let account_id = unique_account_id(&base_id, ctx);
        // base_url is pre-filled with the preset default and editable. Persist
        // only a genuine override; blank + no preset default = missing endpoint.
        let base_url = {
            let b = form.base_url.trim();
            if b.is_empty() {
                if preset.default_base_url.is_none() {
                    return false; // custom endpoint requires a URL
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
    }

    /// Build an edit form pre-filled from the selected account.
    fn open_edit(config: &Config, id: &str) -> EditForm {
        let is_legacy =
            !config.provider_accounts.contains_key(id) && config.providers.contains_key(id);
        let base_url = if is_legacy {
            config.providers.get(id).and_then(|p| p.base_url.clone())
        } else {
            config
                .provider_accounts
                .get(id)
                .and_then(|a| a.base_url.clone())
        }
        .unwrap_or_default();
        EditForm {
            id: id.to_string(),
            is_legacy,
            api_key: String::new(),
            base_url,
            focus: FormField::ApiKey,
        }
    }

    /// Apply an account edit in place (blank fields keep the current value), save.
    fn save_edit(&self, form: &EditForm, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) -> bool {
        let api_key = form.api_key.trim();
        let base_url = form.base_url.trim();
        let mut desired = ctx.config.clone();
        if form.is_legacy {
            if let Some(p) = desired.providers.get_mut(&form.id) {
                if !api_key.is_empty() {
                    p.api_key = Some(api_key.to_string());
                }
                if !base_url.is_empty() {
                    p.base_url = Some(base_url.to_string());
                }
            }
        } else if let Some(a) = desired.provider_accounts.get_mut(&form.id) {
            if !api_key.is_empty() {
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
        let preset_id = ctx
            .config
            .logical_accounts()
            .get(&account_id)
            .map(|a| a.provider.clone())
            .unwrap_or_default();
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
                    if self.save_add(&form, ctx, renderer) {
                        return Ok(ModalAction::Close);
                    }
                    // Save refused (missing endpoint): keep editing.
                    self.mode = Mode::Add(form);
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
                KeyCode::Tab | KeyCode::Down | KeyCode::BackTab | KeyCode::Up => {
                    form.focus = if form.focus == FormField::ApiKey {
                        FormField::BaseUrl
                    } else {
                        FormField::ApiKey
                    };
                }
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
                    ModelField::Model => form.model.push(c),
                    ModelField::Window if c.is_ascii_digit() => form.window.push(c),
                    _ => {}
                },
                KeyCode::Backspace => match form.focus {
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
                    Tab::Accounts => self.mode = Mode::Add(AddForm::new(0)),
                    // Add a model to an existing account; if none exist yet, fall
                    // back to creating an account first.
                    Tab::Models => {
                        self.mode = match ModelForm::new_add(&ctx.config) {
                            Some(f) => Mode::Model(f),
                            None => Mode::Add(AddForm::new(0)),
                        };
                    }
                }
            }
            // Ctrl+E: edit the selected row.
            KeyCode::Char('e') if ctrl => {
                self.pending_delete = None;
                if let Some(id) = self.selected_id(&ctx.config) {
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
                if let Some(id) = self.selected_id(&ctx.config) {
                    let is_account = self.tab == Tab::Accounts;
                    if self.confirm_double_delete(&id, is_account)
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
                        if ids.is_empty() {
                            let msg = if self.query.trim().is_empty() {
                                crate::i18n::t(crate::i18n::Msg::ProviderPanelEmptyAccounts)
                            } else {
                                crate::i18n::t(crate::i18n::Msg::ProviderPanelNoMatchingAccounts)
                            };
                            items.push((msg.into_owned(), String::new()));
                        }
                        for id in &ids {
                            let a = accounts.get(id);
                            let count = models.values().filter(|m| m.account == *id).count();
                            let is_legacy = !ctx.config.provider_accounts.contains_key(id)
                                && ctx.config.providers.contains_key(id);
                            let mut left = id.clone();
                            if is_legacy {
                                left.push_str(&format!(
                                    " [{}]",
                                    crate::i18n::t(crate::i18n::Msg::ProviderPanelLegacyBadge)
                                ));
                            }
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
                                crate::i18n::t(crate::i18n::Msg::ProviderPanelModelCount { count });
                            items.push((left, format!("{vendor} · {model_count}{mark}")));
                        }
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
                    format!("({})", p.id) // placeholder: derived from the preset
                } else {
                    form.name.clone()
                };
                items.push(field_row("名称", name, form.focus == FormField::Name));
                items.push(field_row(
                    &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldVendor),
                    format!(
                        "‹ {} ›   ({})",
                        p.display_name,
                        crate::i18n::t(crate::i18n::Msg::ProviderPanelSwitchHint)
                    ),
                    form.focus == FormField::Preset,
                ));
                // Always editable (pre-filled with the preset default).
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
                hint = "Tab 下一项  ←→ 切厂商  ↵ 保存  Esc 返回  （模型到模型页加）".into();
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
                let masked = "•".repeat(form.api_key.chars().count());
                items.push(field_row(
                    &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldApiKey),
                    format!(
                        "{masked}   ({})",
                        crate::i18n::t(crate::i18n::Msg::ProviderPanelKeepOriginal)
                    ),
                    form.focus == FormField::ApiKey,
                ));
                items.push(field_row(
                    &crate::i18n::t(crate::i18n::Msg::ProviderPanelFieldBaseUrl),
                    form.base_url.clone(),
                    form.focus == FormField::BaseUrl,
                ));
                hint = crate::i18n::t(crate::i18n::Msg::ProviderPanelAccountFormHint).into_owned();
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

    fn preset_idx(id: &str) -> usize {
        provider_preset::PRESETS
            .iter()
            .position(|p| p.id == id)
            .unwrap()
    }

    #[test]
    fn add_form_is_account_only_with_name_and_editable_base_url() {
        // Account-only form: name, vendor preset, base_url (always), api key when keyed.
        assert_eq!(
            AddForm::new(preset_idx("deepseek")).fields(),
            vec![
                FormField::Name,
                FormField::Preset,
                FormField::BaseUrl,
                FormField::ApiKey,
            ]
        );
        // base_url is pre-filled with the preset's default endpoint (editable).
        assert!(!AddForm::new(preset_idx("deepseek")).base_url.is_empty());
        // Ollama: keyless → no api_key field.
        assert!(!AddForm::new(preset_idx("ollama"))
            .fields()
            .contains(&FormField::ApiKey));
        // openai-compatible: no default endpoint → base_url blank, still shown.
        let compat = AddForm::new(preset_idx("openai-compatible"));
        assert!(compat.base_url.is_empty());
        assert!(compat.fields().contains(&FormField::BaseUrl));
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
        let acc = ProviderPanel::open_edit(&cfg, "acc");
        assert!(!acc.is_legacy);
        assert_eq!(acc.base_url, "https://mirror/v1");
        assert!(acc.api_key.is_empty()); // blank = keep existing
    }

    #[test]
    fn model_form_add_vs_edit() {
        let cfg: Config = serde_json::from_value(serde_json::json!({
            "provider_accounts": { "acc": { "provider": "deepseek" } },
            "models": { "acc/m": { "account": "acc", "model": "deepseek-chat", "context_window": 131072 } }
        }))
        .unwrap();
        // Add: account is a selectable field, defaults to an existing account.
        let add = ModelForm::new_add(&cfg).unwrap();
        assert!(add.fields().contains(&ModelField::Account));
        assert_eq!(add.account_id(), "acc");
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
        // Empty query → all accounts.
        assert_eq!(p.filtered_ids(&cfg).len(), 2);
        // Match by account id substring.
        p.query = "deep".into();
        assert_eq!(p.filtered_ids(&cfg), vec!["deep".to_string()]);
        // Match by vendor even when the id doesn't contain it.
        p.query = "openai".into();
        assert_eq!(p.filtered_ids(&cfg), vec!["openai-main".to_string()]);
        // No match → empty.
        p.query = "zzz".into();
        assert!(p.filtered_ids(&cfg).is_empty());
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
        // The account filter only applies to the Models tab.
        p.query.clear();
        p.tab = Tab::Accounts;
        assert_eq!(p.filtered_ids(&cfg).len(), 2);
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
