// crates/atomcode-tuix/src/modals/welcome_wizard.rs
//
// First-run onboarding modal. 3 options:
//
//   ▸ Set up CodingPlan      Free tokens · recommended
//     Configure manually     API key
//     Skip for now           explore first
//
// Option 0 was "Login with AtomGit" up to v4.19 — it triggered a pure OAuth
// flow and left the user stuck at "signed in but no provider" until they
// found `/codingplan` themselves. v4.20+ routes option 0 straight into the
// CodingPlan setup flow, which does login-if-needed → claim free plan →
// register plan-eligible providers in one shot. Users who want identity
// without the plan can still use `/login` after skipping.
//
// Opens at startup when `providers.is_empty() && no stored OAuth auth` —
// users with a config or prior OAuth auth are never shown this. The Welcome
// banner itself is still pushed to scrollback by the event-loop's startup
// path; this modal adds a "Get started:" guide line pair into body, plus
// the 3-choice menu rendered through the standard `MenuPayload` footer
// (same mechanism as the slash-command palette).
//
// Side-channel: the modal cannot directly run the CodingPlan flow or swap
// itself for `ProviderWizard::MainMenu` (both need `active_modal` mutation
// and suspend/resume of raw mode, neither of which `LoopCtx` exposes). So
// it sets one of two flags on `LoopCtx` and returns `Close`; the event
// loop's post-close branch reads the flag and drives the follow-up action.
// This matches the pattern already used by `IssueWizard` → `pending_new_issue`.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, Buffer, LoopCtx};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

/// Three-choice first-run onboarding menu.
pub struct WelcomeWizard {
    /// Currently highlighted option index (0..=2).
    pub selected: usize,
}

impl WelcomeWizard {
    pub fn new() -> Self {
        Self { selected: 0 }
    }

    /// Stable option list.
    fn options() -> [(String, String); 3] {
        use crate::i18n::{t, Msg};
        [
            (t(Msg::WelcomeOptionCodingPlan).into_owned(),
             t(Msg::WelcomeOptionCodingPlanHint).into_owned()),
            (t(Msg::WelcomeOptionConfigureManually).into_owned(),
             t(Msg::WelcomeOptionConfigureManuallyHint).into_owned()),
            (t(Msg::WelcomeOptionSkip).into_owned(),
             t(Msg::WelcomeOptionSkipHint).into_owned()),
        ]
    }

    fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn down(&mut self) {
        if self.selected < 2 {
            self.selected += 1;
        }
    }
}

impl Default for WelcomeWizard {
    fn default() -> Self {
        Self::new()
    }
}

impl Modal for WelcomeWizard {
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
                self.up();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Down => {
                self.down();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Enter => {
                match self.selected {
                    0 => ctx.pending_run_codingplan = true,
                    1 => ctx.pending_open_provider_wizard = true,
                    // 2 = Skip — no side effect; event loop's Close branch
                    // will redraw idle with the "no provider" status hint
                    // (see `build_status`) so the user still sees what to do.
                    _ => {}
                }
                Ok(ModalAction::Close)
            }
            KeyCode::Esc => {
                // Esc behaves like Skip — no side effect, just dismiss.
                Ok(ModalAction::Close)
            }
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let payload = build_menu_payload(self);
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

fn build_menu_payload(w: &WelcomeWizard) -> MenuPayload {
    let items: Vec<(String, String)> = WelcomeWizard::options()
        .into_iter()
        .collect();
    MenuPayload {
        items,
        selected: w.selected,
            kind: crate::render::MenuKind::SlashCommand,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_starts_on_codingplan() {
        let w = WelcomeWizard::new();
        assert_eq!(w.selected, 0);
    }

    /// Option 0 is CodingPlan setup — the recommended first-run
    /// action (login + claim + register plan providers in one shot).
    /// Option 1 falls back to manual provider config; option 2 skips
    /// setup entirely. Label text is also pinned here so a rename
    /// has to go through a test update.
    #[test]
    fn options_put_codingplan_first() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let opts = WelcomeWizard::options();
        assert_eq!(opts[0].0, "Set up CodingPlan");
        assert_eq!(opts[0].1, "Free tokens · recommended");
        assert_eq!(opts[1].0, "Configure manually");
        assert_eq!(opts[2].0, "Skip for now");
    }

    #[test]
    fn down_advances_but_stops_at_last() {
        let mut w = WelcomeWizard::new();
        w.down();
        assert_eq!(w.selected, 1);
        w.down();
        assert_eq!(w.selected, 2);
        // Already at last row — further Down must not overflow past the
        // 3-option range (there's no wrap; guarding the 0..=2 invariant).
        w.down();
        assert_eq!(w.selected, 2);
    }

    #[test]
    fn up_retreats_but_stops_at_first() {
        let mut w = WelcomeWizard::new();
        w.selected = 2;
        w.up();
        assert_eq!(w.selected, 1);
        w.up();
        assert_eq!(w.selected, 0);
        // Underflow guard — saturating_sub holds us at 0.
        w.up();
        assert_eq!(w.selected, 0);
    }

    #[test]
    fn menu_payload_reflects_selection() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let mut w = WelcomeWizard::new();
        w.selected = 1;
        let p = build_menu_payload(&w);
        assert_eq!(p.selected, 1);
        assert_eq!(p.items.len(), 3);
        assert_eq!(p.items[0].0, "Set up CodingPlan");
        assert_eq!(p.items[0].1, "Free tokens · recommended");
    }
}
