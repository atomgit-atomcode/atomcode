//! Searchable half-screen `/config` editor.

use anyhow::Result;
use atomcode_config::settings::{
    patch_selection_retry_max_attempts, selection_retry_max_attempts, ApplyPolicy, SettingKind,
    SettingSpec, SETTINGS,
};
use atomcode_config::Config;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{
    backspace_at_cursor, delete_at_cursor, insert_at_cursor, next_grapheme_boundary,
    previous_grapheme_boundary, Modal, ModalAction,
};
use crate::event_loop::{
    apply_config_panel_commit, build_status, Buffer, LoopCtx, PersistedConfigReload,
};
use crate::render::{MenuKind, MenuPayload, Renderer, UiLine};
use crate::state::UiState;

const RETRY_SETTING_ID: &str = "model.retry_max_attempts";

#[derive(Clone, Copy)]
enum PanelSetting {
    Static(&'static SettingSpec),
    RetryMaxAttempts,
}

impl PanelSetting {
    fn id(self) -> &'static str {
        match self {
            Self::Static(setting) => setting.id,
            Self::RetryMaxAttempts => RETRY_SETTING_ID,
        }
    }

    fn kind(self) -> SettingKind {
        match self {
            Self::Static(setting) => setting.kind,
            Self::RetryMaxAttempts => SettingKind::Integer {
                min: 1,
                max: u32::MAX as i64,
            },
        }
    }

    fn apply(self) -> ApplyPolicy {
        match self {
            Self::Static(setting) => setting.apply,
            Self::RetryMaxAttempts => ApplyPolicy::AgentReassemble,
        }
    }

    fn matches(self, query: &str) -> bool {
        match self {
            Self::Static(setting) => setting.matches(query),
            Self::RetryMaxAttempts => {
                let query = query.trim().to_lowercase();
                query.is_empty()
                    || RETRY_SETTING_ID.contains(&query)
                    || "retry attempts current model".contains(&query)
                    || "重试次数当前模型".contains(&query)
            }
        }
    }

    fn label(self, zh: bool, selection: &str) -> String {
        match self {
            Self::Static(setting) => if zh {
                setting.label_zh
            } else {
                setting.label_en
            }
            .to_string(),
            Self::RetryMaxAttempts if zh => format!("最大重试次数（当前模型：{selection}）"),
            Self::RetryMaxAttempts => format!("Retry attempts (current model: {selection})"),
        }
    }

    fn value(self, config: &Config, selection: &str) -> String {
        match self {
            Self::Static(setting) => setting.value(config),
            Self::RetryMaxAttempts => selection_retry_max_attempts(config, selection)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "auto".into()),
        }
    }
}

pub struct ConfigPanel {
    query: String,
    query_cursor_byte: usize,
    search_focused: bool,
    selected: usize,
    editing: Option<PanelSetting>,
    edit_value: String,
    edit_cursor_byte: usize,
    replace_edit_value_on_input: bool,
    pending_reset: Option<&'static str>,
}

impl ConfigPanel {
    pub fn open() -> Self {
        Self {
            query: String::new(),
            query_cursor_byte: 0,
            search_focused: false,
            selected: 0,
            editing: None,
            edit_value: String::new(),
            edit_cursor_byte: 0,
            replace_edit_value_on_input: false,
            pending_reset: None,
        }
    }

    fn filtered(&self) -> Vec<PanelSetting> {
        SETTINGS
            .iter()
            .map(PanelSetting::Static)
            .chain(std::iter::once(PanelSetting::RetryMaxAttempts))
            .filter(|setting| setting.matches(&self.query))
            .collect()
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_down(&mut self) {
        let len = self.filtered().len();
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    fn selected_setting(&self) -> Option<PanelSetting> {
        self.filtered().get(self.selected).copied()
    }

    fn save(
        &mut self,
        setting: PanelSetting,
        next: Option<&str>,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<()> {
        let mut previous_document = None;
        let selection = ctx.provider_selection.clone();
        let commit = ctx.config_store.update_document(|document| {
            previous_document = Some(document.to_string());
            match (setting, next) {
                (PanelSetting::Static(setting), Some(next)) => setting.patch(document, next),
                (PanelSetting::Static(setting), None) => {
                    setting.reset(document);
                    Ok(())
                }
                (PanelSetting::RetryMaxAttempts, next) => {
                    patch_selection_retry_max_attempts(document, &selection, next)
                }
            }
        })?;
        let committed_value = setting.value(&commit.snapshot.config, &selection);
        let success_message = format!("✓ {} = {}", setting.id(), committed_value);
        let outcome = apply_config_panel_commit(
            ctx,
            commit,
            previous_document.unwrap_or_default(),
            setting.apply() == ApplyPolicy::AgentReassemble,
            setting.apply() == ApplyPolicy::CapabilityReprepare,
            matches!(setting, PanelSetting::RetryMaxAttempts),
            success_message.clone(),
        )?;
        if matches!(outcome, PersistedConfigReload::Applied { .. }) {
            crate::sync_history_replay_config(renderer, &ctx.config, &ctx.caps);
            renderer.render(UiLine::Muted(success_message));
        }
        Ok(())
    }

    fn activate(&mut self, ctx: &mut LoopCtx, renderer: &mut dyn Renderer) -> Result<()> {
        let Some(setting) = self.selected_setting() else {
            return Ok(());
        };
        let current = setting.value(&ctx.config, &ctx.provider_selection);
        match setting.kind() {
            SettingKind::Boolean => self.save(
                setting,
                Some(if current == "true" { "false" } else { "true" }),
                ctx,
                renderer,
            ),
            SettingKind::OptionalBoolean => {
                let values = ["auto", "enabled", "disabled"];
                let index = values
                    .iter()
                    .position(|value| *value == current)
                    .unwrap_or(0);
                self.save(
                    setting,
                    Some(values[(index + 1) % values.len()]),
                    ctx,
                    renderer,
                )
            }
            SettingKind::Choice(values) => {
                let index = values
                    .iter()
                    .position(|value| *value == current)
                    .unwrap_or(0);
                self.save(
                    setting,
                    Some(values[(index + 1) % values.len()]),
                    ctx,
                    renderer,
                )
            }
            SettingKind::Integer { .. } | SettingKind::Text => {
                self.editing = Some(setting);
                self.edit_value = current;
                self.edit_cursor_byte = self.edit_value.len();
                self.replace_edit_value_on_input = true;
                Ok(())
            }
        }
    }

    fn draw_payload(&self, ctx: &LoopCtx) -> MenuPayload {
        let filtered = self.filtered();
        let zh = matches!(crate::i18n::current_locale(), crate::i18n::Locale::ZhCn);
        let title = if zh {
            format!("配置 ({} / {})", filtered.len(), SETTINGS.len() + 1)
        } else {
            format!("Config ({} / {})", filtered.len(), SETTINGS.len() + 1)
        };
        let search = if let Some(setting) = self.editing {
            format!("{} = {}", setting.id(), self.edit_value)
        } else {
            self.query.clone()
        };
        let hint = if let Some(id) = self.pending_reset {
            if zh {
                format!("再次按 Delete 恢复 {id} 的默认值")
            } else {
                format!("Press Delete again to reset {id}")
            }
        } else if zh {
            "↑↓ 选择 · Enter 修改 · Delete 恢复默认 · Esc 返回".to_string()
        } else {
            "↑↓ select · Enter change · Delete reset · Esc close".to_string()
        };
        let mut items = vec![
            (title, String::new()),
            (String::new(), String::new()),
            (search, String::new()),
            (String::new(), String::new()),
        ];
        items.extend(filtered.iter().map(|setting| {
            let policy = match setting.apply() {
                ApplyPolicy::ImmediateUi => {
                    if zh {
                        "立即"
                    } else {
                        "now"
                    }
                }
                ApplyPolicy::NextTurn => {
                    if zh {
                        "下一轮"
                    } else {
                        "next turn"
                    }
                }
                ApplyPolicy::AgentReassemble => {
                    if zh {
                        "重新加载"
                    } else {
                        "reload"
                    }
                }
                ApplyPolicy::CapabilityReprepare => {
                    if zh {
                        "重建能力"
                    } else {
                        "reprepare"
                    }
                }
                ApplyPolicy::NextStartup => {
                    if zh {
                        "重启后"
                    } else {
                        "restart"
                    }
                }
            };
            (
                setting.label(zh, &ctx.provider_selection),
                format!(
                    "{} · {policy}",
                    setting.value(&ctx.config, &ctx.provider_selection)
                ),
            )
        }));
        items.push((format!("— {hint} —"), String::new()));
        MenuPayload {
            items,
            selected: if filtered.is_empty() {
                usize::MAX
            } else {
                4 + self.selected
            },
            kind: MenuKind::DirectoryList,
        }
    }
}

impl Modal for ConfigPanel {
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        if let Some(setting) = self.editing {
            match code {
                KeyCode::Enter => {
                    let next = self.edit_value.clone();
                    match self.save(setting, Some(&next), ctx, renderer) {
                        Ok(()) => {
                            self.editing = None;
                            self.edit_value.clear();
                            self.edit_cursor_byte = 0;
                            self.replace_edit_value_on_input = false;
                        }
                        Err(error) => renderer.render(UiLine::Error(error.to_string())),
                    }
                }
                KeyCode::Esc => {
                    self.editing = None;
                    self.edit_value.clear();
                    self.edit_cursor_byte = 0;
                    self.replace_edit_value_on_input = false;
                }
                KeyCode::Backspace => {
                    if self.replace_edit_value_on_input {
                        self.edit_value.clear();
                        self.edit_cursor_byte = 0;
                        self.replace_edit_value_on_input = false;
                    } else {
                        backspace_at_cursor(&mut self.edit_value, &mut self.edit_cursor_byte);
                    }
                }
                KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    if self.replace_edit_value_on_input {
                        self.edit_value.clear();
                        self.edit_cursor_byte = 0;
                        self.replace_edit_value_on_input = false;
                    }
                    insert_at_cursor(
                        &mut self.edit_value,
                        &mut self.edit_cursor_byte,
                        c.encode_utf8(&mut [0; 4]),
                    )
                }
                KeyCode::Left => {
                    self.replace_edit_value_on_input = false;
                    self.edit_cursor_byte =
                        previous_grapheme_boundary(&self.edit_value, self.edit_cursor_byte);
                }
                KeyCode::Right => {
                    self.replace_edit_value_on_input = false;
                    self.edit_cursor_byte =
                        next_grapheme_boundary(&self.edit_value, self.edit_cursor_byte);
                }
                KeyCode::Home => {
                    self.replace_edit_value_on_input = false;
                    self.edit_cursor_byte = 0;
                }
                KeyCode::End => {
                    self.replace_edit_value_on_input = false;
                    self.edit_cursor_byte = self.edit_value.len();
                }
                KeyCode::Delete => {
                    self.replace_edit_value_on_input = false;
                    delete_at_cursor(&mut self.edit_value, &mut self.edit_cursor_byte);
                }
                _ => {}
            }
            self.draw(buf, state, ctx, renderer);
            return Ok(ModalAction::Continue);
        }

        if !matches!(code, KeyCode::Delete) || self.search_focused {
            self.pending_reset = None;
        }

        match code {
            KeyCode::Up => {
                self.search_focused = false;
                self.move_up();
            }
            KeyCode::Down => {
                self.search_focused = false;
                self.move_down();
            }
            KeyCode::Enter => {
                if let Err(error) = self.activate(ctx, renderer) {
                    renderer.render(UiLine::Error(error.to_string()));
                }
            }
            KeyCode::Delete if self.search_focused => {
                delete_at_cursor(&mut self.query, &mut self.query_cursor_byte);
                self.selected = 0;
            }
            KeyCode::Delete => {
                if let Some(setting) = self.selected_setting() {
                    if self.pending_reset == Some(setting.id()) {
                        if let Err(error) = self.save(setting, None, ctx, renderer) {
                            renderer.render(UiLine::Error(error.to_string()));
                        }
                        self.pending_reset = None;
                    } else {
                        self.pending_reset = Some(setting.id());
                    }
                }
            }
            KeyCode::Backspace => {
                backspace_at_cursor(&mut self.query, &mut self.query_cursor_byte);
                self.search_focused = true;
                self.selected = 0;
            }
            KeyCode::Char(c) if !mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                insert_at_cursor(
                    &mut self.query,
                    &mut self.query_cursor_byte,
                    c.encode_utf8(&mut [0; 4]),
                );
                self.search_focused = true;
                self.selected = 0;
            }
            KeyCode::Left => {
                self.search_focused = true;
                self.query_cursor_byte =
                    previous_grapheme_boundary(&self.query, self.query_cursor_byte)
            }
            KeyCode::Right => {
                self.search_focused = true;
                self.query_cursor_byte = next_grapheme_boundary(&self.query, self.query_cursor_byte)
            }
            KeyCode::Home => {
                self.search_focused = true;
                self.query_cursor_byte = 0;
            }
            KeyCode::End => {
                self.search_focused = true;
                self.query_cursor_byte = self.query.len();
            }
            KeyCode::Esc => return Ok(ModalAction::Close),
            _ => return Ok(ModalAction::Continue),
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn handle_paste(
        &mut self,
        text: &str,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        if self.editing.is_some() && self.replace_edit_value_on_input {
            self.edit_value.clear();
            self.edit_cursor_byte = 0;
            self.replace_edit_value_on_input = false;
        }
        let (target, cursor) = if self.editing.is_some() {
            (&mut self.edit_value, &mut self.edit_cursor_byte)
        } else {
            (&mut self.query, &mut self.query_cursor_byte)
        };
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        insert_at_cursor(target, cursor, &clean);
        if self.editing.is_none() {
            self.search_focused = true;
        }
        self.selected = 0;
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, _buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        renderer.render(UiLine::InputPrompt {
            buf: if self.editing.is_some() {
                self.edit_value.clone()
            } else {
                self.query.clone()
            },
            cursor_byte: if self.editing.is_some() {
                self.edit_cursor_byte
            } else {
                self.query_cursor_byte
            },
            menu: Some(self.draw_payload(ctx)),
            status: build_status(state, ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_filters_by_chinese_label_and_id() {
        let mut panel = ConfigPanel::open();
        panel.query = "Todo".into();
        assert!(panel
            .filtered()
            .iter()
            .all(|setting| setting.id().contains("todo")));
        panel.query = "主题".into();
        assert_eq!(panel.filtered()[0].id(), "ui.theme");
    }

    #[test]
    fn navigation_stays_inside_filtered_results() {
        let mut panel = ConfigPanel::open();
        panel.query = "ui.".into();
        for _ in 0..100 {
            panel.move_down();
        }
        assert_eq!(panel.selected + 1, panel.filtered().len());
        for _ in 0..100 {
            panel.move_up();
        }
        assert_eq!(panel.selected, 0);
    }

    #[test]
    fn search_accepts_spaces_for_multi_word_aliases() {
        let mut panel = ConfigPanel::open();
        panel.query = "language server".into();
        assert!(panel
            .filtered()
            .iter()
            .all(|setting| setting.id().starts_with("lsp.")));
    }

    #[test]
    fn retry_setting_is_searchable_in_both_languages() {
        let mut panel = ConfigPanel::open();
        panel.query = "retry".into();
        assert_eq!(panel.filtered()[0].id(), RETRY_SETTING_ID);

        panel.query = "重试".into();
        assert_eq!(panel.filtered()[0].id(), RETRY_SETTING_ID);
    }
}
