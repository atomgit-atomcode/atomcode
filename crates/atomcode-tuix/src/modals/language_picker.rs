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
                // Flip the global locale FIRST so the confirmation
                // below renders in the just-picked language. Without
                // this the "switched to 简体中文" line still comes
                // back in English on a zh_CN selection.
                i18n::set_locale(locale);
                ctx.config.language = Some(locale);
                let config_path = atomcode_core::config::Config::default_path();
                if let Err(e) = ctx.config.save(&config_path) {
                    // TODO: surface via renderer once a non-modal error display is available
                    eprintln!("[language] failed to save config: {e}");
                }
                renderer.render(UiLine::CommandOutput(
                    crate::i18n::t(crate::i18n::Msg::LanguageSwitched {
                        label: &label,
                        locale: &locale.to_string(),
                    })
                    .into_owned(),
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
            kind: crate::render::MenuKind::SlashCommand,
        };
        renderer.render(UiLine::InputPrompt {
            buf: buf.text.clone(),
            cursor_byte: buf.cursor,
            menu: Some(payload),
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
    fn open_selects_current_locale() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(Locale::ZhCn);
        let picker = LanguagePicker::open();
        assert_eq!(picker.selected, 1); // ZhCn is second option
    }

    #[test]
    fn open_defaults_to_en() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(Locale::En);
        let picker = LanguagePicker::open();
        assert_eq!(picker.selected, 0); // En is first option
    }

    /// Switching to zh_CN renders a Chinese confirmation line that
    /// includes the success checkmark + the picked label + the locale
    /// code. Regression guard for "no feedback after picking
    /// a language" — the Enter handler is supposed to push a
    /// CommandOutput line with these three markers visible, in the
    /// freshly-picked locale.
    #[test]
    fn switch_confirmation_zh_cn_has_checkmark_label_and_locale() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(Locale::ZhCn);
        let msg = crate::i18n::t(crate::i18n::Msg::LanguageSwitched {
            label: "简体中文",
            locale: "zh_CN",
        });
        assert!(msg.contains("✓"), "missing checkmark: {}", msg);
        assert!(msg.contains("简体中文"), "missing label: {}", msg);
        assert!(msg.contains("zh_CN"), "missing locale code: {}", msg);
        assert!(msg.contains("已切换"), "missing '已切换' verb: {}", msg);
        assert!(msg.ends_with('\n'), "missing trailing newline: {:?}", msg);
    }

    #[test]
    fn switch_confirmation_en_has_checkmark_label_and_locale() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(Locale::En);
        let msg = crate::i18n::t(crate::i18n::Msg::LanguageSwitched {
            label: "English",
            locale: "en",
        });
        assert!(msg.contains("✓"), "missing checkmark: {}", msg);
        assert!(msg.contains("English"), "missing label: {}", msg);
        assert!(msg.contains("(en)"), "missing locale code: {}", msg);
        assert!(msg.to_lowercase().contains("switched"), "missing 'switched' verb: {}", msg);
        assert!(msg.ends_with('\n'), "missing trailing newline: {:?}", msg);
    }
}
