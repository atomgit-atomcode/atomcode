// crates/atomcode-tuix/src/modals/mod.rs
//
// Modal overlay abstraction. Each of the three existing overlays
// (`/model` picker, `/provider` wizard, `/resume` session picker)
// implements `Modal`, and the event loop owns exactly one
// `active_modal: Option<Box<dyn Modal>>`. Adding a fourth modal means
// "new struct + new impl" — not another `Option<T>` field and another
// `handle_X_key` fn and another branch in `handle_input`.
//
// Modal impl lives next to the struct definition (for now still in
// `event_loop.rs`; Step 5 will move each modal to its own file under
// `crates/atomcode-tuix/src/modals/`). This module only defines the
// trait + action enum.

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use crate::event_loop::{Buffer, LoopCtx};
use crate::render::Renderer;
use crate::state::UiState;

pub mod dir_picker;
pub mod file_viewer;
pub mod issue_wizard;
pub mod language_picker;
pub mod model_picker;
pub mod onboarding_wizard;
pub mod plugin_manager;
pub mod password;
pub mod provider_wizard;
pub mod proxy_picker;
mod qr;
pub mod session_picker;
pub use dir_picker::DirPicker;
pub use file_viewer::FileViewer;
pub use issue_wizard::IssueWizard;
pub use language_picker::LanguagePicker;
pub use model_picker::ModelPicker;
pub use onboarding_wizard::OnboardingWizard;
pub use plugin_manager::PluginManager;
pub use provider_wizard::ProviderWizard;
pub use proxy_picker::ProxyPicker;
pub use session_picker::SessionPicker;

/// Result of a modal consuming one key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalAction {
    /// Modal stays active; keep dispatching keys to it next time.
    Continue,
    /// Modal finished (user hit Esc, or the submit side-effect is
    /// done). Caller must drop `active_modal` and redraw idle.
    Close,
}

/// A modal overlay: takes over key handling until it returns
/// `ModalAction::Close`. The implementation owns its own state (the
/// selected index, the wizard step, the filter query, etc.) and is
/// responsible for painting itself whenever its visible state changes
/// — the event loop only calls `draw` once at open time and once more
/// after `Close` to restore the idle prompt.
pub trait Modal: Send {
    /// Process one keystroke. Must either fully handle it (including
    /// any re-paint the modal wants) or report that the modal is now
    /// done so the caller can tear it down.
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction>;

    /// Paint the modal against the current terminal state. Called once
    /// when the modal is installed into `active_modal`; `handle_key`
    /// is expected to handle subsequent repaints after each key.
    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer);

    /// Handle a bracketed-paste payload while the modal is active.
    /// Default: append the text to `buf` (so text-input wizard steps
    /// naturally accept URL / API-key paste) and redraw. Modals that
    /// only present pickers (no text input) can leave the default —
    /// buf updates are harmless when the modal isn't displaying it.
    fn handle_paste(
        &mut self,
        text: &str,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        buf.insert_paste(text.to_string());
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    /// Whether this modal must receive ALL key/paste events regardless of the
    /// current `UiPhase`. The default Idle-only modal routing assumes the modal
    /// was opened from the prompt while idle; but the password modal installs
    /// mid-turn (phase == `Streaming`) when sudo/ssh asks for a password, so it
    /// must capture keys even while a tool runs — otherwise the typed password
    /// leaks into the type-ahead buffer and Esc cancels the turn instead of the
    /// modal. Default: `false` (preserve existing Idle-only behavior).
    fn captures_all_keys(&self) -> bool {
        false
    }

    /// Notify the modal that an async plugin job finished, so it can refresh
    /// any cached lists it is displaying. Default: ignore. Only the
    /// interactive `/plugin` manager overrides this. The event loop calls it
    /// before rendering the job result and before redrawing the modal.
    fn on_plugin_event(&mut self, _ev: &atomcode_core::plugin::PluginJobEvent) {}
}
