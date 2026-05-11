// crates/atomcode-tuix/src/modals/language_picker.rs
//
// `/language` modal — language picker.
//
// Lists available locales with the current one pre-selected.
// Up/Down navigates, Enter selects (persists to config + switches
// global locale), Esc cancels. Renders as a MenuPayload above the
// input box.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, Buffer, LoopCtx};
use crate::i18n::{self, Locale};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

pub struct LanguagePicker {
    options: Vec<(Locale, String, String)>,
    selected: usize,
}

impl LanguagePicker {
    pub fn open() -> Self {
        let current = i18n::current_locale();
        let options = vec![
            (Locale::En, "English".to_string(), "en".to_string()),
            (Locale::ZhCn, "简体中文".to_string(), "zh_CN".to_string()),
        ];
        let selected = options
            .iter()
            .position(|(loc, _, _)| *loc == current)
            .unwrap_or(0);
        Self { options, selected }
    }
}

impl Modal for LanguagePicker {
    fn handle_key(
        &mut self,
        code: KeyCode,
        _mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        match code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Down => {
                let max = self.options.len().saturating_sub(1);
                if self.selected < max {
                    self.selected += 1;
                }
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Enter => {
                let (locale, label, _) = &self.options[self.selected];
                let locale = *locale;
                let label = label.clone();
                i18n::set_locale(locale);
                ctx.config.language = Some(locale);
                let config_path = atomcode_core::config::Config::default_path();
                if let Err(e) = ctx.config.save(&config_path) {
                    eprintln!("[language] failed to save config: {e}");
                }
                renderer.render(UiLine::CommandOutput(
                    format!("  Language set to: {label} ({locale})\n"),
                ));
                renderer.flush();
                Ok(ModalAction::Close)
            }
            KeyCode::Esc => Ok(ModalAction::Close),
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let items: Vec<(String, String)> = self
            .options
            .iter()
            .map(|(_, label, hint)| (label.clone(), hint.clone()))
            .collect();
        let payload = MenuPayload {
            items,
            selected: self.selected,
        };
        renderer.render(UiLine::InputPrompt {
            buf: buf.text.clone(),
            cursor_byte: buf.cursor,
            menu: Some(payload),
            status: build_status(state, ctx),
        });
        renderer.flush();
    }
}
