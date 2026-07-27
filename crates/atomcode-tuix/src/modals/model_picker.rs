// crates/atomcode-tuix/src/modals/model_picker.rs
//
// `/model` modal — provider list picker.
//
// Holds the provider list sorted alphabetically with the current default
// first. Up/Down navigates, Enter selects (persists to config + notifies
// agent), Esc cancels, printable chars + Backspace edit the filter query.
// Renders as a MenuPayload above the input box.

use anyhow::Result;
use atomcode_config::config::Config;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, set_default_provider_and_reload, Buffer, LoopCtx};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelCycleDirection {
    Next,
    Previous,
}

/// Map the global model-cycle shortcuts without stealing other modified
/// function keys from the host terminal or future bindings.
pub(crate) fn model_cycle_direction(
    code: KeyCode,
    modifiers: KeyModifiers,
) -> Option<ModelCycleDirection> {
    match (code, modifiers) {
        (KeyCode::F(2), KeyModifiers::NONE) => Some(ModelCycleDirection::Next),
        (KeyCode::F(2), KeyModifiers::SHIFT) => Some(ModelCycleDirection::Previous),
        _ => None,
    }
}

/// Pick the adjacent model profile in stable id order and wrap at both ends.
/// `/model` exposes the unified model catalog (legacy providers project to one
/// model each), so the shortcut follows that same source of truth.
pub(crate) fn adjacent_provider(config: &Config, direction: ModelCycleDirection) -> Option<String> {
    let mut ids: Vec<String> = config.logical_models().into_keys().collect();
    ids.sort_unstable();
    if ids.len() < 2 {
        return None;
    }

    let current = config.effective_model_selection().unwrap_or_default();
    let pos = ids
        .iter()
        .position(|id| *id == current)
        .unwrap_or(match direction {
            ModelCycleDirection::Next => ids.len() - 1,
            ModelCycleDirection::Previous => 0,
        });
    let target = match direction {
        ModelCycleDirection::Next => (pos + 1) % ids.len(),
        ModelCycleDirection::Previous => (pos + ids.len() - 1) % ids.len(),
    };
    Some(ids[target].clone())
}

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
        let mut providers: Vec<String> = config.logical_models().into_keys().collect();
        providers.sort();
        // Put the current selection at top for quick re-confirmation.
        let cur = config.effective_model_selection().unwrap_or_default();
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

    /// Recompute `filtered` from `query`, matching against the selection id,
    /// account, wire model name, and display name (all case-insensitive).
    pub fn update_filter(&mut self, config: &Config) {
        let q = self.query.to_lowercase();
        let models = config.logical_models();
        let accounts = config.logical_accounts();
        self.filtered = self
            .providers
            .iter()
            .enumerate()
            .filter(|(_, id)| {
                if q.is_empty() {
                    return true;
                }
                if id.to_lowercase().contains(&q) {
                    return true;
                }
                if let Some(m) = models.get(*id) {
                    if m.model.to_lowercase().contains(&q) {
                        return true;
                    }
                    if m.account.to_lowercase().contains(&q) {
                        return true;
                    }
                    if m
                        .display_name
                        .as_deref()
                        .is_some_and(|d| d.to_lowercase().contains(&q))
                    {
                        return true;
                    }
                    // Vendor / protocol (the account's preset id).
                    if let Some(a) = accounts.get(&m.account) {
                        if a.provider.to_lowercase().contains(&q) {
                            return true;
                        }
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
                if set_default_provider_and_reload(ctx, &chosen, renderer) {
                    Ok(ModalAction::Close)
                } else {
                    self.draw(buf, state, ctx, renderer);
                    Ok(ModalAction::Continue)
                }
            }
            KeyCode::Esc => Ok(ModalAction::Close),
            _ => Ok(ModalAction::Continue),
        }
    }

    fn handle_paste(
        &mut self,
        text: &str,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // Paste goes into the query, not the main buffer
        for c in text.chars() {
            if c.is_control() {
                continue; // skip newlines/control characters
            }
            self.query.push(c);
        }
        self.update_filter(&ctx.config);
        self.selected = 0;
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, _buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let payload = build_menu_payload(self, ctx);
        // Show the typed filter query as the editable input line (not the main
        // buffer, which stays untouched while the modal is open). Typing routes
        // into `self.query`; rendering `buf.text` here would leave the input box
        // blank even though filtering works. Mirrors `dir_picker`.
        renderer.render(UiLine::InputPrompt {
            buf: self.query.clone(),
            cursor_byte: self.query.len(),
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
            "(no models configured — use /provider add)".to_string()
        } else if p.query.is_empty() {
            "(no models match)".to_string()
        } else {
            format!("(no models match \"{}\" — Backspace to clear)", p.query)
        };
        return MenuPayload {
            items: vec![(label, String::new())],
            selected: 0,
            kind: crate::render::MenuKind::TwoColumn {
                row_prefix: "",
                selected_marker: "▸",
            },
        };
    }
    let models = ctx.config.logical_models();
    let items: Vec<(String, String)> = p
        .filtered
        .iter()
        .map(|&idx| {
            let id = &p.providers[idx];
            let desc = models
                .get(id)
                .map(|m| {
                    let name = m.display_name.as_deref().unwrap_or(&m.model);
                    format!("{} · {}", m.account, name)
                })
                .unwrap_or_default();
            (id.clone(), desc)
        })
        .collect();
    MenuPayload {
        items,
        selected: p.selected,
        kind: crate::render::MenuKind::TwoColumn {
            row_prefix: "",
            selected_marker: "▸",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_config::config::provider::ProviderConfig;
    use atomcode_config::config::Config;
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
                    capable_model: None,
                    pricing: None,
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
            vec![
                ("alpha", "openai", "gpt-4"),
                ("beta", "anthropic", "claude-3"),
            ],
            "alpha",
        );
        let p = ModelPicker::open(&config);
        assert_eq!(p.filtered.len(), 2);
        assert_eq!(p.selected, 0);
        // Default provider should be first
        assert_eq!(p.providers[0], "alpha");
    }

    #[test]
    fn open_lists_new_schema_model_profiles() {
        // One account, two model profiles — the picker lists them by selection
        // id, current default first, and filters by wire model name.
        let config: Config = serde_json::from_value(serde_json::json!({
            "default_model": "acc/coder",
            "provider_accounts": { "acc": { "provider": "deepseek" } },
            "models": {
                "acc/coder": { "account": "acc", "model": "deepseek-coder", "context_window": 131072 },
                "acc/chat":  { "account": "acc", "model": "deepseek-chat",   "context_window": 131072 }
            }
        }))
        .unwrap();
        let p = ModelPicker::open(&config);
        assert_eq!(p.filtered.len(), 2);
        assert_eq!(p.providers[0], "acc/coder"); // default selection first
        assert!(p.providers.contains(&"acc/chat".to_string()));

        let mut p2 = ModelPicker::open(&config);
        p2.query = "chat".into();
        p2.update_filter(&config);
        assert_eq!(p2.filtered.len(), 1);
        assert_eq!(p2.providers[p2.filtered[0]], "acc/chat");
    }

    #[test]
    fn update_filter_matches_by_name_case_insensitive() {
        let config = make_config(
            vec![
                ("Alpha", "openai", "gpt-4"),
                ("Beta", "anthropic", "claude-3"),
            ],
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
            vec![
                ("alpha", "openai", "gpt-4"),
                ("beta", "anthropic", "claude-3"),
            ],
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
            vec![
                ("alpha", "openai", "gpt-4"),
                ("beta", "anthropic", "claude-3"),
            ],
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
            vec![
                ("alpha", "openai", "gpt-4"),
                ("beta", "anthropic", "claude-3"),
            ],
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
            vec![
                ("alpha", "openai", "gpt-4"),
                ("beta", "anthropic", "claude-3"),
            ],
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
            vec![
                ("a", "openai", "gpt-4"),
                ("b", "anthropic", "claude-3"),
                ("c", "openai", "gpt-3.5"),
            ],
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
            vec![
                ("alpha", "openai", "gpt-4"),
                ("beta", "anthropic", "claude-3"),
            ],
            "alpha",
        );
        let p = ModelPicker::open(&config);
        assert_eq!(p.chosen_provider(), Some("alpha"));
    }

    #[test]
    fn f2_shortcuts_map_to_cycle_directions() {
        assert_eq!(
            model_cycle_direction(KeyCode::F(2), KeyModifiers::NONE),
            Some(ModelCycleDirection::Next)
        );
        assert_eq!(
            model_cycle_direction(KeyCode::F(2), KeyModifiers::SHIFT),
            Some(ModelCycleDirection::Previous)
        );
        assert_eq!(
            model_cycle_direction(KeyCode::F(2), KeyModifiers::CONTROL),
            None
        );
        assert_eq!(
            model_cycle_direction(KeyCode::F(3), KeyModifiers::NONE),
            None
        );
    }

    #[test]
    fn adjacent_provider_cycles_in_stable_sorted_order() {
        let config = make_config(
            vec![
                ("charlie", "openai", "gpt-4"),
                ("alpha", "anthropic", "claude-3"),
                ("bravo", "ollama", "qwen"),
            ],
            "bravo",
        );

        assert_eq!(
            adjacent_provider(&config, ModelCycleDirection::Next).as_deref(),
            Some("charlie")
        );
        assert_eq!(
            adjacent_provider(&config, ModelCycleDirection::Previous).as_deref(),
            Some("alpha")
        );
    }

    #[test]
    fn adjacent_provider_wraps_and_ignores_single_provider() {
        let first = make_config(
            vec![
                ("charlie", "openai", "gpt-4"),
                ("alpha", "anthropic", "claude-3"),
            ],
            "alpha",
        );
        assert_eq!(
            adjacent_provider(&first, ModelCycleDirection::Previous).as_deref(),
            Some("charlie")
        );

        let last = make_config(
            vec![
                ("charlie", "openai", "gpt-4"),
                ("alpha", "anthropic", "claude-3"),
            ],
            "charlie",
        );
        assert_eq!(
            adjacent_provider(&last, ModelCycleDirection::Next).as_deref(),
            Some("alpha")
        );

        let only = make_config(vec![("alpha", "openai", "gpt-4")], "alpha");
        assert_eq!(adjacent_provider(&only, ModelCycleDirection::Next), None);
    }

    #[test]
    fn chosen_returns_none_when_filter_empty() {
        let config = make_config(vec![("alpha", "openai", "gpt-4")], "alpha");
        let mut p = ModelPicker::open(&config);
        p.query = "zzz".to_string();
        p.update_filter(&config);
        assert_eq!(p.chosen_provider(), None);
    }
}
