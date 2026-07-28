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
use atomcode_config::config::provider::{
    default_context_window_for, ModelProfileConfig, ProviderAccountConfig, ProviderConfig,
    ProviderPricing,
};
use atomcode_config::config::provider_preset;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{
    build_status, save_and_reload, set_default_provider_and_reload, Buffer, LoopCtx,
};
use crate::input::key_action::classify;
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

pub enum ProviderWizard {
    /// Initial picker: Add / Edit / Delete / Set Default.
    MainMenu { selected: usize },
    /// Sequential `Add` prompts. `draft` accumulates answered fields.
    /// The flow leads with a `Template` paste step (auto-detect). Once it
    /// resolves, `plan` holds the exact remaining questions (manual → all
    /// fields; import → only the gaps), and `idx` points at the current one.
    /// Progress is simply `(idx + 1, plan.len())` — no per-step guessing.
    Add {
        step: WizardStep,
        draft: DraftProvider,
        plan: Vec<WizardStep>,
        idx: usize,
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
    /// Add via a curated vendor preset. `selected` indexes
    /// [`provider_preset::PRESETS`]. Presets with a built-in endpoint run the
    /// quick two-prompt [`ProviderWizard::AddPreset`] flow; presets without one
    /// (the `*-compatible` custom endpoints) fall through to the manual
    /// [`ProviderWizard::Add`] flow.
    PresetPick { selected: usize },
    /// Quick add for a preset with a built-in endpoint: prompt API key (unless
    /// the preset is keyless, e.g. Ollama) then the model, and save as one
    /// account + one model profile.
    AddPreset {
        preset_idx: usize,
        step: PresetStep,
        account: String,
        api_key: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PresetStep {
    ApiKey,
    Model,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WizardStep {
    /// Add-only first step: paste a curl / JSON / TOML template (or a bare
    /// Base URL), or leave blank to fill everything manually.
    Template,
    Name,
    ProviderType,
    BaseUrl,
    ApiKey,
    Model,
    ContextWindow,
    /// Optional USD per million tokens: input,output,cached-input.
    Pricing,
}

#[derive(Clone, Debug, Default)]
pub struct DraftProvider {
    pub name: String,
    pub provider_type: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Max context tokens. `None` = the user left it blank, so fall back to
    /// the provider-type default ([`default_context_window_for`]) on Add or
    /// keep the existing value on Edit.
    pub context_window: Option<usize>,
    pub pricing: Option<ProviderPricing>,
    pub clear_pricing: bool,
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
        if let Some(cw) = self.context_window {
            base.context_window = cw;
        }
        if self.clear_pricing {
            base.pricing = None;
        } else if let Some(pricing) = self.pricing {
            base.pricing = Some(pricing);
        }
    }

    fn into_config(self) -> ProviderConfig {
        use atomcode_config::config::provider::default_context_window_for;
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
            context_window: self
                .context_window
                .unwrap_or_else(|| default_context_window_for(&provider_type)),
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
            pricing: self.pricing,
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
        push(
            renderer,
            &crate::i18n::t(crate::i18n::Msg::ProviderWizardCancelled),
        );
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
                            // Lead with vendor selection ("选厂商+填 key 即完成");
                            // the custom-endpoint presets fall through to the
                            // manual flow.
                            let new = ProviderWizard::PresetPick { selected: 0 };
                            redraw(buf, state, ctx, &new, renderer);
                            *wizard = new;
                        }
                        "edit" | "delete" | "set-default" if providers.is_empty() => {
                            push(
                                renderer,
                                &crate::i18n::t(crate::i18n::Msg::ProviderNoProviders),
                            );
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
                    push(
                        renderer,
                        &crate::i18n::t(crate::i18n::Msg::ProviderDeleteConfirm { name: &target }),
                    );
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
                    if set_default_provider_and_reload(ctx, &chosen, renderer) {
                        return Ok(ModalAction::Close);
                    }
                    *wizard = ProviderWizard::SetDefaultPick {
                        providers,
                        selected,
                    };
                    redraw(buf, state, ctx, wizard, renderer);
                    return Ok(ModalAction::Continue);
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
                    let mut desired = ctx.config.clone();
                    desired.providers.remove(&target);
                    // If we just dropped the default, fall back to any
                    // remaining provider or blank.
                    if desired.default_provider == target {
                        desired.default_provider =
                            desired.providers.keys().next().cloned().unwrap_or_default();
                    }
                    if save_and_reload(
                        ctx,
                        desired,
                        renderer,
                        crate::i18n::t(crate::i18n::Msg::ProviderDeleted { name: &target })
                            .into_owned(),
                        true,
                    ) {
                        return Ok(ModalAction::Close);
                    }
                    *wizard = ProviderWizard::DeleteConfirm { target };
                    redraw(buf, state, ctx, wizard, renderer);
                    return Ok(ModalAction::Continue);
                }
                _ => {
                    push(
                        renderer,
                        &crate::i18n::t(crate::i18n::Msg::ProviderDeleteKept),
                    );
                    Ok(ModalAction::Close)
                }
            }
        }

        // ── Text-input states: Enter submits, chars edit buf, others pass through Buffer. ──
        ProviderWizard::Add {
            step,
            mut draft,
            plan,
            idx,
        } => {
            if matches!(code, KeyCode::Enter) {
                // Expand any folded `[Pasted #N …]` placeholder so the
                // Template step parses the real pasted content.
                let answer = buf.expanded_text();
                // Don't echo the raw paste back for the (multi-line) template
                // step — `resolve_template` prints a "Detected:" summary instead.
                if !matches!(step, WizardStep::Template) {
                    push(renderer, &format!("  ↳ {}", answer));
                }
                buf.text.clear();
                buf.cursor = 0;

                // ── Pre-plan step: Template paste ──
                if matches!(step, WizardStep::Template) {
                    match resolve_template(&mut draft, &answer, &ctx.config.providers, renderer) {
                        TemplateOutcome::Manual => {
                            // Manual entry leads with a required Base URL — its
                            // own pre-plan step, no (x/y) counter.
                            let new = ProviderWizard::Add {
                                step: WizardStep::BaseUrl,
                                draft,
                                plan: Vec::new(),
                                idx: 0,
                            };
                            if let ProviderWizard::Add { step, draft, .. } = &new {
                                show_add_step_prompt(
                                    *step, draft, 0, 0, buf, state, ctx, &new, renderer,
                                );
                            }
                            *wizard = new;
                            return Ok(ModalAction::Continue);
                        }
                        TemplateOutcome::Retry => {
                            let new = ProviderWizard::Add {
                                step: WizardStep::Template,
                                draft,
                                plan: Vec::new(),
                                idx: 0,
                            };
                            show_add_step_prompt(
                                WizardStep::Template,
                                &DraftProvider::default(),
                                0,
                                0,
                                buf,
                                state,
                                ctx,
                                &new,
                                renderer,
                            );
                            *wizard = new;
                            return Ok(ModalAction::Continue);
                        }
                        // Import: draft pre-filled → fall through to enter the plan.
                        TemplateOutcome::Import => {}
                    }
                    ensure_name_default(&mut draft, &ctx.config.providers);
                    let plan = import_plan(&draft);
                    let total = plan.len();
                    let first = plan[0];
                    let new = ProviderWizard::Add {
                        step: first,
                        draft,
                        plan,
                        idx: 0,
                    };
                    if let ProviderWizard::Add { step, draft, .. } = &new {
                        show_add_step_prompt(
                            *step, draft, 0, total, buf, state, ctx, &new, renderer,
                        );
                    }
                    *wizard = new;
                    return Ok(ModalAction::Continue);
                }

                // ── Pre-plan step: manual Base URL (required) ──
                if matches!(step, WizardStep::BaseUrl) {
                    let url = answer.trim();
                    if url.is_empty() {
                        push(
                            renderer,
                            &crate::i18n::t(crate::i18n::Msg::ProviderBaseUrlEmpty),
                        );
                        let new = ProviderWizard::Add {
                            step: WizardStep::BaseUrl,
                            draft,
                            plan: Vec::new(),
                            idx: 0,
                        };
                        if let ProviderWizard::Add { step, draft, .. } = &new {
                            show_add_step_prompt(
                                *step, draft, 0, 0, buf, state, ctx, &new, renderer,
                            );
                        }
                        *wizard = new;
                        return Ok(ModalAction::Continue);
                    }
                    let ptype = apply_manual_base_url(&mut draft, url, &ctx.config.providers);
                    push(
                        renderer,
                        &crate::i18n::t(crate::i18n::Msg::ProviderTypeInferred {
                            type_name: ptype,
                        }),
                    );
                    let plan = import_plan(&draft);
                    let total = plan.len();
                    let first = plan[0];
                    let new = ProviderWizard::Add {
                        step: first,
                        draft,
                        plan,
                        idx: 0,
                    };
                    if let ProviderWizard::Add { step, draft, .. } = &new {
                        show_add_step_prompt(
                            *step, draft, 0, total, buf, state, ctx, &new, renderer,
                        );
                    }
                    *wizard = new;
                    return Ok(ModalAction::Continue);
                }

                // A planned question step.
                match store_step(&mut draft, step, &answer, &ctx.config.providers, renderer) {
                    StepOutcome::Retry => {
                        let total = plan.len();
                        let new = ProviderWizard::Add {
                            step,
                            draft,
                            plan,
                            idx,
                        };
                        if let ProviderWizard::Add { step, draft, .. } = &new {
                            show_add_step_prompt(
                                *step, draft, idx, total, buf, state, ctx, &new, renderer,
                            );
                        }
                        *wizard = new;
                        return Ok(ModalAction::Continue);
                    }
                    StepOutcome::Ok => {
                        let next_idx = idx + 1;
                        if next_idx >= plan.len() {
                            // All fields gathered — commit and switch to it, so
                            // the newly added entry is the current default
                            // without an extra /model step.
                            let name = draft.name.clone();
                            let model = draft.model.clone();
                            let cfg = draft.clone().into_config();
                            let mut desired = ctx.config.clone();
                            desired.providers.insert(name.clone(), cfg);
                            desired.default_provider = name.clone();
                            if save_and_reload(
                                ctx,
                                desired,
                                renderer,
                                crate::i18n::t(crate::i18n::Msg::ProviderAdded {
                                    name: &name,
                                    model: &model,
                                })
                                .into_owned(),
                                true,
                            ) {
                                return Ok(ModalAction::Close);
                            }
                            buf.text = answer;
                            buf.cursor = buf.text.len();
                            *wizard = ProviderWizard::Add {
                                step,
                                draft,
                                plan,
                                idx,
                            };
                            redraw(buf, state, ctx, wizard, renderer);
                            return Ok(ModalAction::Continue);
                        }
                        let next = plan[next_idx];
                        if matches!(next, WizardStep::Name) {
                            ensure_name_default(&mut draft, &ctx.config.providers);
                        }
                        let total = plan.len();
                        let new = ProviderWizard::Add {
                            step: next,
                            draft,
                            plan,
                            idx: next_idx,
                        };
                        if let ProviderWizard::Add { step, draft, .. } = &new {
                            show_add_step_prompt(
                                *step, draft, next_idx, total, buf, state, ctx, &new, renderer,
                            );
                        }
                        *wizard = new;
                        return Ok(ModalAction::Continue);
                    }
                }
            }
            // Forward other keys to the buffer so typing / editing works.
            forward_to_buffer(code, _mods, buf, state, ctx);
            *wizard = ProviderWizard::Add {
                step,
                draft,
                plan,
                idx,
            };
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }

        ProviderWizard::Edit {
            target,
            step,
            mut draft,
        } => {
            if matches!(code, KeyCode::Enter) {
                let answer = buf.expanded_text();
                push(
                    renderer,
                    &format!(
                        "  ↳ {}",
                        if answer.is_empty() {
                            crate::i18n::t(crate::i18n::Msg::ProviderEditKeep).into_owned()
                        } else {
                            answer.clone()
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
                        let mut desired = ctx.config.clone();
                        if let Some(existing) = desired.providers.get_mut(&target) {
                            draft.apply_onto(existing);
                        }
                        if save_and_reload(
                            ctx,
                            desired,
                            renderer,
                            crate::i18n::t(crate::i18n::Msg::ProviderUpdated { name: &target })
                                .into_owned(),
                            false,
                        ) {
                            return Ok(ModalAction::Close);
                        }
                        buf.text = answer;
                        buf.cursor = buf.text.len();
                        *wizard = ProviderWizard::Edit {
                            target,
                            step,
                            draft,
                        };
                        redraw(buf, state, ctx, wizard, renderer);
                        return Ok(ModalAction::Continue);
                    }
                }
            }
            forward_to_buffer(code, _mods, buf, state, ctx);
            *wizard = ProviderWizard::Edit {
                target,
                step,
                draft,
            };
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }

        // ── Preset vendor picker ──
        ProviderWizard::PresetPick { mut selected } => {
            let presets = provider_preset::PRESETS;
            match code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < presets.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let preset = &presets[selected];
                    if preset.default_base_url.is_some() {
                        // Quick flow: (API key unless keyless) → model.
                        let account = unique_account_id(preset.id, ctx);
                        let keyless =
                            matches!(preset.auth_kind, provider_preset::AuthKind::None);
                        let step = if keyless {
                            PresetStep::Model
                        } else {
                            PresetStep::ApiKey
                        };
                        push(
                            renderer,
                            &format!(
                                "  {} · {}",
                                preset.display_name,
                                preset.default_base_url.unwrap_or_default()
                            ),
                        );
                        push(renderer, &preset_step_prompt(step, preset));
                        *wizard = ProviderWizard::AddPreset {
                            preset_idx: selected,
                            step,
                            account,
                            api_key: String::new(),
                        };
                        redraw(buf, state, ctx, wizard, renderer);
                        return Ok(ModalAction::Continue);
                    }
                    // Custom-endpoint preset (no built-in URL): manual flow,
                    // pre-seeding the wire protocol.
                    let draft = DraftProvider {
                        provider_type: preset.provider_type.wire().to_string(),
                        ..DraftProvider::default()
                    };
                    let new = ProviderWizard::Add {
                        step: WizardStep::Template,
                        draft: draft.clone(),
                        plan: Vec::new(),
                        idx: 0,
                    };
                    show_add_step_prompt(
                        WizardStep::Template,
                        &draft,
                        0,
                        0,
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
            *wizard = ProviderWizard::PresetPick { selected };
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }

        // ── Quick preset add: API key → model, saved as account + model ──
        ProviderWizard::AddPreset {
            preset_idx,
            step,
            account,
            mut api_key,
        } => {
            if matches!(code, KeyCode::Enter) {
                let answer = buf.expanded_text();
                let shown = if matches!(step, PresetStep::ApiKey) && !answer.trim().is_empty() {
                    "••••••".to_string()
                } else {
                    answer.clone()
                };
                push(renderer, &format!("  ↳ {}", shown));
                buf.text.clear();
                buf.cursor = 0;
                let preset = &provider_preset::PRESETS[preset_idx];
                match step {
                    PresetStep::ApiKey => {
                        api_key = answer.trim().to_string();
                        push(renderer, &preset_step_prompt(PresetStep::Model, preset));
                        *wizard = ProviderWizard::AddPreset {
                            preset_idx,
                            step: PresetStep::Model,
                            account,
                            api_key,
                        };
                        redraw(buf, state, ctx, wizard, renderer);
                        return Ok(ModalAction::Continue);
                    }
                    PresetStep::Model => {
                        let model = answer.trim().to_string();
                        if model.is_empty() {
                            push(renderer, &preset_step_prompt(PresetStep::Model, preset));
                            *wizard = ProviderWizard::AddPreset {
                                preset_idx,
                                step: PresetStep::Model,
                                account,
                                api_key,
                            };
                            redraw(buf, state, ctx, wizard, renderer);
                            return Ok(ModalAction::Continue);
                        }
                        if commit_preset_account(ctx, renderer, preset, &account, &api_key, &model) {
                            return Ok(ModalAction::Close);
                        }
                        *wizard = ProviderWizard::AddPreset {
                            preset_idx,
                            step: PresetStep::Model,
                            account,
                            api_key,
                        };
                        redraw(buf, state, ctx, wizard, renderer);
                        return Ok(ModalAction::Continue);
                    }
                }
            }
            forward_to_buffer(code, _mods, buf, state, ctx);
            *wizard = ProviderWizard::AddPreset {
                preset_idx,
                step,
                account,
                api_key,
            };
            redraw(buf, state, ctx, wizard, renderer);
            Ok(ModalAction::Continue)
        }
    }
}

/// A unique account id derived from the preset id, avoiding collisions with
/// existing accounts or legacy provider names.
fn unique_account_id(base: &str, ctx: &LoopCtx) -> String {
    let taken = |id: &str| {
        ctx.config.provider_accounts.contains_key(id) || ctx.config.providers.contains_key(id)
    };
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_else(|| base.to_string())
}

/// Prompt text for a preset add step.
fn preset_step_prompt(step: PresetStep, preset: &provider_preset::ProviderPreset) -> String {
    match step {
        PresetStep::ApiKey => {
            let env = preset
                .api_key_env
                .map(|e| format!(" (leave blank to use ${e})"))
                .unwrap_or_default();
            format!("API key for {}?{}", preset.display_name, env)
        }
        PresetStep::Model => format!("Model? (e.g. the model id from {})", preset.display_name),
    }
}

/// Pure builder: turn a preset add into an `(account, model, model_id)` triple.
/// Separated from the ctx/save so it can be unit-tested.
fn build_preset_entry(
    preset: &provider_preset::ProviderPreset,
    account_id: &str,
    api_key: &str,
    model_name: &str,
) -> (ProviderAccountConfig, ModelProfileConfig, String) {
    let model_id = format!("{account_id}/{model_name}");
    let account = ProviderAccountConfig {
        provider: preset.id.to_string(),
        display_name: None,
        api_key: (!api_key.trim().is_empty()).then(|| api_key.trim().to_string()),
        base_url: None,
        user_agent: None,
        skip_tls_verify: false,
        enterprise_url: None,
        ephemeral: false,
    };
    let model = ModelProfileConfig {
        account: account_id.to_string(),
        model: model_name.to_string(),
        display_name: None,
        system_prompt: None,
        context_window: default_context_window_for(preset.provider_type.wire()),
        max_tokens: None,
        capable_model: None,
        thinking_type: None,
        thinking_keep: None,
        reasoning_history: None,
        reasoning_effort: None,
        thinking_enabled: None,
        thinking_budget: None,
        pricing: None,
    };
    (account, model, model_id)
}

/// Build one account + one model profile from a preset add and save+reload,
/// making the new model the default.
fn commit_preset_account(
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    preset: &provider_preset::ProviderPreset,
    account_id: &str,
    api_key: &str,
    model_name: &str,
) -> bool {
    let (account, model, model_id) = build_preset_entry(preset, account_id, api_key, model_name);
    let mut desired = ctx.config.clone();
    desired
        .provider_accounts
        .insert(account_id.to_string(), account);
    desired.models.insert(model_id.clone(), model);
    desired.default_model = Some(model_id);
    save_and_reload(
        ctx,
        desired,
        renderer,
        crate::i18n::t(crate::i18n::Msg::ProviderAdded {
            name: account_id,
            model: model_name,
        })
        .into_owned(),
        true,
    )
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
                (
                    crate::i18n::t(crate::i18n::Msg::ProviderMenuAdd).into_owned(),
                    crate::i18n::t(crate::i18n::Msg::ProviderMenuAddDesc).into_owned(),
                ),
                (
                    crate::i18n::t(crate::i18n::Msg::ProviderMenuEdit).into_owned(),
                    crate::i18n::t(crate::i18n::Msg::ProviderMenuEditDesc).into_owned(),
                ),
                (
                    crate::i18n::t(crate::i18n::Msg::ProviderMenuDelete).into_owned(),
                    crate::i18n::t(crate::i18n::Msg::ProviderMenuDeleteDesc).into_owned(),
                ),
                (
                    crate::i18n::t(crate::i18n::Msg::ProviderMenuSetDefault).into_owned(),
                    crate::i18n::t(crate::i18n::Msg::ProviderMenuSetDefaultDesc).into_owned(),
                ),
            ],
            selected: *selected,
            kind: crate::render::MenuKind::Action,
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
        ProviderWizard::PresetPick { selected } => {
            let items: Vec<(String, String)> = provider_preset::PRESETS
                .iter()
                .map(|p| {
                    let desc = p
                        .default_base_url
                        .map(str::to_string)
                        .unwrap_or_else(|| "custom endpoint — you'll enter the URL".to_string());
                    (p.display_name.to_string(), desc)
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
        | ProviderWizard::AddPreset { .. }
        | ProviderWizard::DeleteConfirm { .. } => None,
    };
    renderer.render(UiLine::InputPrompt {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        menu,
        status: build_status(state, ctx),
        attachments: Vec::new(),
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
    use crate::i18n::{t, Msg};
    match (step, existing) {
        (WizardStep::Template, _) => t(Msg::ProviderImportPrompt).into_owned(),
        (WizardStep::Name, _) => t(Msg::ProviderStepName).into_owned(),
        (WizardStep::ProviderType, None) => t(Msg::ProviderStepType).into_owned(),
        (WizardStep::ProviderType, Some(p)) => t(Msg::ProviderStepTypeWithHint {
            current: &p.provider_type,
        })
        .into_owned(),
        (WizardStep::BaseUrl, None) => t(Msg::ProviderStepBaseUrl).into_owned(),
        (WizardStep::BaseUrl, Some(p)) => {
            let default_hint = t(Msg::ProviderDefaultHint);
            let hint = p.base_url.as_deref().unwrap_or(&default_hint);
            t(Msg::ProviderStepBaseUrlWithHint { current: hint }).into_owned()
        }
        (WizardStep::ApiKey, None) => t(Msg::ProviderStepApiKey).into_owned(),
        (WizardStep::ApiKey, Some(p)) => {
            let hint = if p.api_key.is_some() {
                t(Msg::ProviderStepApiKeySet)
            } else {
                t(Msg::ProviderStepApiKeyUnset)
            };
            t(Msg::ProviderStepApiKeyWithHint { hint: &hint }).into_owned()
        }
        (WizardStep::Model, None) => t(Msg::ProviderStepModel).into_owned(),
        (WizardStep::Model, Some(p)) => {
            t(Msg::ProviderStepModelWithHint { current: &p.model }).into_owned()
        }
        (WizardStep::ContextWindow, None) => {
            use atomcode_config::config::provider::default_context_window_for;
            // No provider_type here (Add uses show_add_step_prompt); fall back
            // to the openai default just for the bare prompt.
            t(Msg::ProviderStepContextWindow {
                default: default_context_window_for("openai"),
            })
            .into_owned()
        }
        (WizardStep::ContextWindow, Some(p)) => t(Msg::ProviderStepContextWindowWithHint {
            current: p.context_window,
        })
        .into_owned(),
        (WizardStep::Pricing, None) => t(Msg::ProviderStepPricing).into_owned(),
        (WizardStep::Pricing, Some(p)) => match p.pricing {
            Some(pricing) => {
                let current = format!(
                    "{},{},{}",
                    pricing.input_per_million,
                    pricing.output_per_million,
                    pricing.cached_input_per_million
                );
                t(Msg::ProviderStepPricingWithHint { current: &current }).into_owned()
            }
            None => t(Msg::ProviderStepPricing).into_owned(),
        },
    }
}

/// Push an Add-flow prompt. Gap steps are prefixed with their
/// `(current/total)` progress; the `Template` step has no counter (it's the
/// entry, before the gap count is known). The Name step shows the
/// auto-derived default; the Template step shows the paste prompt.
fn show_add_step_prompt(
    step: WizardStep,
    draft: &DraftProvider,
    idx: usize,
    total: usize,
    buf: &Buffer,
    state: &UiState,
    ctx: &LoopCtx,
    wizard: &ProviderWizard,
    renderer: &mut dyn Renderer,
) {
    use crate::i18n::{t, Msg};
    let body = match step {
        WizardStep::Template => t(Msg::ProviderImportPrompt).into_owned(),
        WizardStep::Name => t(Msg::ProviderStepNameDefault {
            default: &draft.name,
        })
        .into_owned(),
        WizardStep::ContextWindow => {
            use atomcode_config::config::provider::default_context_window_for;
            t(Msg::ProviderStepContextWindow {
                default: default_context_window_for(&draft.provider_type),
            })
            .into_owned()
        }
        WizardStep::Pricing => t(Msg::ProviderStepPricing).into_owned(),
        other => step_prompt_text(other, None),
    };
    if matches!(step, WizardStep::Template) || total == 0 {
        push(renderer, &body);
    } else {
        let progress = t(Msg::ProviderStepProgress {
            current: idx + 1,
            total,
        });
        push(renderer, &format!("{progress} {body}"));
    }
    redraw(buf, state, ctx, wizard, renderer);
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

/// What the Template paste step decided.
#[derive(Debug, PartialEq)]
enum TemplateOutcome {
    /// Blank input → fill everything by hand.
    Manual,
    /// A template was recognized and `draft` pre-filled from it.
    Import,
    /// Non-blank but not a recognizable template → re-prompt.
    Retry,
}

/// Handle the Template paste step. Blank → manual. A recognized template
/// (curl / JSON / TOML, anything yielding a URL) pre-fills the draft and
/// echoes a summary. Anything else (incl. a bare URL or hostname) is *not*
/// treated as a base_url here — that belongs in the manual Base URL step —
/// so it re-prompts.
fn resolve_template(
    draft: &mut DraftProvider,
    answer: &str,
    existing: &std::collections::HashMap<String, ProviderConfig>,
    renderer: &mut dyn Renderer,
) -> TemplateOutcome {
    use crate::i18n::{t, Msg};
    if answer.trim().is_empty() {
        return TemplateOutcome::Manual;
    }
    let parsed = parse_template(answer);
    let Some(raw_url) = parsed.url.as_deref() else {
        push(renderer, &t(Msg::ProviderImportFailed));
        return TemplateOutcome::Retry;
    };
    let ptype = infer_type(raw_url);
    draft.provider_type = ptype.to_string();
    draft.base_url = normalize_base_url(raw_url, ptype);
    if let Some(k) = parsed.api_key {
        if !is_placeholder(&k) {
            draft.api_key = k;
        }
    }
    if let Some(m) = parsed.model {
        if !m.trim().is_empty() {
            draft.model = m;
        }
    }
    let base = parsed
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| derive_name(&draft.base_url, ptype, |n| existing.contains_key(n)));
    draft.name = dedupe_name(&base, |n| existing.contains_key(n));
    let model_disp = if draft.model.is_empty() {
        "?"
    } else {
        &draft.model
    };
    push(
        renderer,
        &t(Msg::ProviderImportParsed {
            base_url: &draft.base_url,
            type_name: ptype,
            model: model_disp,
        }),
    );
    TemplateOutcome::Import
}

/// Apply a manually-entered (required) Base URL: store it, infer + set the
/// provider type, and seed the name default. Returns the inferred type so the
/// caller can echo it. After this the manual flow joins `import_plan`.
fn apply_manual_base_url(
    draft: &mut DraftProvider,
    url: &str,
    existing: &std::collections::HashMap<String, ProviderConfig>,
) -> &'static str {
    let ptype = infer_type(url);
    draft.base_url = url.to_string();
    draft.provider_type = ptype.to_string();
    ensure_name_default(draft, existing);
    ptype
}

/// Post-import questions: only the gaps the template didn't fill, then the
/// Name confirmation (always shown).
fn import_plan(draft: &DraftProvider) -> Vec<WizardStep> {
    let mut plan = Vec::new();
    if draft.api_key.is_empty() {
        plan.push(WizardStep::ApiKey);
    }
    if draft.model.is_empty() {
        plan.push(WizardStep::Model);
    }
    // Always offer the context window (blank keeps the provider-type default),
    // then confirm the Name last.
    plan.push(WizardStep::ContextWindow);
    plan.push(WizardStep::Pricing);
    plan.push(WizardStep::Name);
    plan
}

/// Parse a user-typed context-window size into a token count. Accepts plain
/// digits, `_` / `,` grouping, and a `k` / `m` suffix (`128k` → 128_000,
/// `1m` → 1_000_000). Returns `None` for empty, non-numeric, or zero input so
/// the caller can re-prompt or fall back to a default.
fn parse_context_window(s: &str) -> Option<usize> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    let (digits, mult) = match t.chars().last() {
        Some('k') | Some('K') => (&t[..t.len() - 1], 1_000usize),
        Some('m') | Some('M') => (&t[..t.len() - 1], 1_000_000usize),
        _ => (t, 1usize),
    };
    let cleaned: String = digits.chars().filter(|c| *c != '_' && *c != ',').collect();
    if cleaned.is_empty() || !cleaned.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n = cleaned.parse::<usize>().ok()?.checked_mul(mult)?;
    (n > 0).then_some(n)
}

fn parse_pricing(s: &str) -> Option<ProviderPricing> {
    let values: Vec<_> = s.split(',').map(str::trim).collect();
    if values.len() != 3 {
        return None;
    }
    ProviderPricing {
        input_per_million: values[0].parse().ok()?,
        output_per_million: values[1].parse().ok()?,
        cached_input_per_million: values[2].parse().ok()?,
    }
    .validated()
}

/// Seed the Name step's default if it isn't set yet.
fn ensure_name_default(
    draft: &mut DraftProvider,
    existing: &std::collections::HashMap<String, ProviderConfig>,
) {
    if draft.name.is_empty() {
        draft.name = derive_name(&draft.base_url, &draft.provider_type, |n| {
            existing.contains_key(n)
        });
    }
}

/// Outcome of validating + storing one planned question's answer.
#[derive(Debug, PartialEq)]
enum StepOutcome {
    /// Stored; advance to the next planned step.
    Ok,
    /// Invalid input; re-prompt the same step.
    Retry,
}

/// Validate and store a single planned Add question. Re-prompts (Retry) on a
/// required-field violation; otherwise stores into `draft` and returns Ok.
fn store_step(
    draft: &mut DraftProvider,
    step: WizardStep,
    answer: &str,
    existing: &std::collections::HashMap<String, ProviderConfig>,
    renderer: &mut dyn Renderer,
) -> StepOutcome {
    let ans = answer.trim();
    match step {
        WizardStep::ApiKey => draft.api_key = ans.to_string(),
        WizardStep::Model => {
            if ans.is_empty() {
                push(
                    renderer,
                    &crate::i18n::t(crate::i18n::Msg::ProviderModelEmpty),
                );
                return StepOutcome::Retry;
            }
            draft.model = ans.to_string();
        }
        WizardStep::Name => {
            let chosen = if ans.is_empty() {
                draft.name.clone()
            } else {
                ans.to_string()
            };
            draft.name = dedupe_name(&chosen, |n| existing.contains_key(n));
        }
        WizardStep::ContextWindow => {
            // Blank keeps the provider-type default (draft stays None);
            // a non-empty value must parse to a positive token count.
            if !ans.is_empty() {
                match parse_context_window(ans) {
                    Some(n) => draft.context_window = Some(n),
                    None => {
                        push(
                            renderer,
                            &crate::i18n::t(crate::i18n::Msg::ProviderContextWindowInvalid),
                        );
                        return StepOutcome::Retry;
                    }
                }
            }
        }
        WizardStep::Pricing => {
            if ans.eq_ignore_ascii_case("clear") || ans == "清除" {
                draft.pricing = None;
                draft.clear_pricing = true;
            } else if !ans.is_empty() {
                let Some(pricing) = parse_pricing(ans) else {
                    push(renderer, &crate::i18n::t(crate::i18n::Msg::ProviderPricingInvalid));
                    return StepOutcome::Retry;
                };
                draft.pricing = Some(pricing);
                draft.clear_pricing = false;
            }
        }
        // Template / Base URL / Type are pre-plan steps, never planned.
        WizardStep::Template | WizardStep::BaseUrl | WizardStep::ProviderType => {}
    }
    StepOutcome::Ok
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
        // Template is an Add-only step; Edit never reaches it.
        WizardStep::Template | WizardStep::Name => {
            // Name isn't editable (it's the key into the provider map).
            Some(WizardStep::ProviderType)
        }
        WizardStep::ProviderType => {
            if !ans.is_empty() && !["openai", "claude", "ollama"].contains(&ans) {
                push(
                    renderer,
                    &crate::i18n::t(crate::i18n::Msg::ProviderUnknownTypeEdit),
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
            Some(WizardStep::ContextWindow)
        }
        WizardStep::ContextWindow => {
            // Blank keeps the existing value (apply_onto leaves it untouched);
            // a non-empty value must parse, else re-prompt the same step.
            if !ans.is_empty() {
                match parse_context_window(ans) {
                    Some(n) => draft.context_window = Some(n),
                    None => {
                        push(
                            renderer,
                            &crate::i18n::t(crate::i18n::Msg::ProviderContextWindowInvalid),
                        );
                        return Some(WizardStep::ContextWindow);
                    }
                }
            }
            Some(WizardStep::Pricing)
        }
        WizardStep::Pricing => {
            if ans.eq_ignore_ascii_case("clear") || ans == "清除" {
                draft.pricing = None;
                draft.clear_pricing = true;
            } else if !ans.is_empty() {
                let Some(pricing) = parse_pricing(ans) else {
                    push(renderer, &crate::i18n::t(crate::i18n::Msg::ProviderPricingInvalid));
                    return Some(WizardStep::Pricing);
                };
                draft.pricing = Some(pricing);
                draft.clear_pricing = false;
            }
            None
        }
    }
}

/// Raw fields pulled out of a pasted template (curl / JSON / TOML) before
/// any normalization. `url` is the endpoint as written; `api_key` may still
/// be a placeholder. Type / base_url / name are derived from these later.
#[derive(Debug, Default, PartialEq)]
struct ParsedTemplate {
    url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    name: Option<String>,
}

/// Extract provider fields from a pasted template. Detects a curl command
/// (headers + URL + JSON body) or a key/value block (JSON or TOML).
fn parse_template(input: &str) -> ParsedTemplate {
    let lower = input.to_ascii_lowercase();
    let is_curl = lower.contains("curl")
        || lower.contains("authorization")
        || lower.contains("x-api-key")
        || lower.contains(" -h ")
        || lower.contains("-d ");
    if is_curl {
        ParsedTemplate {
            url: first_url(input),
            api_key: curl_api_key(input),
            model: quoted_value_after(input, "model"),
            name: None,
        }
    } else {
        ParsedTemplate {
            url: quoted_value_after(input, "base_url")
                .or_else(|| quoted_value_after(input, "baseurl")),
            api_key: quoted_value_after(input, "api_key")
                .or_else(|| quoted_value_after(input, "apikey")),
            model: quoted_value_after(input, "model"),
            name: toml_provider_name(input),
        }
    }
}

/// First `http(s)://…` token in the text, read up to the next whitespace,
/// quote or line-continuation backslash.
fn first_url(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let start = ["https://", "http://"]
        .iter()
        .filter_map(|s| lower.find(s))
        .min()?;
    let rest = &input[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '\\')
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// API key from a curl command: `Authorization: Bearer X` or `x-api-key: X`.
fn curl_api_key(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    if let Some(i) = lower.find("authorization") {
        let rest = &input[i..];
        if let Some(b) = lower[i..].find("bearer") {
            return Some(take_token(&rest[b + "bearer".len()..]));
        }
        if let Some(c) = rest.find(':') {
            return Some(take_token(&rest[c + 1..]));
        }
    }
    if let Some(i) = lower.find("x-api-key") {
        let rest = &input[i + "x-api-key".len()..];
        if let Some(c) = rest.find(':') {
            return Some(take_token(&rest[c + 1..]));
        }
    }
    None
}

/// Trim leading whitespace then read up to the next whitespace/quote/backslash.
fn take_token(s: &str) -> String {
    let s = s.trim_start();
    let end = s
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '\\')
        .unwrap_or(s.len());
    s[..end].to_string()
}

/// Value of a `key: "value"` / `key = "value"` pair (JSON or TOML style),
/// matching the key case-insensitively. Skips the key's own closing quote.
fn quoted_value_after(input: &str, key: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    let key_l = key.to_ascii_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(&key_l) {
        let pos = from + rel + key.len();
        let rest = &input[pos..];
        if let Some(sep) = rest.find(|c| c == ':' || c == '=') {
            // Only quotes/whitespace may sit between the key and its separator.
            if rest[..sep]
                .chars()
                .all(|c| c == '"' || c == '\'' || c.is_whitespace())
            {
                let after_sep = &rest[sep + 1..];
                if let Some(oq) = after_sep.find(|c| c == '"' || c == '\'') {
                    let quote = after_sep.as_bytes()[oq] as char;
                    let val_start = &after_sep[oq + 1..];
                    if let Some(end) = val_start.find(quote) {
                        return Some(val_start[..end].to_string());
                    }
                }
            }
        }
        from = pos;
    }
    None
}

/// Provider name from a TOML `[providers.NAME]` header.
fn toml_provider_name(input: &str) -> Option<String> {
    let i = input.find("[providers.")?;
    let rest = &input[i + "[providers.".len()..];
    let end = rest.find(']')?;
    Some(rest[..end].trim().to_string())
}

/// Normalize a raw endpoint URL into a provider `base_url` per its type:
/// strip the request path for claude/ollama, strip only the trailing
/// endpoint segment for openai (keeping prefixes like `/v1`).
fn normalize_base_url(raw_url: &str, provider_type: &str) -> String {
    let url = raw_url.trim().trim_end_matches('/');
    match provider_type {
        // Claude / Ollama base URLs are just the authority; drop any path.
        "claude" | "ollama" => match url.find("://") {
            Some(scheme_end) => {
                let after = scheme_end + 3;
                let authority_end = url[after..].find('/').map_or(url.len(), |i| after + i);
                url[..authority_end].to_string()
            }
            None => url.to_string(),
        },
        // OpenAI-compatible: keep prefixes like `/v1`, strip the endpoint.
        _ => {
            const ENDPOINTS: [&str; 5] = [
                "/chat/completions",
                "/completions",
                "/embeddings",
                "/responses",
                "/messages",
            ];
            for ep in ENDPOINTS {
                if let Some(stripped) = url.strip_suffix(ep) {
                    return stripped.to_string();
                }
            }
            url.to_string()
        }
    }
}

/// Whether an extracted API key is a documentation placeholder (e.g.
/// `<API_KEY>`, `YOUR_API_KEY`, `sk-xxxx`) rather than a real secret.
fn is_placeholder(value: &str) -> bool {
    let v = value.trim();
    if v.is_empty() {
        return true;
    }
    // Shell / env variable reference: `$OPENROUTER_API_KEY`, `${API_KEY}`.
    if v.starts_with('$') {
        return true;
    }
    // Angle-bracket placeholder: `<API_KEY>`, `<your-key-here>`.
    if v.starts_with('<') && v.ends_with('>') {
        return true;
    }
    // Bare env-var-style name: `OPENROUTER_API_KEY`, `INSERT_TOKEN_HERE`
    // (all-caps + underscores, mentioning key/token/secret). Real keys are
    // mixed-case and rarely use this shape.
    if v.contains('_')
        && v.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && (v.contains("KEY") || v.contains("TOKEN") || v.contains("SECRET"))
    {
        return true;
    }
    let lower = v.to_ascii_lowercase();
    if lower.contains("your") && lower.contains("key") {
        return true;
    }
    if matches!(
        lower.as_str(),
        "api_key" | "apikey" | "placeholder" | "token"
    ) {
        return true;
    }
    // A run of filler chars like `sk-xxxx` or `****`.
    let bytes = lower.as_bytes();
    bytes.windows(4).any(|w| w == b"xxxx") || v.as_bytes().windows(4).any(|w| w == b"****")
}

/// Infer the provider `type` from a pasted base URL. Most third-party
/// endpoints are OpenAI-compatible, so `openai` is the catch-all.
fn infer_type(base_url: &str) -> &'static str {
    let url = base_url.to_ascii_lowercase();
    if url.contains("anthropic") {
        "claude"
    } else if url.contains("ollama") || url.contains("11434") {
        "ollama"
    } else {
        "openai"
    }
}

/// Derive a unique provider name from a base URL (falling back to the
/// provider type when there's no usable host). `is_taken` reports whether
/// a candidate name already exists, so collisions get a `-N` suffix.
fn derive_name(base_url: &str, provider_type: &str, is_taken: impl Fn(&str) -> bool) -> String {
    // host = base_url minus scheme, path and port.
    let host = base_url
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    let labels: Vec<&str> = host.split('.').filter(|s| !s.is_empty()).collect();
    // Prefer the registrable domain — the label just before the public
    // suffix — so `api-ai.gitcode.com` → `gitcode`, not `api-ai`. Hop over a
    // generic second-level like `com` in `example.com.cn`.
    let label = if labels.len() >= 2 {
        let mut i = labels.len() - 2;
        const GENERIC_SLD: [&str; 7] = ["com", "net", "org", "co", "gov", "edu", "ac"];
        if i >= 1 && GENERIC_SLD.contains(&labels[i]) {
            i -= 1;
        }
        labels[i]
    } else {
        ""
    };
    // No usable host (blank / localhost / IP literal) → name after the type.
    let base = if label.chars().any(|c| c.is_ascii_alphabetic()) {
        label
    } else {
        provider_type
    };

    dedupe_name(base, is_taken)
}

/// Make `base` unique by appending `-2`, `-3`, … until `is_taken` is false.
fn dedupe_name(base: &str, is_taken: impl Fn(&str) -> bool) -> String {
    if !is_taken(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}");
        if !is_taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Route a keystroke into `Buffer::apply` so text-input wizard steps
/// support the usual editing shortcuts (Backspace / Left / Right / etc).
fn forward_to_buffer(
    code: KeyCode,
    modifiers: KeyModifiers,
    buf: &mut Buffer,
    state: &mut UiState,
    ctx: &LoopCtx,
) {
    let action = classify(code, modifiers);
    let _ = buf.apply(action, ctx.history.entries(), &ctx.commands);
    crate::event_loop::sync_recalled_attachments(state, buf, ctx.history.entries());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_add_builds_a_resolvable_account_and_model() {
        // "select vendor + add key" → one account + one model, resolvable through
        // the single boundary using the preset's built-in endpoint.
        let preset = provider_preset::preset("deepseek").unwrap();
        let (account, model, model_id) =
            build_preset_entry(preset, "deepseek", "sk-test", "deepseek-chat");
        assert_eq!(model_id, "deepseek/deepseek-chat");
        assert_eq!(account.provider, "deepseek");
        assert_eq!(account.api_key.as_deref(), Some("sk-test"));
        assert_eq!(model.account, "deepseek");
        assert_eq!(model.model, "deepseek-chat");

        let mut cfg = atomcode_config::config::Config::default();
        cfg.provider_accounts.insert("deepseek".into(), account);
        cfg.models.insert(model_id.clone(), model);
        cfg.default_model = Some(model_id);
        assert!(cfg.validate_provider_accounts_and_models().is_empty());
        let r = cfg.resolve_model(None).unwrap();
        assert_eq!(r.model, "deepseek-chat");
        assert_eq!(r.api_key.as_deref(), Some("sk-test"));
        assert_eq!(r.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
    }

    #[test]
    fn parse_template_curl_openai() {
        let input = r#"curl -X POST https://taotoken.net/api/v1/chat/completions \
  -H "Authorization: Bearer <API_KEY>" \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen3.7-max","messages":[{"role":"user","content":"你好"}]}'"#;
        let p = parse_template(input);
        assert_eq!(
            p.url.as_deref(),
            Some("https://taotoken.net/api/v1/chat/completions")
        );
        assert_eq!(p.api_key.as_deref(), Some("<API_KEY>"));
        assert_eq!(p.model.as_deref(), Some("qwen3.7-max"));
    }

    #[test]
    fn parse_template_curl_anthropic_x_api_key() {
        let input = r#"curl https://api.anthropic.com/v1/messages \
  -H "x-api-key: sk-ant-123" \
  -H "anthropic-version: 2023-06-01" \
  -d '{"model":"claude-opus-4","max_tokens":1024}'"#;
        let p = parse_template(input);
        assert_eq!(
            p.url.as_deref(),
            Some("https://api.anthropic.com/v1/messages")
        );
        assert_eq!(p.api_key.as_deref(), Some("sk-ant-123"));
        assert_eq!(p.model.as_deref(), Some("claude-opus-4"));
    }

    #[test]
    fn parse_template_python_requests_snippet() {
        // Real-world paste: a Python `requests` example. The `Authorization`
        // header puts it in curl mode; URL / Bearer key / model still resolve.
        let input = r#"import requests
API_URL = "https://api-ai.gitcode.com/v1/chat/completions"
headers = {
    "Authorization": f"Bearer MWtoCL7zchAZpQGssV6uDdRE",
}
chunks = query({
    "model": "deepseek-ai/DeepSeek-R1",
    "stream": True,
})"#;
        let p = parse_template(input);
        assert_eq!(
            p.url.as_deref(),
            Some("https://api-ai.gitcode.com/v1/chat/completions")
        );
        assert_eq!(p.api_key.as_deref(), Some("MWtoCL7zchAZpQGssV6uDdRE"));
        assert_eq!(p.model.as_deref(), Some("deepseek-ai/DeepSeek-R1"));
    }

    #[test]
    fn parse_template_json_block() {
        let input =
            r#"{"type":"openai","base_url":"https://x.ai/v1","api_key":"sk-1","model":"grok-2"}"#;
        let p = parse_template(input);
        assert_eq!(p.url.as_deref(), Some("https://x.ai/v1"));
        assert_eq!(p.api_key.as_deref(), Some("sk-1"));
        assert_eq!(p.model.as_deref(), Some("grok-2"));
    }

    #[test]
    fn parse_template_toml_block() {
        let input = "[providers.foo]\ntype = \"openai\"\nbase_url = \"https://y.com/v1\"\napi_key = \"sk-2\"\nmodel = \"m2\"";
        let p = parse_template(input);
        assert_eq!(p.url.as_deref(), Some("https://y.com/v1"));
        assert_eq!(p.api_key.as_deref(), Some("sk-2"));
        assert_eq!(p.model.as_deref(), Some("m2"));
        assert_eq!(p.name.as_deref(), Some("foo"));
    }

    #[test]
    fn normalize_base_url_openai_strips_endpoint() {
        assert_eq!(
            normalize_base_url("https://taotoken.net/api/v1/chat/completions", "openai"),
            "https://taotoken.net/api/v1"
        );
    }

    #[test]
    fn normalize_base_url_openai_keeps_plain() {
        assert_eq!(
            normalize_base_url("https://x.ai/v1", "openai"),
            "https://x.ai/v1"
        );
    }

    #[test]
    fn normalize_base_url_claude_strips_path() {
        assert_eq!(
            normalize_base_url("https://api.anthropic.com/v1/messages", "claude"),
            "https://api.anthropic.com"
        );
    }

    #[test]
    fn normalize_base_url_ollama_keeps_host_port() {
        assert_eq!(
            normalize_base_url("http://localhost:11434/api/chat", "ollama"),
            "http://localhost:11434"
        );
    }

    #[test]
    fn is_placeholder_detects_angle_brackets() {
        assert!(is_placeholder("<API_KEY>"));
    }

    #[test]
    fn is_placeholder_detects_your_api_key() {
        assert!(is_placeholder("YOUR_API_KEY"));
    }

    #[test]
    fn is_placeholder_detects_x_run() {
        assert!(is_placeholder("sk-xxxxxxxx"));
    }

    #[test]
    fn is_placeholder_accepts_real_key() {
        assert!(!is_placeholder("sk-ant-abc123def456"));
        assert!(!is_placeholder("9f8c7b6a5"));
        assert!(!is_placeholder("MWtoCL7zchAZpQGssV6uDdRE"));
    }

    #[test]
    fn is_placeholder_detects_shell_var() {
        assert!(is_placeholder("$OPENROUTER_API_KEY"));
        assert!(is_placeholder("${OPENROUTER_API_KEY}"));
        assert!(is_placeholder("$API_KEY"));
    }

    #[test]
    fn is_placeholder_detects_env_var_name() {
        assert!(is_placeholder("OPENROUTER_API_KEY"));
        assert!(is_placeholder("INSERT_TOKEN_HERE"));
    }

    #[test]
    fn resolve_template_drops_shell_var_key_then_import_asks_key() {
        let input = r#"curl https://openrouter.ai/api/v1/chat/completions \
  -H "Authorization: Bearer $OPENROUTER_API_KEY" \
  -d '{"model":"stepfun/step-3.7-flash"}'"#;
        let mut d = DraftProvider::default();
        let existing = std::collections::HashMap::new();
        let mut sink = crate::render::plain::PlainRenderer::with_writer(std::io::sink());
        let outcome = resolve_template(&mut d, input, &existing, &mut sink);
        assert_eq!(outcome, TemplateOutcome::Import);
        assert_eq!(d.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(d.model, "stepfun/step-3.7-flash");
        assert!(
            d.api_key.is_empty(),
            "shell-var key must be dropped, got {:?}",
            d.api_key
        );
        assert_eq!(d.name, "openrouter");
        // Only gap is the key → plan asks ApiKey then Name.
        assert_eq!(
            import_plan(&d),
            vec![
                WizardStep::ApiKey,
                WizardStep::ContextWindow,
                WizardStep::Pricing,
                WizardStep::Name
            ]
        );
    }

    #[test]
    fn infer_type_detects_claude() {
        assert_eq!(infer_type("https://api.anthropic.com"), "claude");
    }

    #[test]
    fn infer_type_detects_ollama_by_port() {
        assert_eq!(infer_type("http://localhost:11434"), "ollama");
    }

    #[test]
    fn infer_type_detects_ollama_by_name() {
        assert_eq!(infer_type("http://my-ollama-host:8080"), "ollama");
    }

    #[test]
    fn infer_type_defaults_to_openai() {
        assert_eq!(infer_type("https://api.deepseek.com/v1"), "openai");
    }

    #[test]
    fn infer_type_empty_defaults_to_openai() {
        assert_eq!(infer_type(""), "openai");
    }

    #[test]
    fn derive_name_strips_api_prefix_and_tld() {
        assert_eq!(
            derive_name("https://api.deepseek.com/v1", "openai", |_| false),
            "deepseek"
        );
    }

    #[test]
    fn derive_name_handles_cn_tld() {
        assert_eq!(
            derive_name("https://api.moonshot.cn/v1", "openai", |_| false),
            "moonshot"
        );
    }

    #[test]
    fn derive_name_handles_no_api_prefix() {
        assert_eq!(
            derive_name("https://openrouter.ai/api/v1", "openai", |_| false),
            "openrouter"
        );
    }

    #[test]
    fn derive_name_prefers_registrable_domain_over_subdomain() {
        // api-ai.gitcode.com → gitcode (not "api-ai")
        assert_eq!(
            derive_name("https://api-ai.gitcode.com/v1", "openai", |_| false),
            "gitcode"
        );
        assert_eq!(
            derive_name("https://www.openrouter.ai/api/v1", "openai", |_| false),
            "openrouter"
        );
    }

    #[test]
    fn derive_name_handles_multi_part_tld() {
        // example.com.cn → example (hop over the generic `com`)
        assert_eq!(
            derive_name("https://api.example.com.cn/v1", "openai", |_| false),
            "example"
        );
    }

    #[test]
    fn derive_name_falls_back_to_type_for_localhost() {
        assert_eq!(
            derive_name("http://localhost:11434", "ollama", |_| false),
            "ollama"
        );
    }

    #[test]
    fn derive_name_falls_back_to_type_when_empty() {
        assert_eq!(derive_name("", "claude", |_| false), "claude");
    }

    #[test]
    fn derive_name_suffixes_on_collision() {
        assert_eq!(
            derive_name("https://api.deepseek.com/v1", "openai", |n| n == "deepseek"),
            "deepseek-2"
        );
    }

    fn draft_filled(provider_type: &str, api_key: &str, model: &str) -> DraftProvider {
        DraftProvider {
            provider_type: provider_type.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn apply_manual_base_url_infers_type_and_plans_gaps() {
        let mut d = DraftProvider::default();
        let existing = std::collections::HashMap::new();
        let ptype = apply_manual_base_url(&mut d, "https://api.anthropic.com", &existing);
        assert_eq!(ptype, "claude");
        assert_eq!(d.base_url, "https://api.anthropic.com");
        assert_eq!(d.provider_type, "claude");
        assert_eq!(d.name, "anthropic"); // derived from host
                                         // Manual entry never supplies key/model, so both are asked, then the
                                         // context window, then Name.
        assert_eq!(
            import_plan(&d),
            vec![
                WizardStep::ApiKey,
                WizardStep::Model,
                WizardStep::ContextWindow,
                WizardStep::Pricing,
                WizardStep::Name
            ]
        );
    }

    #[test]
    fn import_plan_only_covers_gaps() {
        // key + model missing → ask both, then context window, then Name.
        assert_eq!(
            import_plan(&draft_filled("openai", "", "")),
            vec![
                WizardStep::ApiKey,
                WizardStep::Model,
                WizardStep::ContextWindow,
                WizardStep::Pricing,
                WizardStep::Name
            ]
        );
        // everything filled → still offer the context window, then confirm Name.
        assert_eq!(
            import_plan(&draft_filled("openai", "sk-1", "gpt-4o")),
            vec![
                WizardStep::ContextWindow,
                WizardStep::Pricing,
                WizardStep::Name
            ]
        );
        // model already filled → skip it, keep context window before Name.
        assert_eq!(
            import_plan(&draft_filled("openai", "", "gpt-4o")),
            vec![
                WizardStep::ApiKey,
                WizardStep::ContextWindow,
                WizardStep::Pricing,
                WizardStep::Name
            ]
        );
    }

    #[test]
    fn parse_context_window_accepts_plain_grouping_and_suffixes() {
        assert_eq!(parse_context_window("128000"), Some(128_000));
        assert_eq!(parse_context_window("128_000"), Some(128_000));
        assert_eq!(parse_context_window("128,000"), Some(128_000));
        assert_eq!(parse_context_window("128k"), Some(128_000));
        assert_eq!(parse_context_window("200K"), Some(200_000));
        assert_eq!(parse_context_window("1m"), Some(1_000_000));
        assert_eq!(parse_context_window("  64000 "), Some(64_000));
        // Rejections → None (caller re-prompts or uses default).
        assert_eq!(parse_context_window("0"), None);
        assert_eq!(parse_context_window("banana"), None);
        assert_eq!(parse_context_window(""), None);
        assert_eq!(parse_context_window("12x"), None);
    }

    #[test]
    fn parse_pricing_requires_three_finite_non_negative_values() {
        let pricing = parse_pricing("2.5, 10, 0.25").unwrap();
        assert_eq!(pricing.input_per_million, 2.5);
        assert_eq!(pricing.output_per_million, 10.0);
        assert_eq!(pricing.cached_input_per_million, 0.25);
        assert!(parse_pricing("0,0,0").is_some());
        assert!(parse_pricing("-1,2,3").is_none());
        assert!(parse_pricing("1,2").is_none());
        assert!(parse_pricing("NaN,2,3").is_none());
    }

    #[test]
    fn store_step_context_window_parses_blank_and_invalid() {
        let existing = std::collections::HashMap::new();
        let mut sink = crate::render::plain::PlainRenderer::with_writer(std::io::sink());
        // Valid → stored.
        let mut d = draft_filled("openai", "sk", "gpt-4o");
        assert_eq!(
            store_step(
                &mut d,
                WizardStep::ContextWindow,
                "64000",
                &existing,
                &mut sink
            ),
            StepOutcome::Ok
        );
        assert_eq!(d.context_window, Some(64_000));
        // Blank → left None (→ provider-type default at into_config).
        let mut d2 = draft_filled("openai", "sk", "gpt-4o");
        assert_eq!(
            store_step(&mut d2, WizardStep::ContextWindow, "", &existing, &mut sink),
            StepOutcome::Ok
        );
        assert_eq!(d2.context_window, None);
        // Garbage → Retry, unchanged.
        let mut d3 = draft_filled("openai", "sk", "gpt-4o");
        assert_eq!(
            store_step(
                &mut d3,
                WizardStep::ContextWindow,
                "banana",
                &existing,
                &mut sink
            ),
            StepOutcome::Retry
        );
        assert_eq!(d3.context_window, None);
    }

    #[test]
    fn into_config_uses_entered_window_else_type_default() {
        // Entered value wins.
        let mut d = draft_filled("openai", "sk", "gpt-4o");
        d.context_window = Some(64_000);
        assert_eq!(d.into_config().context_window, 64_000);
        // None → provider-type default (ollama = 8000, openai = 128000).
        assert_eq!(
            draft_filled("ollama", "", "llama3")
                .into_config()
                .context_window,
            8_000
        );
        assert_eq!(
            draft_filled("openai", "sk", "gpt-4o")
                .into_config()
                .context_window,
            128_000
        );
    }

    #[test]
    fn advance_edit_flows_model_to_context_window_then_ends() {
        let mut sink = crate::render::plain::PlainRenderer::with_writer(std::io::sink());
        let mut d = DraftProvider::default();
        // Model now advances to ContextWindow (was the terminal step).
        assert_eq!(
            advance_edit(&mut d, WizardStep::Model, "gpt-4o", &mut sink),
            Some(WizardStep::ContextWindow)
        );
        // Valid entry advances to optional pricing, value captured.
        assert_eq!(
            advance_edit(&mut d, WizardStep::ContextWindow, "32000", &mut sink),
            Some(WizardStep::Pricing)
        );
        assert_eq!(d.context_window, Some(32_000));
        assert_eq!(
            advance_edit(&mut d, WizardStep::Pricing, "1,2,0.5", &mut sink),
            None
        );
        assert_eq!(d.pricing.unwrap().output_per_million, 2.0);
        // Blank context still advances, stays None so apply_onto keeps it.
        let mut d2 = DraftProvider::default();
        assert_eq!(
            advance_edit(&mut d2, WizardStep::ContextWindow, "", &mut sink),
            Some(WizardStep::Pricing)
        );
        assert_eq!(d2.context_window, None);
        // Invalid → re-prompt the same step.
        let mut d3 = DraftProvider::default();
        assert_eq!(
            advance_edit(&mut d3, WizardStep::ContextWindow, "xyz", &mut sink),
            Some(WizardStep::ContextWindow)
        );
    }

    #[test]
    fn apply_onto_sets_context_window_only_when_present() {
        let mut base = draft_filled("openai", "sk", "gpt-4o").into_config();
        assert_eq!(base.context_window, 128_000);
        // None → untouched (empty-Enter keeps existing).
        DraftProvider::default().apply_onto(&mut base);
        assert_eq!(base.context_window, 128_000);
        // Some → applied.
        let mut d = DraftProvider::default();
        d.context_window = Some(48_000);
        d.apply_onto(&mut base);
        assert_eq!(base.context_window, 48_000);
    }

    #[test]
    fn resolve_template_blank_goes_manual() {
        let mut d = DraftProvider::default();
        let existing = std::collections::HashMap::new();
        let mut sink = crate::render::plain::PlainRenderer::with_writer(std::io::sink());
        assert_eq!(
            resolve_template(&mut d, "", &existing, &mut sink),
            TemplateOutcome::Manual
        );
        assert!(d.base_url.is_empty());
    }

    #[test]
    fn resolve_template_curl_fills_draft() {
        let input = r#"curl https://taotoken.net/api/v1/chat/completions \
  -H "Authorization: Bearer <API_KEY>" \
  -d '{"model":"qwen3.7-max"}'"#;
        let mut d = DraftProvider::default();
        let existing = std::collections::HashMap::new();
        let mut sink = crate::render::plain::PlainRenderer::with_writer(std::io::sink());
        assert_eq!(
            resolve_template(&mut d, input, &existing, &mut sink),
            TemplateOutcome::Import
        );
        assert_eq!(d.provider_type, "openai");
        assert_eq!(d.base_url, "https://taotoken.net/api/v1");
        assert_eq!(d.model, "qwen3.7-max");
        assert!(d.api_key.is_empty(), "placeholder key must be dropped");
        assert_eq!(d.name, "taotoken");
        assert_eq!(
            import_plan(&d),
            vec![
                WizardStep::ApiKey,
                WizardStep::ContextWindow,
                WizardStep::Pricing,
                WizardStep::Name
            ]
        );
    }

    #[test]
    fn resolve_template_bare_url_reprompts_not_base_url() {
        // A bare URL is NOT a template — it belongs in the manual Base URL
        // step, so the Template step re-prompts instead of consuming it.
        let mut d = DraftProvider::default();
        let existing = std::collections::HashMap::new();
        let mut sink = crate::render::plain::PlainRenderer::with_writer(std::io::sink());
        assert_eq!(
            resolve_template(&mut d, "https://api.deepseek.com/v1", &existing, &mut sink),
            TemplateOutcome::Retry
        );
        assert!(d.base_url.is_empty());
    }

    #[test]
    fn derive_name_suffixes_until_free() {
        assert_eq!(
            derive_name("https://api.deepseek.com/v1", "openai", |n| n == "deepseek"
                || n == "deepseek-2"),
            "deepseek-3"
        );
    }
}
