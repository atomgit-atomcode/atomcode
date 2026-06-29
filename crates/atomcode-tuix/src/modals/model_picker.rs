// crates/atomcode-tuix/src/modals/model_picker.rs
//
// `/model` modal — provider list picker.
//
// Holds the provider list sorted alphabetically with the current default
// first. Up/Down navigates, Enter selects (persists to config + notifies
// agent), Esc cancels, printable chars + Backspace edit the filter query.
// Renders as a MenuPayload above the input box.

use anyhow::Result;
use atomcode_core::config::Config;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, save_and_reload, Buffer, LoopCtx};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

pub struct ModelPicker {
    /// All provider names, sorted alphabetically with the current default first.
    pub providers: Vec<String>,
    /// User-typed filter text. Empty string = show all.
    pub query: String,
    /// Indices into `providers` that match `query` (case-insensitive substring
    /// on provider name, provider_type, and model).
    pub filtered: Vec<usize>,
    /// Index into `filtered`.
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
        let filtered: Vec<usize> = (0..providers.len()).collect();
        Self {
            providers,
            query: String::new(),
            filtered,
            selected: 0,
        }
    }

    /// Recompute `filtered` from `query`, matching against provider name,
    /// provider_type, and model (all case-insensitive substring).
    pub fn update_filter(&mut self, config: &Config) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, name)| {
                if q.is_empty() {
                    return true;
                }
                // Match provider name
                if name.to_lowercase().contains(&q) {
                    return true;
                }
                // Match provider_type and model from config
                if let Some(c) = config.providers.get(*name) {
                    if c.provider_type.to_lowercase().contains(&q) {
                        return true;
                    }
                    if c.model.to_lowercase().contains(&q) {
                        return true;
                    }
                }
                false
            })
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
    }

    pub fn up(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.filtered.len().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
        }
    }

    /// Return the provider name at the current filtered selection.
    pub fn chosen_provider(&self) -> Option<&str> {
        let i = *self.filtered.get(self.selected)?;
        self.providers.get(i).map(|s| s.as_str())
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
                self.up();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Down => {
                self.down();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.update_filter(&ctx.config);
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Char(c) if !_mods.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.update_filter(&ctx.config);
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Enter => {
                let chosen = match self.chosen_provider() {
                    Some(p) => p.to_string(),
                    None => return Ok(ModalAction::Close),
                };
                let display = ctx
                    .config
                    .providers
                    .get(&chosen)
                    .map(|p| p.model.clone())
                    .unwrap_or_else(|| chosen.clone());
                ctx.config.default_provider = chosen.clone();
                ctx.model_name = display.clone();
                // Refresh the footer's context window NOW. The `used/window`
                // pair comes from the cached ContextStats snapshot, which is
                // only updated during a turn — switching models fires none, so
                // without this the denominator stays pinned to the previous
                // model's window (e.g. 10.1k/200k after switching to a 128k
                // model). `default_provider` was just set above, so the lookup
                // returns the new model's window.
                state.on_model_window_changed(ctx.config.default_context_window());
                // Persist to config.toml + notify agent. Without this,
                // the switch lives only in memory and the next startup
                // reverts to whatever was last saved.
                save_and_reload(ctx, renderer);
                // Live-sync: broadcast this switch so an attached webui's model
                // dropdown follows. No-op when no live session exists (it just
                // updates the process-level selection, which is harmless).
                atomcode_daemon::live_set_provider(chosen.clone());
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
                renderer.render(UiLine::CommandOutput(
                    crate::i18n::t(crate::i18n::Msg::ModelSwitched { provider: &chosen, model: &display }).into_owned(),
                ));
                renderer.flush();
                Ok(ModalAction::Close)
            }
            KeyCode::Esc => Ok(ModalAction::Close),
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let payload = build_menu_payload(self, ctx);
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

fn build_menu_payload(p: &ModelPicker, ctx: &LoopCtx) -> MenuPayload {
    // Empty state: surface a hint row so the user can tell the filter is
    // active and which query is excluding everything (otherwise the menu
    // renders as blank space and looks like the modal hung).
    if p.filtered.is_empty() {
        let label = if p.providers.is_empty() {
            "(no providers configured — use /provider add)".to_string()
        } else if p.query.is_empty() {
            "(no providers match)".to_string()
        } else {
            format!(
                "(no providers match \"{}\" — Backspace to clear)",
                p.query
            )
        };
        return MenuPayload {
            items: vec![(label, String::new())],
            selected: 0,
            kind: crate::render::MenuKind::TwoColumn { row_prefix: "", selected_marker: "▸" },
        };
    }
    let items: Vec<(String, String)> = p
        .filtered
        .iter()
        .map(|&idx| {
            let name = &p.providers[idx];
            let desc = ctx
                .config
                .providers
                .get(name)
                .map(|c| format!("{} · {}", c.provider_type, c.model))
                .unwrap_or_default();
            (name.clone(), desc)
        })
        .collect();
    MenuPayload {
        items,
        selected: p.selected,
        kind: crate::render::MenuKind::TwoColumn { row_prefix: "", selected_marker: "▸" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::config::provider::ProviderConfig;
    use atomcode_core::config::Config;
    fn make_config(providers: Vec<(&str, &str, &str)>, default: &str) -> Config {
        use std::collections::HashMap;
        let mut map: HashMap<String, ProviderConfig> = HashMap::new();
        for (name, ptype, model) in providers {
            map.insert(
                name.to_string(),
                ProviderConfig {
                    provider_type: ptype.to_string(),
                    api_key: None,
                    model: model.to_string(),
                    base_url: None,
                    system_prompt: None,
                    user_agent: None,
                    context_window: 128000,
                    max_tokens: None,
                    thinking_type: None,
                    thinking_keep: None,
                    reasoning_history: None,
                    reasoning_effort: None,
                    thinking_enabled: None,
                    thinking_budget: None,
                    skip_tls_verify: false,
                    ephemeral: false,
                },
            );
        }
        Config {
            providers: map,
            ..Config::with_default_provider(default)
        }
    }

    #[test]
    fn open_shows_all_providers_initially() {
        let config = make_config(
            vec![("alpha", "openai", "gpt-4"), ("beta", "anthropic", "claude-3")],
            "alpha",
        );
        let p = ModelPicker::open(&config);
        assert_eq!(p.filtered.len(), 2);
        assert_eq!(p.selected, 0);
        // Default provider should be first
        assert_eq!(p.providers[0], "alpha");
    }

    #[test]
    fn update_filter_matches_by_name_case_insensitive() {
        let config = make_config(
            vec![("Alpha", "openai", "gpt-4"), ("Beta", "anthropic", "claude-3")],
            "Alpha",
        );
        let mut p = ModelPicker::open(&config);
        p.query = "alpha".to_string();
        p.update_filter(&config);
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.providers[p.filtered[0]], "Alpha");
    }

    #[test]
    fn update_filter_matches_by_provider_type() {
        let config = make_config(
            vec![("alpha", "openai", "gpt-4"), ("beta", "anthropic", "claude-3")],
            "alpha",
        );
        let mut p = ModelPicker::open(&config);
        p.query = "anthropic".to_string();
        p.update_filter(&config);
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.providers[p.filtered[0]], "beta");
    }

    #[test]
    fn update_filter_matches_by_model() {
        let config = make_config(
            vec![("alpha", "openai", "gpt-4"), ("beta", "anthropic", "claude-3")],
            "alpha",
        );
        let mut p = ModelPicker::open(&config);
        p.query = "gpt".to_string();
        p.update_filter(&config);
        assert_eq!(p.filtered.len(), 1);
        assert_eq!(p.providers[p.filtered[0]], "alpha");
    }

    #[test]
    fn update_filter_empty_query_shows_all() {
        let config = make_config(
            vec![("alpha", "openai", "gpt-4"), ("beta", "anthropic", "claude-3")],
            "alpha",
        );
        let mut p = ModelPicker::open(&config);
        p.query = "zzz".to_string();
        p.update_filter(&config);
        assert_eq!(p.filtered.len(), 0);
        p.query.clear();
        p.update_filter(&config);
        assert_eq!(p.filtered.len(), 2);
    }

    #[test]
    fn update_filter_resets_selection_to_zero() {
        let config = make_config(
            vec![("alpha", "openai", "gpt-4"), ("beta", "anthropic", "claude-3")],
            "alpha",
        );
        let mut p = ModelPicker::open(&config);
        p.selected = 1;
        p.query = "gpt".to_string();
        p.update_filter(&config);
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn down_and_up_stay_within_filtered_bounds() {
        let config = make_config(
            vec![("a", "openai", "gpt-4"), ("b", "anthropic", "claude-3"), ("c", "openai", "gpt-3.5")],
            "a",
        );
        let mut p = ModelPicker::open(&config);
        // Filter to openai providers (2 matches)
        p.query = "openai".to_string();
        p.update_filter(&config);
        assert_eq!(p.filtered.len(), 2);
        p.down();
        assert_eq!(p.selected, 1);
        p.down(); // should clamp
        assert_eq!(p.selected, 1);
        p.up();
        assert_eq!(p.selected, 0);
        p.up(); // should clamp
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn chosen_returns_provider_at_selected() {
        let config = make_config(
            vec![("alpha", "openai", "gpt-4"), ("beta", "anthropic", "claude-3")],
            "alpha",
        );
        let p = ModelPicker::open(&config);
        assert_eq!(p.chosen_provider(), Some("alpha"));
    }

    #[test]
    fn chosen_returns_none_when_filter_empty() {
        let config = make_config(
            vec![("alpha", "openai", "gpt-4")],
            "alpha",
        );
        let mut p = ModelPicker::open(&config);
        p.query = "zzz".to_string();
        p.update_filter(&config);
        assert_eq!(p.chosen_provider(), None);
    }
}
