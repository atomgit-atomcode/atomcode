//! Static task context contributed by the row owning the skill registry.
use crate::{
    agent::MessageOrigin,
    events::{PreStep, StepDecision},
    seams::{LlmSvc, SessionSvc},
    session::InjectionOrigin,
};
use async_trait::async_trait;
use atomcode_capabilities::skills::{
    auto_load::{select, Selection},
    SkillRegistry,
};
use atomcode_plexus::{Context, Next, Waterfall};
use std::sync::Arc;

pub fn mount(ctx: &Context, registry: Arc<SkillRegistry>, enabled: bool) {
    if !enabled {
        tracing::info!(target: "atomcode_skills", outcome = "disabled", reason = "configured_off", "automatic skill selection");
    }
    if enabled {
        let _ = ctx.on_waterfall::<PreStep>(Arc::new(AutoSkills(registry)), false);
    }
}
struct AutoSkills(Arc<SkillRegistry>);
#[async_trait]
impl Waterfall<PreStep> for AutoSkills {
    async fn handle(&self, step: &mut StepDecision, next: Next<'_, PreStep>) -> StepDecision {
        if step.step != 1 || !matches!(step.origin, MessageOrigin::User) || step.rejected.is_some()
        {
            return next.run(step).await;
        }
        if step
            .injections
            .iter()
            .any(|(_, origin)| matches!(origin, InjectionOrigin::Skill { .. }))
        {
            tracing::info!(target: "atomcode_skills", turn = step.turn, outcome = "skipped", reason = "explicit_skill", "automatic skill selection");
            return next.run(step).await;
        }
        let Some(prompt) = step.message.as_deref() else {
            return next.run(step).await;
        };
        match select(&self.0, prompt) {
            Selection::Skip(reason) => {
                tracing::info!(target: "atomcode_skills", turn = step.turn, outcome = "skipped", reason, "automatic skill selection")
            }
            Selection::Load(skill) => {
                // No calling agent, no contextual mutation. The same task-local
                // ownership seam is used by skill permissions and tool execution.
                if let Some(session) =
                    crate::agent::current().and_then(|ctx| ctx.service::<SessionSvc>())
                {
                    match skill.expand_static_for_injection(session.id()) {
                        Err(reason) => {
                            tracing::info!(target: "atomcode_skills", turn = step.turn, outcome = "skipped", skill = %skill.name, reason, "automatic skill selection")
                        }
                        Ok(body) => {
                            let content = format!("Automatically loaded static skill: {}. Follow its workflow for this task; it grants no tool permissions.\n\n{}", skill.name, body);
                            let window = crate::agent::current()
                                .and_then(|ctx| ctx.service::<LlmSvc>())
                                .map(|provider| provider.context_window())
                                .unwrap_or(0);
                            let budget = if window == 0 {
                                8192
                            } else {
                                (window as usize / 8).min(16 * 1024)
                            };
                            if content.len() > budget {
                                tracing::info!(target: "atomcode_skills", turn = step.turn, outcome = "skipped", skill = %skill.name, reason = "context_budget", "automatic skill selection");
                                return next.run(step).await;
                            }
                            let existing = step.injections.iter().any(|(text, _)| text == &content);
                            let retained = session
                                .derive_messages()
                                .iter()
                                .any(|message| message.text == content || message.text == body);
                            if existing || retained {
                                tracing::info!(target: "atomcode_skills", turn = step.turn, outcome = "skipped", skill = %skill.name, reason = "already_loaded", "automatic skill selection");
                            } else {
                                tracing::info!(target: "atomcode_skills", turn = step.turn, outcome = "loaded", skill = %skill.name, bytes = content.len(), reason = "debugging_task", "automatic skill selection");
                                // Skill provenance activates allowed-tools. A runtime
                                // selection must supply context without that authority.
                                step.injections.push((content, InjectionOrigin::Reminder));
                            }
                        }
                    }
                }
            }
        }
        next.run(step).await
    }
}
