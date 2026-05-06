// crates/atomcode-tuix/src/modals/model_picker.rs
//
// `/model` modal — provider list picker.
//
// Holds the provider list sorted alphabetically with the current default
// first. Up/Down navigates, Enter selects (persists to config + notifies
// agent), Esc cancels. Renders as a MenuPayload above the input box.

use anyhow::Result;
use atomcode_core::config::Config;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, save_and_reload, Buffer, LoopCtx};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

pub struct ModelPicker {
    pub providers: Vec<String>,
    pub selected: usize,
}

impl ModelPicker {
    pub fn open(config: &Config) -> Self {
        let mut providers: Vec<String> = config.providers.keys().cloned().collect();
        providers.sort();
        // Put the current default at top for quick re-confirmation.
        let cur = config.default_provider.clone();
        if let Some(idx) = providers.iter().position(|p| *p == cur) {
            providers.swap(0, idx);
        }
        Self {
            providers,
            selected: 0,
        }
    }
}

impl Modal for ModelPicker {
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
                let max = self.providers.len().saturating_sub(1);
                if self.selected < max {
                    self.selected += 1;
                }
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Enter => {
                let chosen = self.providers[self.selected].clone();
                let display = ctx
                    .config
                    .providers
                    .get(&chosen)
                    .map(|p| p.model.clone())
                    .unwrap_or_else(|| chosen.clone());
                ctx.config.default_provider = chosen.clone();
                ctx.model_name = display.clone();
                // Persist to config.toml + notify agent. Without this,
                // the switch lives only in memory and the next startup
                // reverts to whatever was last saved.
                save_and_reload(ctx, renderer);
                // Clear any stale drift warning tied to the PREVIOUS
                // active provider — if the new provider is also CodingPlan,
                // the re-fire below will repopulate the slot with a
                // correct warning (or leave it clean).
                if let Ok(mut g) = ctx.monitor_warning.lock() {
                    *g = None;
                }
                // Same treatment for the usage slot: switching providers
                // invalidates the previous CodingPlan's quota snapshot.
                // Clearing here makes `build_usage_hint` short-circuit to
                // None until a fresh fetch lands; otherwise the user would
                // briefly see the OLD plan's percent on the new provider.
                if let Ok(mut g) = ctx.usage_slot.lock() {
                    *g = None;
                }
                // Re-fire the drift check if the new provider is also
                // CodingPlan-managed. Bypasses the 15-min cooldown on
                // purpose — explicit user action deserves a fresh read.
                if crate::event_loop::monitor::is_codingplan_provider(&chosen) {
                    ctx.monitor_last_check_at = Some(std::time::Instant::now());
                    crate::event_loop::monitor::spawn_check(
                        ctx.config.clone(),
                        ctx.model_name.clone(),
                        ctx.monitor_warning.clone(),
                        ctx.wake_tx.clone(),
                    );
                    // Mirror: re-fire usage check too. 30s cooldown is
                    // bypassed because the user just made an explicit
                    // switch — they want fresh data, not stale 30s data.
                    ctx.usage_last_check_at = Some(std::time::Instant::now());
                    crate::event_loop::usage_monitor::spawn_check(
                        ctx.usage_slot.clone(),
                        ctx.wake_tx.clone(),
                    );
                }
                renderer.render(UiLine::CommandOutput(format!(
                    "  Switched to {} · {}\n",
                    chosen, display
                )));
                renderer.flush();
                Ok(ModalAction::Close)
            }
            KeyCode::Esc => Ok(ModalAction::Close),
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let items: Vec<(String, String)> = self
            .providers
            .iter()
            .map(|name| {
                let desc = ctx
                    .config
                    .providers
                    .get(name)
                    .map(|c| format!("{} · {}", c.provider_type, c.model))
                    .unwrap_or_default();
                (name.clone(), desc)
            })
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
        });
        renderer.flush();
    }
}
