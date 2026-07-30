use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{Buffer, LoopCtx};
use crate::i18n::{current_locale, Locale};
use crate::render::{DiffPanelRow, DiffPanelSpan, DiffPanelTone, Renderer, UiLine};
use crate::state::UiState;

enum Stage {
    Target,
    Scope,
}

pub struct RewindModal {
    catalog: atomcode_coding::RewindCatalog,
    stage: Stage,
    /// `points.len()` is the synthetic `(current)` row.
    selected_target: usize,
    selected_scope: usize,
}

impl RewindModal {
    pub fn open(catalog: atomcode_coding::RewindCatalog) -> Self {
        let selected_target = catalog.points.len();
        Self {
            catalog,
            stage: Stage::Target,
            selected_target,
            selected_scope: 0,
        }
    }

    fn l<'a>(en: &'a str, zh: &'a str) -> &'a str {
        match current_locale() {
            Locale::ZhCn => zh,
            Locale::En => en,
        }
    }

    fn close(renderer: &mut dyn Renderer) -> ModalAction {
        renderer.render(UiLine::ModalOverlayClear);
        renderer.flush();
        ModalAction::Close
    }

    fn selected_point(&self) -> Option<&atomcode_capabilities::session::RewindPoint> {
        self.catalog.points.get(self.selected_target)
    }

    fn scope(&self) -> atomcode_coding::RewindScope {
        match self.selected_scope {
            1 => atomcode_coding::RewindScope::Code,
            2 => atomcode_coding::RewindScope::ConversationAndCode,
            _ => atomcode_coding::RewindScope::Conversation,
        }
    }

    fn scope_disabled(&self) -> bool {
        self.selected_scope != 0 && self.catalog.code_unavailable.is_some()
    }

    fn target_rows(&self, max_entries: usize) -> Vec<DiffPanelRow> {
        let mut rows = vec![
            row(
                Self::l(
                    "Restore the code and/or conversation to the point before…",
                    "将代码和/或对话恢复到以下提示之前…",
                ),
                DiffPanelTone::Default,
                false,
            ),
            DiffPanelRow::new(Vec::new()),
        ];
        let total = self.catalog.points.len() + 1;
        let window = max_entries.min(total).max(1);
        let start = self
            .selected_target
            .saturating_add(1)
            .saturating_sub(window)
            .min(total.saturating_sub(window));
        let end = (start + window).min(total);
        if start > 0 {
            rows.push(row(
                match current_locale() {
                    Locale::ZhCn => format!("↑ 上方还有 {start} 个回退点"),
                    Locale::En => format!("↑ {start} more above"),
                },
                DiffPanelTone::Muted,
                false,
            ));
            rows.push(DiffPanelRow::new(Vec::new()));
        }
        for index in start..end.min(self.catalog.points.len()) {
            let point = &self.catalog.points[index];
            rows.push(row(
                format!(
                    "{} {}",
                    if index == self.selected_target {
                        "›"
                    } else {
                        " "
                    },
                    point.prompt_preview
                ),
                if index == self.selected_target {
                    DiffPanelTone::Highlight
                } else {
                    DiffPanelTone::Default
                },
                index == self.selected_target,
            ));
            let detail = if point.files.is_empty() {
                Self::l("  No code changes", "  无代码变更").to_string()
            } else if point.files.len() == 1 {
                let file = &point.files[0];
                format!("  {} +{} -{}", file.path, file.additions, file.deletions)
            } else {
                format!(
                    "  {}",
                    match current_locale() {
                        Locale::ZhCn => format!("{} 个文件有变更", point.files.len()),
                        Locale::En => format!("{} files changed", point.files.len()),
                    }
                )
            };
            rows.push(row(detail, DiffPanelTone::Muted, false));
            rows.push(DiffPanelRow::new(Vec::new()));
        }
        if end == total {
            rows.push(row(
                format!(
                    "{} (current)",
                    if self.selected_target == self.catalog.points.len() {
                        "›"
                    } else {
                        " "
                    }
                ),
                if self.selected_target == self.catalog.points.len() {
                    DiffPanelTone::Highlight
                } else {
                    DiffPanelTone::Muted
                },
                self.selected_target == self.catalog.points.len(),
            ));
        } else {
            rows.push(row(
                match current_locale() {
                    Locale::ZhCn => format!("↓ 下方还有 {} 个回退点", total - end),
                    Locale::En => format!("↓ {} more below", total - end),
                },
                DiffPanelTone::Muted,
                false,
            ));
        }
        // Keep the footer visually separated from the final target row.
        rows.push(DiffPanelRow::new(Vec::new()));
        rows
    }

    fn scope_rows(&self) -> Vec<DiffPanelRow> {
        let point = self
            .selected_point()
            .expect("scope selection requires a rewind point");
        let mut rows = vec![
            row(
                format!(
                    "{} “{}”",
                    Self::l("Rewind to before", "回退到此提示之前："),
                    point.prompt_preview
                ),
                DiffPanelTone::Default,
                true,
            ),
            DiffPanelRow::new(Vec::new()),
        ];
        let labels = [
            Self::l("Conversation only", "仅回退对话"),
            Self::l("Code only", "仅回退代码"),
            Self::l("Conversation and code", "回退对话和代码"),
        ];
        for (index, label) in labels.iter().enumerate() {
            let disabled = index != 0 && self.catalog.code_unavailable.is_some();
            let selected = index == self.selected_scope;
            let suffix = if disabled {
                Self::l("  (unavailable)", "  (不可用)")
            } else {
                ""
            };
            rows.push(row(
                format!("{} {label}{suffix}", if selected { "›" } else { " " }),
                if disabled {
                    DiffPanelTone::Muted
                } else if selected {
                    DiffPanelTone::Highlight
                } else {
                    DiffPanelTone::Default
                },
                selected && !disabled,
            ));
        }
        if let Some(reason) = &self.catalog.code_unavailable {
            rows.push(DiffPanelRow::new(Vec::new()));
            rows.push(row(reason, DiffPanelTone::Warning, false));
        }
        rows
    }

    fn redraw(&self, renderer: &mut dyn Renderer) {
        let (win_width, screen_height) = crossterm::terminal::size().unwrap_or((80, 24));
        let rows = match self.stage {
            Stage::Target => {
                let max_entries = ((screen_height as usize).saturating_sub(10) / 3).max(1);
                self.target_rows(max_entries)
            }
            Stage::Scope => self.scope_rows(),
        };
        let footer = match self.stage {
            Stage::Target => Self::l(
                "↑/↓ select · Enter continue · Esc cancel",
                "↑/↓ 选择 · Enter 继续 · Esc 取消",
            ),
            Stage::Scope => Self::l(
                "↑/↓ select · Enter rewind · ← back · Esc cancel",
                "↑/↓ 选择 · Enter 回退 · ← 返回 · Esc 取消",
            ),
        };
        renderer.render(UiLine::DiffPanel {
            // Rewind is the product feature name and remains untranslated.
            title: row("Rewind", DiffPanelTone::Brand, true),
            win_height: (rows.len() + 4).min(screen_height as usize).max(1) as u16,
            rows,
            footer: footer.to_string(),
            win_width,
        });
        renderer.flush();
    }
}

impl Modal for RewindModal {
    fn handle_key(
        &mut self,
        code: KeyCode,
        _mods: KeyModifiers,
        _buf: &mut Buffer,
        _state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        match self.stage {
            Stage::Target => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected_target = self.selected_target.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected_target = self
                        .selected_target
                        .saturating_add(1)
                        .min(self.catalog.points.len());
                }
                KeyCode::Enter if self.selected_target == self.catalog.points.len() => {
                    return Ok(Self::close(renderer));
                }
                KeyCode::Enter => {
                    self.stage = Stage::Scope;
                    self.selected_scope = 0;
                }
                KeyCode::Esc | KeyCode::Char('q') => return Ok(Self::close(renderer)),
                _ => {}
            },
            Stage::Scope => match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected_scope = self.selected_scope.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected_scope = self.selected_scope.saturating_add(1).min(2);
                }
                KeyCode::Left | KeyCode::Backspace => self.stage = Stage::Target,
                KeyCode::Esc | KeyCode::Char('q') => return Ok(Self::close(renderer)),
                KeyCode::Enter if !self.scope_disabled() => {
                    let point = self
                        .selected_point()
                        .expect("scope selection requires a rewind point");
                    if let Err(error) = ctx.runtime.rewind(
                        self.catalog.clone(),
                        point.turn_id,
                        self.scope(),
                        ctx.foreground_runtime_id,
                        ctx.runtime_event_tx.clone(),
                    ) {
                        renderer.render(UiLine::Error(format!(
                            "{}: {error}",
                            Self::l("Could not start Rewind", "无法开始回退")
                        )));
                        renderer.flush();
                        return Ok(ModalAction::Continue);
                    }
                    return Ok(Self::close(renderer));
                }
                _ => {}
            },
        }
        self.redraw(renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, _buf: &Buffer, _state: &UiState, _ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        self.redraw(renderer);
    }
}

fn row(text: impl Into<String>, tone: DiffPanelTone, bold: bool) -> DiffPanelRow {
    let span = DiffPanelSpan::new(text, tone);
    DiffPanelRow::new(vec![if bold { span.bold() } else { span }])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> atomcode_coding::RewindCatalog {
        atomcode_coding::RewindCatalog {
            generation: atomcode_coding::RuntimeGeneration(1),
            revision: 1,
            points: vec![atomcode_capabilities::session::RewindPoint {
                turn_id: 1,
                prompt_number: 1,
                prompt_preview: "first prompt".into(),
                before_tree: "a".repeat(40),
                after_tree: "b".repeat(40),
                files: Vec::new(),
            }],
            code_unavailable: None,
        }
    }

    #[test]
    fn opens_with_current_selected() {
        let modal = RewindModal::open(catalog());
        assert_eq!(modal.selected_target, modal.catalog.points.len());
        assert!(modal.selected_point().is_none());
    }

    #[test]
    fn code_scopes_are_disabled_when_checkpoint_is_unavailable() {
        let mut catalog = catalog();
        catalog.code_unavailable = Some("not a git worktree".into());
        let mut modal = RewindModal::open(catalog);
        modal.stage = Stage::Scope;
        modal.selected_scope = 1;
        assert!(modal.scope_disabled());
        modal.selected_scope = 0;
        assert!(!modal.scope_disabled());
    }

    #[test]
    fn bounded_target_window_keeps_default_current_row_visible() {
        let mut catalog = catalog();
        let template = catalog.points[0].clone();
        catalog.points = (1..=20)
            .map(|turn_id| atomcode_capabilities::session::RewindPoint {
                turn_id,
                prompt_number: turn_id as usize,
                prompt_preview: format!("prompt {turn_id}"),
                ..template.clone()
            })
            .collect();
        let modal = RewindModal::open(catalog);
        let rows = modal.target_rows(4);
        assert!(rows
            .iter()
            .any(|row| { row.spans.iter().any(|span| span.text.contains("(current)")) }));
        assert!(rows.iter().any(|row| {
            row.spans
                .iter()
                .any(|span| span.text.contains("more above") || span.text.contains("上方还有"))
        }));
        assert!(
            rows.last().is_some_and(|row| row.spans.is_empty()),
            "the target list should leave one blank row before the footer"
        );
    }
}
