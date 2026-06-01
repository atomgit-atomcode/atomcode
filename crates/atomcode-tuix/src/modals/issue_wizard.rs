// crates/atomcode-tuix/src/modals/issue_wizard.rs
//
// `/issue` modal — two-step wizard that collects a **new** AtomGit
// issue's title + body, then hands them to the event loop's post-close
// branch which POSTs `/api/v5/repos/{owner}/{repo}/issues` and echoes
// the created issue URL into scrollback.
//
// (Unrelated to `/fixissue <url>`, which pulls an *existing* issue.)

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, Buffer, LoopCtx, NewIssueDraft};
use crate::render::{Renderer, UiLine};
use crate::state::UiState;

/// Which field is currently being edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Title,
    Description,
}

pub struct IssueWizard {
    owner: String,
    repo: String,
    step: Step,
    title: String,
    /// True once `emit_prompt` has printed the step-1 ("Enter title")
    /// header. Kept so step transitions only emit the step-2 header
    /// after advancing (printing it on every redraw would duplicate).
    prompt_shown: bool,
    /// True once the step-2 ("Enter description") header has been
    /// printed into scrollback. Matches `prompt_shown` in purpose.
    desc_prompt_shown: bool,
}

impl IssueWizard {
    pub fn open(owner: String, repo: String) -> Self {
        Self {
            owner,
            repo,
            step: Step::Title,
            title: String::new(),
            prompt_shown: false,
            desc_prompt_shown: false,
        }
    }
}

impl Modal for IssueWizard {
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        match code {
            KeyCode::Esc => {
                push(renderer, &crate::i18n::t(crate::i18n::Msg::IssueCancelled));
                buf.text.clear();
                buf.cursor = 0;
                Ok(ModalAction::Close)
            }
            // Shift+Enter / Alt+Enter in the description step inserts a
            // literal newline so users can write a multi-paragraph body.
            // Step-1 (title) stays single-line: a newline in a title
            // would look wrong on AtomGit anyway.
            KeyCode::Enter
                if self.step == Step::Description
                    && (mods.contains(KeyModifiers::SHIFT) || mods.contains(KeyModifiers::ALT)) =>
            {
                buf.text.push('\n');
                buf.cursor = buf.text.len();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Enter => {
                let entered = buf.text.trim().to_string();
                if entered.is_empty() {
                    let what = match self.step {
                        Step::Title => "title",
                        Step::Description => "description",
                    };
                    push(
                        renderer,
                        &crate::i18n::t(crate::i18n::Msg::IssueRequiredField { field: what }),
                    );
                    return Ok(ModalAction::Continue);
                }
                buf.text.clear();
                buf.cursor = 0;
                match self.step {
                    Step::Title => {
                        self.title = entered;
                        self.step = Step::Description;
                        self.emit_description_prompt(renderer);
                        self.draw(buf, state, ctx, renderer);
                        Ok(ModalAction::Continue)
                    }
                    Step::Description => {
                        // Signal the event loop: it will POST to AtomGit
                        // and render the resulting URL back into the
                        // conversation. Wizard is done.
                        ctx.pending_new_issue = Some(NewIssueDraft {
                            owner: self.owner.clone(),
                            repo: self.repo.clone(),
                            title: std::mem::take(&mut self.title),
                            body: entered,
                        });
                        Ok(ModalAction::Close)
                    }
                }
            }
            KeyCode::Backspace => {
                if !buf.text.is_empty() {
                    let len = buf.text.len();
                    // Pop one char (grapheme-aware is overkill for free-form text).
                    let mut end = len;
                    while end > 0 && !buf.text.is_char_boundary(end - 1) {
                        end -= 1;
                    }
                    if end > 0 {
                        buf.text.truncate(end - 1);
                    }
                    buf.cursor = buf.text.len();
                    self.draw(buf, state, ctx, renderer);
                }
                Ok(ModalAction::Continue)
            }
            KeyCode::Char(c) => {
                buf.text.push(c);
                buf.cursor = buf.text.len();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        renderer.render(UiLine::InputPrompt {
            buf: buf.text.clone(),
            cursor_byte: buf.cursor,
            menu: None,
            status: build_status(state, ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
    }
}

impl IssueWizard {
    /// Called once by the event loop right after installing the wizard.
    /// Prints the "which repo" + step-1 ("enter title") header into
    /// scrollback so the user knows what's being asked.
    pub fn emit_prompt(&mut self, renderer: &mut dyn Renderer) {
        if self.prompt_shown {
            return;
        }
        self.prompt_shown = true;
        push(
            renderer,
            &crate::i18n::t(crate::i18n::Msg::IssueNewOn { owner: &self.owner, repo: &self.repo }),
        );
        push(
            renderer,
            &crate::i18n::t(crate::i18n::Msg::IssueStep1),
        );
    }

    fn emit_description_prompt(&mut self, renderer: &mut dyn Renderer) {
        if self.desc_prompt_shown {
            return;
        }
        self.desc_prompt_shown = true;
        push(renderer, "");
        push(
            renderer,
            &crate::i18n::t(crate::i18n::Msg::IssueTitleConfirmed { title: &abbreviate(&self.title, 80) }),
        );
        push(
            renderer,
            &crate::i18n::t(crate::i18n::Msg::IssueStep2),
        );
    }
}

fn push(renderer: &mut dyn Renderer, text: &str) {
    renderer.render(UiLine::CommandOutput(format!("  {}\n", text)));
    renderer.flush();
}

/// Cap a line to `max` chars for display. Longer strings get a `…` tail
/// so the step-2 "✓ title" summary never blows out one screen row.
fn abbreviate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_starts_on_title_step() {
        let w = IssueWizard::open("o".into(), "r".into());
        assert_eq!(w.step, Step::Title);
        assert!(w.title.is_empty());
        assert!(!w.prompt_shown);
    }

    #[test]
    fn abbreviate_short_string_unchanged() {
        assert_eq!(abbreviate("hello", 80), "hello");
    }

    #[test]
    fn abbreviate_long_string_tail_ellipsis() {
        let long = "x".repeat(100);
        let out = abbreviate(&long, 10);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), 10);
    }
}
