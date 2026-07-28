// crates/atomcode-tuix/src/modals/provider_panel.rs
//
// `/provider` full-panel manager, in the style of `/plugin` (PluginManager).
// Two tabs — Accounts and Models — with in-panel forms; the main input box is
// hidden (MenuKind::Plugin). See docs/plans/2026-07-28-provider-panel-ui-design.md.

use anyhow::Result;
use atomcode_config::config::provider::{ModelProfileConfig, ProviderAccountConfig};
use atomcode_config::config::{provider_preset, Config};
use crossterm::event::{KeyCode, KeyModifiers};

use super::provider_wizard::unique_account_id;
use super::{tab_chip, Modal, ModalAction};
use crate::event_loop::{build_status, save_and_reload, set_default_provider_and_reload, Buffer, LoopCtx};
use crate::render::{MenuKind, MenuPayload, Renderer, UiLine};
use crate::state::UiState;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Accounts,
    Models,
}

/// Which add-form field has focus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FormField {
    Preset,
    BaseUrl,
    ApiKey,
    Model,
    Window,
    MakeDefault,
}

#[derive(Clone)]
struct AddForm {
    preset_idx: usize,
    base_url: String,
    api_key: String,
    model: String,
    window: String,
    make_default: bool,
    focus: FormField,
}

impl AddForm {
    fn new(preset_idx: usize) -> Self {
        Self {
            preset_idx,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            window: String::new(),
            make_default: true,
            focus: FormField::Preset,
        }
    }

    fn preset(&self) -> &'static provider_preset::ProviderPreset {
        &provider_preset::PRESETS[self.preset_idx]
    }

    /// The visible field sequence for the current preset (base_url only for
    /// endpoint-less custom presets; api key only for keyed presets).
    fn fields(&self) -> Vec<FormField> {
        let p = self.preset();
        let mut v = vec![FormField::Preset];
        if p.default_base_url.is_none() {
            v.push(FormField::BaseUrl);
        }
        if !matches!(p.auth_kind, provider_preset::AuthKind::None) {
            v.push(FormField::ApiKey);
        }
        v.push(FormField::Model);
        v.push(FormField::Window);
        v.push(FormField::MakeDefault);
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
        // Keep focus valid if the field set changed.
        if !self.fields().contains(&self.focus) {
            self.focus = FormField::Preset;
        }
    }
}

enum Mode {
    List,
    Add(AddForm),
    /// Confirm deleting an account (with its models) or a single model.
    DeleteConfirm { id: String, is_account: bool },
}

pub struct ProviderPanel {
    tab: Tab,
    selected: usize,
    mode: Mode,
}

impl ProviderPanel {
    pub fn open() -> Self {
        Self {
            tab: Tab::Accounts,
            selected: 0,
            mode: Mode::List,
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

    fn current_len(&self, config: &Config) -> usize {
        match self.tab {
            Tab::Accounts => Self::account_ids(config).len(),
            Tab::Models => Self::model_ids(config).len(),
        }
    }

    fn selected_id(&self, config: &Config) -> Option<String> {
        let ids = match self.tab {
            Tab::Accounts => Self::account_ids(config),
            Tab::Models => Self::model_ids(config),
        };
        ids.get(self.selected).cloned()
    }

    /// Persist the add form as one account + one model, optionally default.
    /// Returns true when saved (caller closes), false to stay on the form.
    fn save_add(&self, form: &AddForm, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) -> bool {
        let preset = form.preset();
        let model_name = form.model.trim();
        if model_name.is_empty() {
            return false;
        }
        let account_id = unique_account_id(preset.id, ctx);
        let model_id = format!("{account_id}/{model_name}");
        let base_url = if preset.default_base_url.is_none() {
            let b = form.base_url.trim();
            if b.is_empty() {
                return false; // custom endpoint requires a URL
            }
            Some(b.to_string())
        } else {
            None
        };
        let context_window = form
            .window
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|w| *w > 0)
            .unwrap_or_else(|| {
                atomcode_config::config::provider::default_context_window_for(
                    preset.provider_type.wire(),
                )
            });
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
        let model = ModelProfileConfig {
            account: account_id.clone(),
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
        };
        let mut desired = ctx.config.clone();
        desired.provider_accounts.insert(account_id, account);
        desired.models.insert(model_id.clone(), model);
        if form.make_default {
            desired.default_model = Some(model_id);
        }
        save_and_reload(
            ctx,
            desired,
            renderer,
            crate::i18n::t(crate::i18n::Msg::ProviderAdded {
                name: preset.display_name,
                model: model_name,
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
        // Clear a now-dangling default.
        if desired
            .default_model
            .as_deref()
            .is_some_and(|d| desired.resolve_model(Some(d)).is_err())
        {
            desired.default_model = None;
        }
        save_and_reload(
            ctx,
            desired,
            renderer,
            format!("已删除 {id}"),
            true,
        )
    }
}

impl Modal for ProviderPanel {
    fn handle_key(
        &mut self,
        code: KeyCode,
        _mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // ── Delete confirm ──
        if let Mode::DeleteConfirm { id, is_account } = &self.mode {
            let (id, is_account) = (id.clone(), *is_account);
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    if self.commit_delete(&id, is_account, ctx, renderer) {
                        return Ok(ModalAction::Close);
                    }
                    self.mode = Mode::List;
                }
                _ => self.mode = Mode::List,
            }
            self.draw(buf, state, ctx, renderer);
            return Ok(ModalAction::Continue);
        }

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
                KeyCode::Char(' ') if form.focus == FormField::MakeDefault => {
                    form.make_default = !form.make_default;
                }
                KeyCode::Char(c) => match form.focus {
                    FormField::BaseUrl => form.base_url.push(c),
                    FormField::ApiKey => form.api_key.push(c),
                    FormField::Model => form.model.push(c),
                    FormField::Window if c.is_ascii_digit() => form.window.push(c),
                    _ => {}
                },
                KeyCode::Backspace => match form.focus {
                    FormField::BaseUrl => {
                        form.base_url.pop();
                    }
                    FormField::ApiKey => {
                        form.api_key.pop();
                    }
                    FormField::Model => {
                        form.model.pop();
                    }
                    FormField::Window => {
                        form.window.pop();
                    }
                    _ => {}
                },
                KeyCode::Enter => {
                    let form = form.clone();
                    if self.save_add(&form, ctx, renderer) {
                        return Ok(ModalAction::Close);
                    }
                    // Save refused (empty model / missing URL): keep editing.
                    self.mode = Mode::Add(form);
                }
                _ => {}
            }
            self.draw(buf, state, ctx, renderer);
            return Ok(ModalAction::Continue);
        }

        // ── List mode ──
        let len = self.current_len(&ctx.config);
        match code {
            KeyCode::Esc => return Ok(ModalAction::Close),
            KeyCode::Tab | KeyCode::Right => {
                self.tab = Tab::Models;
                self.selected = 0;
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.tab = Tab::Accounts;
                self.selected = 0;
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                if self.selected + 1 < len {
                    self.selected += 1;
                }
            }
            KeyCode::Char('a') => {
                // Start the add form at the first endpoint-backed preset.
                self.mode = Mode::Add(AddForm::new(0));
            }
            KeyCode::Char('d') => {
                if let Some(id) = self.selected_id(&ctx.config) {
                    self.mode = Mode::DeleteConfirm {
                        id,
                        is_account: self.tab == Tab::Accounts,
                    };
                }
            }
            KeyCode::Enter => {
                if let Some(id) = self.selected_id(&ctx.config) {
                    match self.tab {
                        // Set default + switch session.
                        Tab::Models => {
                            if set_default_provider_and_reload(ctx, &id, renderer) {
                                return Ok(ModalAction::Close);
                            }
                        }
                        // Drill into the account's models.
                        Tab::Accounts => {
                            self.tab = Tab::Models;
                            self.selected = Self::model_ids(&ctx.config)
                                .iter()
                                .position(|mid| {
                                    ctx.config
                                        .logical_models()
                                        .get(mid)
                                        .map(|m| m.account == id)
                                        .unwrap_or(false)
                                })
                                .unwrap_or(0);
                        }
                    }
                }
            }
            _ => {}
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, _buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let mut items: Vec<(String, String)> = Vec::new();
        let t0 = tab_chip("账号", self.tab == Tab::Accounts);
        let t1 = tab_chip("模型", self.tab == Tab::Models);
        items.push((format!("{t0}   {t1}"), String::new()));
        items.push((String::new(), String::new()));
        let header_rows = items.len();

        let mut selected = items.len(); // nothing highlighted by default
        let mut hint = String::new();

        match &self.mode {
            Mode::List => {
                let cur = ctx.config.effective_model_selection().unwrap_or_default();
                let accounts = ctx.config.logical_accounts();
                let models = ctx.config.logical_models();
                let default_account = models.get(&cur).map(|m| m.account.clone());
                match self.tab {
                    Tab::Accounts => {
                        let ids = Self::account_ids(&ctx.config);
                        if ids.is_empty() {
                            items.push(("(尚无 Provider — 按 a 添加第一个)".into(), String::new()));
                        }
                        for id in &ids {
                            let a = accounts.get(id);
                            let count = models.values().filter(|m| m.account == *id).count();
                            let is_legacy = !ctx.config.provider_accounts.contains_key(id)
                                && ctx.config.providers.contains_key(id);
                            let mut left = id.clone();
                            if is_legacy {
                                left.push_str(" [旧]");
                            }
                            let vendor = a.map(|a| a.provider.clone()).unwrap_or_default();
                            let mark = if default_account.as_deref() == Some(id) {
                                "  [默认]"
                            } else {
                                ""
                            };
                            items.push((left, format!("{vendor} · {count} 模型{mark}")));
                        }
                        hint = "a 添加  d 删除  ↵ 展开模型  Tab 切换  Esc 关闭".into();
                    }
                    Tab::Models => {
                        let ids = Self::model_ids(&ctx.config);
                        if ids.is_empty() {
                            items.push(("(尚无模型 — 在账号页按 a 添加)".into(), String::new()));
                        }
                        for id in &ids {
                            let m = models.get(id);
                            let mark = if *id == cur { "  ● [默认]" } else { "" };
                            let desc = m
                                .map(|m| {
                                    let name = m.display_name.as_deref().unwrap_or(&m.model);
                                    format!("{} · {}{}", m.account, name, mark)
                                })
                                .unwrap_or_default();
                            items.push((id.clone(), desc));
                        }
                        hint = "a 添加  d 删除  ↵ 设为默认  Tab 切换  Esc 关闭".into();
                    }
                }
                if self.current_len(&ctx.config) > 0 {
                    selected = (self.selected + header_rows).min(items.len().saturating_sub(1));
                }
            }
            Mode::Add(form) => {
                let p = form.preset();
                let field_row = |label: &str, value: String, focused: bool| {
                    let marker = if focused { "▸ " } else { "  " };
                    (format!("{marker}{label}: {value}"), String::new())
                };
                items.push(("【添加 Provider】".into(), String::new()));
                items.push((String::new(), String::new()));
                items.push(field_row(
                    "厂商",
                    format!("‹ {} ›   (←→ 切换)", p.display_name),
                    form.focus == FormField::Preset,
                ));
                if p.default_base_url.is_none() {
                    items.push(field_row(
                        "base_url",
                        form.base_url.clone(),
                        form.focus == FormField::BaseUrl,
                    ));
                }
                if !matches!(p.auth_kind, provider_preset::AuthKind::None) {
                    let masked = "•".repeat(form.api_key.chars().count());
                    let env_hint = p
                        .api_key_env
                        .map(|e| format!("   (留空用 ${e})"))
                        .unwrap_or_default();
                    items.push(field_row(
                        "api_key",
                        format!("{masked}{env_hint}"),
                        form.focus == FormField::ApiKey,
                    ));
                }
                items.push(field_row(
                    "模型",
                    form.model.clone(),
                    form.focus == FormField::Model,
                ));
                let win = if form.window.is_empty() {
                    "(默认)".to_string()
                } else {
                    form.window.clone()
                };
                items.push(field_row("窗口", win, form.focus == FormField::Window));
                items.push(field_row(
                    "设为默认",
                    if form.make_default { "[✓]" } else { "[ ]" }.to_string(),
                    form.focus == FormField::MakeDefault,
                ));
                hint = "Tab 下一项  ←→ 切厂商  空格 勾选  ↵ 保存  Esc 返回".into();
            }
            Mode::DeleteConfirm { id, is_account } => {
                items.push((String::new(), String::new()));
                let what = if *is_account { "账号(及其模型)" } else { "模型" };
                items.push((format!("确认删除{what} `{id}`？", ), String::new()));
                hint = "y 确认  n/Esc 取消".into();
            }
        }

        items.push((format!("— {hint} —"), String::new()));

        let payload = MenuPayload {
            items,
            selected,
            kind: MenuKind::Plugin,
        };
        renderer.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
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
        if let Mode::Add(form) = &mut self.mode {
            let clean = text.trim().lines().next().unwrap_or("").trim();
            match form.focus {
                FormField::ApiKey => form.api_key.push_str(clean),
                FormField::BaseUrl => form.base_url.push_str(clean),
                FormField::Model => form.model.push_str(clean),
                _ => {}
            }
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset_idx(id: &str) -> usize {
        provider_preset::PRESETS.iter().position(|p| p.id == id).unwrap()
    }

    #[test]
    fn add_form_fields_depend_on_preset() {
        // DeepSeek: keyed with a built-in endpoint → no base_url, has api_key.
        assert_eq!(
            AddForm::new(preset_idx("deepseek")).fields(),
            vec![
                FormField::Preset,
                FormField::ApiKey,
                FormField::Model,
                FormField::Window,
                FormField::MakeDefault
            ]
        );
        // Ollama: keyless → no api_key field.
        assert!(!AddForm::new(preset_idx("ollama"))
            .fields()
            .contains(&FormField::ApiKey));
        // openai-compatible: no default endpoint → base_url required.
        assert!(AddForm::new(preset_idx("openai-compatible"))
            .fields()
            .contains(&FormField::BaseUrl));
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
}
