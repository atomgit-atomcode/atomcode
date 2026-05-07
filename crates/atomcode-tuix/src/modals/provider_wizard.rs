// crates/atomcode-tuix/src/modals/provider_wizard.rs
//
// `/provider` modal — multi-step Q&A wizard for provider management.
//
// Runs entirely in scrollback (no alt-screen): each step pushes a prompt
// line ("Provider name?"), the user types + Enter, the answer is echoed
// back and the next step's prompt appears. Persistent menus (MainMenu,
// EditPick, DeletePick, SetDefaultPick) reuse the `MenuPayload` footer
// palette. Esc cancels at any point.

use anyhow::Result;
use atomcode_core::config::provider::ProviderConfig;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, save_and_reload, Buffer, LoopCtx};
use crate::input::key_action::classify;
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

pub enum ProviderWizard {
    /// Initial picker: Add / Edit / Delete / Set Default.
    MainMenu { selected: usize },
    /// Sequential `Add` prompts. `draft` accumulates answered fields.
    Add {
        step: WizardStep,
        draft: DraftProvider,
    },
    /// Pick which provider to edit.
    EditPick {
        providers: Vec<String>,
        selected: usize,
    },
    /// Editing a specific provider; same flow as `Add` but prompts show
    /// the existing value as a hint and an empty Enter keeps it.
    Edit {
        target: String,
        step: WizardStep,
        draft: DraftProvider,
    },
    /// Pick which provider to delete.
    DeletePick {
        providers: Vec<String>,
        selected: usize,
    },
    /// Final y/N confirmation before a delete actually lands.
    DeleteConfirm { target: String },
    /// Pick which provider to make default.
    SetDefaultPick {
        providers: Vec<String>,
        selected: usize,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum WizardStep {
    Name,
    ProviderType,
    BaseUrl,
    ApiKey,
    Model,
}

#[derive(Clone, Debug, Default)]
pub struct DraftProvider {
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl DraftProvider {
    /// Merge this draft onto `base` — empty fields leave `base` untouched.
    /// Used by Edit so an empty Enter at a prompt keeps the existing value.
    fn apply_onto(&self, base: &mut ProviderConfig) {
        if !self.provider_type.is_empty() {
            base.provider_type = self.provider_type.clone();
        }
        if !self.base_url.is_empty() {
            base.base_url = Some(self.base_url.clone());
        }
        if !self.api_key.is_empty() {
            base.api_key = Some(self.api_key.clone());
        }
        if !self.model.is_empty() {
            base.model = self.model.clone();
        }
    }

    fn into_config(self) -> ProviderConfig {
        use atomcode_core::config::provider::default_context_window_for;
        let provider_type = self.provider_type.clone();
        ProviderConfig {
            provider_type: provider_type.clone(),
            api_key: if self.api_key.is_empty() {
                None
            } else {
                Some(self.api_key)
            },
            model: self.model,
            base_url: if self.base_url.is_empty() {
                None
            } else {
                Some(self.base_url)
            },
            system_prompt: None,
            user_agent: None,
            context_window: default_context_window_for(&provider_type),
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: false,

}
    }
}

impl Modal for ProviderWizard {
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        handle_key(code, mods, buf, state, ctx, renderer, self)
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        redraw(buf, state, ctx, self, renderer);
    }
}

/// Process one key for the wizard. Returns `Continue` if the wizard
/// stays active, `Close` when it's done (cancelled, committed, or
/// transitioned to Idle after a terminal operation).
fn handle_key(
    code: KeyCode,
    _mods: KeyModifiers,
    buf: &mut Buffer,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    wizard: &mut ProviderWizard,
) -> Result<ModalAction> {
    // Esc always cancels at any point.
    if matches!(code, KeyCode::Esc) {
        buf.text.clear();
        buf.cursor = 0;
        push(renderer, "(cancelled)");
        return Ok(ModalAction::Close);
    }

    // Take the current state out so we can move fields; put it back
    // (or replace it) before returning Continue.
    let current = std::mem::replace(wizard, ProviderWizard::MainMenu { selected: 0 });
    match current {
        // ── Menu states: Up / Down / Enter navigate; others ignored. ──
        ProviderWizard::MainMenu { mut selected } => {
            const ITEMS: [&str; 4] = ["add", "edit", "delete", "set-default"];
            match code {
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    *wizard = ProviderWizard::MainMenu { selected };
                }
                KeyCode::Down => {
                    if selected + 1 < ITEMS.len() {
                        selected += 1;
                    }
                    *wizard = ProviderWizard::MainMenu { selected };
                }
                KeyCode::Enter => {
                    let providers: Vec<String> = {
                        let mut v: Vec<String> = ctx.config.providers.keys().cloned().collect();
                        v.sort();
                        v
                    };
                    match ITEMS[selected] {
                        "add" => {
                            let new = ProviderWizard::Add {
                                step: WizardStep::Name,
                                draft: DraftProvider::default(),
                            };
                            show_step_prompt(
                                WizardStep::Name,
                                None,
                                buf,
                                state,
                                ctx,
                                &new,
                                renderer,
                            );
                            *wizard = new;
                        }
                        "edit" | "delete" | "set-default" if providers.is_empty() => {
                            push(renderer, "No providers configured yet.");
                            return Ok(ModalAction::Close);
                        }
                        "edit" => {
                            let new = ProviderWizard::EditPick {
                                providers,
                                selected: 0,
                            };
                            redraw(buf, state, ctx, &new, renderer);
                            *wizard = new;
                        }
                        "delete" => {
                            let new = ProviderWizard::DeletePick {
                                providers,
                                selected: 0,
                            };
                            redraw(buf, state, ctx, &new, renderer);
                            *wizard = new;
                        }
                        "set-default" => {
                            let new = ProviderWizard::SetDefaultPick {
                                providers,
                                selected: 0,
                            };
                            redraw(buf, state, ctx, &new, renderer);
                            *wizard = new;
                        }
                        _ => {
                            *wizard = ProviderWizard::MainMenu { selected };
                        }
                    }
                }
                _ => {
                    *wizard = ProviderWizard::MainMenu { selected };
                }
            }
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }

        // ── Picker states share Up/Down/Enter logic. ──
        ProviderWizard::EditPick {
            providers,
            mut selected,
        } => {
            match code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < providers.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let target = providers[selected].clone();
                    let existing = ctx.config.providers.get(&target).cloned();
                    let new = ProviderWizard::Edit {
                        target: target.clone(),
                        step: WizardStep::ProviderType, // skip Name (immutable)
                        draft: DraftProvider::default(),
                    };
                    show_step_prompt(
                        WizardStep::ProviderType,
                        existing.as_ref(),
                        buf,
                        state,
                        ctx,
                        &new,
                        renderer,
                    );
                    *wizard = new;
                    return Ok(ModalAction::Continue);
                }
                _ => {}
            }
            *wizard = ProviderWizard::EditPick {
                providers,
                selected,
            };
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }

        ProviderWizard::DeletePick {
            providers,
            mut selected,
        } => {
            match code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < providers.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let target = providers[selected].clone();
                    push(renderer, &format!("Delete \"{}\"? [y/N]", target));
                    *wizard = ProviderWizard::DeleteConfirm { target };
                    redraw(buf, state, ctx, wizard, renderer);
                    return Ok(ModalAction::Continue);
                }
                _ => {}
            }
            *wizard = ProviderWizard::DeletePick {
                providers,
                selected,
            };
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }

        ProviderWizard::SetDefaultPick {
            providers,
            mut selected,
        } => {
            match code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < providers.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let chosen = providers[selected].clone();
                    ctx.config.default_provider = chosen.clone();
                    if let Some(p) = ctx.config.providers.get(&chosen) {
                        ctx.model_name = p.model.clone();
                    }
                    save_and_reload(ctx, renderer);
                    push(renderer, &format!("Default set to {}.", chosen));
                    return Ok(ModalAction::Close);
                }
                _ => {}
            }
            *wizard = ProviderWizard::SetDefaultPick {
                providers,
                selected,
            };
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }

        ProviderWizard::DeleteConfirm { target } => {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    ctx.config.providers.remove(&target);
                    // If we just dropped the default, fall back to any
                    // remaining provider or blank.
                    if ctx.config.default_provider == target {
                        ctx.config.default_provider = ctx
                            .config
                            .providers
                            .keys()
                            .next()
                            .cloned()
                            .unwrap_or_default();
                    }
                    save_and_reload(ctx, renderer);
                    push(renderer, &format!("Removed \"{}\".", target));
                }
                _ => {
                    push(renderer, "(kept)");
                }
            }
            Ok(ModalAction::Close)
        }

        // ── Text-input states: Enter submits, chars edit buf, others pass through Buffer. ──
        ProviderWizard::Add { step, mut draft } => {
            if matches!(code, KeyCode::Enter) {
                let answer = buf.text.clone();
                push(renderer, &format!("  ↳ {}", answer));
                buf.text.clear();
                buf.cursor = 0;
                match advance_add(&mut draft, step, &answer, renderer) {
                    Some(next) => {
                        let new = ProviderWizard::Add { step: next, draft };
                        show_step_prompt(next, None, buf, state, ctx, &new, renderer);
                        *wizard = new;
                        return Ok(ModalAction::Continue);
                    }
                    None => {
                        // All fields gathered — commit and switch to it.
                        // Users expect /provider add to behave like "create
                        // and activate": after the wizard closes, the newly
                        // added entry should be the current default so the
                        // next message uses it without an extra /model step.
                        let name = draft.name.clone();
                        let model = draft.model.clone();
                        let cfg = draft.into_config();
                        ctx.config.providers.insert(name.clone(), cfg);
                        ctx.config.default_provider = name.clone();
                        ctx.model_name = model.clone();
                        save_and_reload(ctx, renderer);
                        push(
                            renderer,
                            &format!(
                                "Added provider \"{}\" and switched to {} · {}.",
                                name, name, model
                            ),
                        );
                        return Ok(ModalAction::Close);
                    }
                }
            }
            // Forward other keys to the buffer so typing / editing works.
            forward_to_buffer(code, _mods, buf, ctx);
            *wizard = ProviderWizard::Add { step, draft };
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }

        ProviderWizard::Edit {
            target,
            step,
            mut draft,
        } => {
            if matches!(code, KeyCode::Enter) {
                let answer = buf.text.clone();
                push(
                    renderer,
                    &format!(
                        "  ↳ {}",
                        if answer.is_empty() {
                            "(keep)"
                        } else {
                            answer.as_str()
                        }
                    ),
                );
                buf.text.clear();
                buf.cursor = 0;
                match advance_edit(&mut draft, step, &answer, renderer) {
                    Some(next) => {
                        let existing = ctx.config.providers.get(&target).cloned();
                        let new = ProviderWizard::Edit {
                            target: target.clone(),
                            step: next,
                            draft,
                        };
                        show_step_prompt(next, existing.as_ref(), buf, state, ctx, &new, renderer);
                        *wizard = new;
                        return Ok(ModalAction::Continue);
                    }
                    None => {
                        // Commit edit: merge draft onto existing provider.
                        if let Some(existing) = ctx.config.providers.get_mut(&target) {
                            draft.apply_onto(existing);
                        }
                        save_and_reload(ctx, renderer);
                        push(renderer, &format!("Updated \"{}\".", target));
                        return Ok(ModalAction::Close);
                    }
                }
            }
            forward_to_buffer(code, _mods, buf, ctx);
            *wizard = ProviderWizard::Edit {
                target,
                step,
                draft,
            };
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }
    }
}

/// Redraw the footer with the wizard's current menu/prompt. Text-input
/// steps show the normal input box; picker steps show an overlay menu
/// built from wizard state.
fn redraw(
    buf: &Buffer,
    state: &UiState,
    ctx: &LoopCtx,
    wizard: &ProviderWizard,
    renderer: &mut dyn Renderer,
) {
    let menu = match wizard {
        ProviderWizard::MainMenu { selected } => Some(MenuPayload {
            items: vec![
                ("add".into(), "Add a new provider".into()),
                ("edit".into(), "Edit an existing provider".into()),
                ("delete".into(), "Remove a provider".into()),
                ("set-default".into(), "Switch the default provider".into()),
            ],
            selected: *selected,
            kind: crate::render::MenuKind::SlashCommand,
        }),
        ProviderWizard::EditPick {
            providers,
            selected,
        }
        | ProviderWizard::DeletePick {
            providers,
            selected,
        }
        | ProviderWizard::SetDefaultPick {
            providers,
            selected,
        } => {
            let items: Vec<(String, String)> = providers
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
            Some(MenuPayload {
                items,
                selected: *selected,
            kind: crate::render::MenuKind::SlashCommand,
            })
        }
        // Q&A steps: plain input box, no overlay menu.
        ProviderWizard::Add { .. }
        | ProviderWizard::Edit { .. }
        | ProviderWizard::DeleteConfirm { .. } => None,
    };
    renderer.render(UiLine::InputPrompt {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        menu,
        status: build_status(state, ctx),
    });
    renderer.flush();
}

/// Push a prompt line into scrollback. Steps share the same "tool-line"
/// styling — a muted line with two-space indent — so the Q&A reads like
/// the rest of the conversation rather than a modal popup.
fn push(renderer: &mut dyn Renderer, text: &str) {
    renderer.render(UiLine::CommandOutput(format!("  {}\n", text)));
    renderer.flush();
}

/// Prompt string for the given wizard step; includes the existing value
/// as a hint in Edit mode so the user sees what empty-Enter will keep.
fn step_prompt_text(step: WizardStep, existing: Option<&ProviderConfig>) -> String {
    match (step, existing) {
        (WizardStep::Name, _) => "Provider name?".into(),
        (WizardStep::ProviderType, None) => "Type? (openai / claude / ollama)".into(),
        (WizardStep::ProviderType, Some(p)) => {
            format!(
                "Type? [{}] (openai / claude / ollama, blank to keep)",
                p.provider_type
            )
        }
        (WizardStep::BaseUrl, None) => "Base URL? (blank to use provider default)".into(),
        (WizardStep::BaseUrl, Some(p)) => {
            let hint = p.base_url.as_deref().unwrap_or("provider default");
            format!("Base URL? [{}] (blank to keep)", hint)
        }
        (WizardStep::ApiKey, None) => "API key? (blank to leave unset)".into(),
        (WizardStep::ApiKey, Some(p)) => {
            let hint = if p.api_key.is_some() {
                "set — blank to keep"
            } else {
                "unset"
            };
            format!("API key? [{}]", hint)
        }
        (WizardStep::Model, None) => "Model?".into(),
        (WizardStep::Model, Some(p)) => format!("Model? [{}] (blank to keep)", p.model),
    }
}

/// Push the prompt for this step into scrollback + redraw footer.
fn show_step_prompt(
    step: WizardStep,
    existing: Option<&ProviderConfig>,
    buf: &Buffer,
    state: &UiState,
    ctx: &LoopCtx,
    wizard: &ProviderWizard,
    renderer: &mut dyn Renderer,
) {
    push(renderer, &step_prompt_text(step, existing));
    redraw(buf, state, ctx, wizard, renderer);
}

/// Validate and advance the "Add" sub-flow. Returns the next state, or
/// None when the wizard has committed / cancelled (caller clears).
fn advance_add(
    draft: &mut DraftProvider,
    step: WizardStep,
    answer: &str,
    renderer: &mut dyn Renderer,
) -> Option<WizardStep> {
    let ans = answer.trim();
    match step {
        WizardStep::Name => {
            if ans.is_empty() {
                push(renderer, "Name cannot be empty.");
                return Some(WizardStep::Name);
            }
            draft.name = ans.to_string();
            Some(WizardStep::ProviderType)
        }
        WizardStep::ProviderType => {
            if !["openai", "claude", "ollama"].contains(&ans) {
                push(renderer, "Unknown type. Choose openai / claude / ollama.");
                return Some(WizardStep::ProviderType);
            }
            draft.provider_type = ans.to_string();
            Some(WizardStep::BaseUrl)
        }
        WizardStep::BaseUrl => {
            draft.base_url = ans.to_string();
            Some(WizardStep::ApiKey)
        }
        WizardStep::ApiKey => {
            draft.api_key = ans.to_string();
            Some(WizardStep::Model)
        }
        WizardStep::Model => {
            if ans.is_empty() {
                push(renderer, "Model cannot be empty.");
                return Some(WizardStep::Model);
            }
            draft.model = ans.to_string();
            None // signal: ready to commit
        }
    }
}

/// Validate and advance the "Edit" sub-flow. Empty answers preserve
/// the existing value, so the caller needs `existing` to know what
/// that value is.
fn advance_edit(
    draft: &mut DraftProvider,
    step: WizardStep,
    answer: &str,
    renderer: &mut dyn Renderer,
) -> Option<WizardStep> {
    let ans = answer.trim();
    match step {
        WizardStep::Name => {
            // Name isn't editable (it's the key into the provider map).
            Some(WizardStep::ProviderType)
        }
        WizardStep::ProviderType => {
            if !ans.is_empty() && !["openai", "claude", "ollama"].contains(&ans) {
                push(
                    renderer,
                    "Unknown type. Choose openai / claude / ollama or leave blank.",
                );
                return Some(WizardStep::ProviderType);
            }
            draft.provider_type = ans.to_string();
            Some(WizardStep::BaseUrl)
        }
        WizardStep::BaseUrl => {
            draft.base_url = ans.to_string();
            Some(WizardStep::ApiKey)
        }
        WizardStep::ApiKey => {
            draft.api_key = ans.to_string();
            Some(WizardStep::Model)
        }
        WizardStep::Model => {
            draft.model = ans.to_string();
            None
        }
    }
}

/// Route a keystroke into `Buffer::apply` so text-input wizard steps
/// support the usual editing shortcuts (Backspace / Left / Right / etc).
fn forward_to_buffer(code: KeyCode, modifiers: KeyModifiers, buf: &mut Buffer, ctx: &LoopCtx) {
    let action = classify(code, modifiers);
    let _ = buf.apply(action, ctx.history.entries(), &ctx.commands);
}
