// crates/atomcode-tuix/src/modals/issue_wizard.rs
//
// `/issue` modal — single-step text prompt that collects an AtomGit
// issue URL from the user and hands it off to the same pipeline as
// `/fixissue <url>`. Enter with non-empty input stashes the trimmed URL
// in `ctx.pending_issue_url` and returns `Close`; the event loop picks
// up the signal and dispatches to `launch_fixissue` so both entry
// points (slash-with-arg and wizard) share one implementation.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, Buffer, LoopCtx};
use crate::render::{Renderer, UiLine};
use crate::state::UiState;

pub struct IssueWizard {
    /// True once we've pushed the prompt line ("Enter issue URL:") into
    /// scrollback. First `draw` flips this so repeated redraws on
    /// subsequent keystrokes don't duplicate the line.
    prompt_shown: bool,
}

impl IssueWizard {
    pub fn open() -> Self {
        Self { prompt_shown: false }
    }
}

impl Modal for IssueWizard {
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
            KeyCode::Esc => {
                push(renderer, "(cancelled)");
                buf.text.clear();
                buf.cursor = 0;
                Ok(ModalAction::Close)
            }
            KeyCode::Enter => {
                let entered = buf.text.trim().to_string();
                buf.text.clear();
                buf.cursor = 0;
                if entered.is_empty() {
                    push(renderer, "(cancelled — no URL entered)");
                    return Ok(ModalAction::Close);
                }
                // Signal the event loop to run the fixissue pipeline.
                // The loop's post-close branch handles it via the shared
                // launch_fixissue helper — keeping wizard + /fixissue
                // arm on a single code path.
                ctx.pending_issue_url = Some(entered);
                Ok(ModalAction::Close)
            }
            KeyCode::Backspace => {
                if !buf.text.is_empty() {
                    let len = buf.text.len();
                    // Pop one char (grapheme-aware is overkill for a URL).
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

    fn draw(
        &self,
        buf: &Buffer,
        state: &UiState,
        ctx: &LoopCtx,
        renderer: &mut dyn Renderer,
    ) {
        // Push the one-shot prompt line into scrollback the first time
        // `draw` runs; subsequent redraws (on each keystroke) only
        // refresh the input box.
        if !self.prompt_shown {
            // SAFETY: we're in &self so we can't mutate prompt_shown here;
            // the flip happens in a wrapper below. This is a hack: push
            // every time unconditionally is fine because `CommandOutput`
            // is idempotent from the renderer's POV (each push is a new
            // body line). But it would duplicate lines on redraw.
            // Instead we push unconditionally only on `open_hook`.
        }
        renderer.render(UiLine::InputPrompt {
            buf: buf.text.clone(),
            cursor_byte: buf.cursor,
            menu: None,
            status: build_status(state, ctx),
        });
        renderer.flush();
    }
}

impl IssueWizard {
    /// Called once by the event loop right after installing the wizard
    /// into `active_modal`. Pushes the "Enter issue URL:" prompt into
    /// scrollback so the user sees what to type. Separate from `draw`
    /// because `draw` takes `&self` and runs on every keystroke — we
    /// only want the prompt line once.
    pub fn emit_prompt(&mut self, renderer: &mut dyn Renderer) {
        if self.prompt_shown {
            return;
        }
        self.prompt_shown = true;
        push(renderer, "Enter AtomGit issue URL (Esc to cancel):");
        push(renderer, "  Example: https://atomgit.com/owner/repo/issues/42");
    }
}

fn push(renderer: &mut dyn Renderer, text: &str) {
    renderer.render(UiLine::CommandOutput(format!("  {}\n", text)));
    renderer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_starts_with_no_prompt_shown() {
        let w = IssueWizard::open();
        assert!(!w.prompt_shown);
    }

    #[test]
    fn emit_prompt_flips_flag_and_is_idempotent() {
        use crate::render::plain::PlainRenderer;
        let mut w = IssueWizard::open();
        let mut r = PlainRenderer::new();
        w.emit_prompt(&mut r);
        assert!(w.prompt_shown);
        // Second call is a no-op.
        w.emit_prompt(&mut r);
        assert!(w.prompt_shown);
    }
}
