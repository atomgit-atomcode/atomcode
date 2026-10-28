//! Skill preauthorization. Ephemeral, per agent and turn; never replayed from
//! a session log. This row uses the same matcher as static permission rules.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_capabilities::skills::Skill;
use atomcode_capabilities::tools::{PermissionRules, RuleDecision, SensitivePaths};
use atomcode_kernel::tool::ToolResult;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde_json::Value;

use crate::events::{
    AgentRemoved, Authorization, PreStep, SessionEventCommitted, StepDecision, ToolExec,
    ToolsExecute,
};
use crate::seams::{SessionSvc, SkillsSvc};
use crate::session::{Committed, InjectionOrigin, SessionEvent};

pub(crate) fn rules(skill: &Skill) -> Result<PermissionRules, String> {
    let (rules, invalid) = PermissionRules::parse(&skill.allowed_tools, &[]);
    if !invalid.is_empty() {
        return Err(format!(
            "skill '{}' has invalid allowed-tools: {}",
            skill.name,
            invalid.join(", ")
        ));
    }
    Ok(rules)
}

#[derive(Default)]
struct Grants {
    turn: u64,
    epoch: u64,
    rules: Vec<PermissionRules>,
}

struct SkillPermissions {
    ctx: Context,
    sensitive: SensitivePaths,
    grants: Mutex<HashMap<String, Grants>>,
}

impl SkillPermissions {
    fn may_preauthorize(&self, exec: &ToolExec) -> bool {
        if atomcode_capabilities::tools::powershell::is_powershell_tool(&exec.call.name) {
            return false;
        }
        if matches!(exec.call.name.as_str(), "bash" | "bash_start") {
            if let Ok(args) = serde_json::from_str::<Value>(&exec.call.arguments) {
                if args
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| {
                        atomcode_capabilities::tools::bash::check_destructive_command(command)
                            .is_some()
                    })
                {
                    return false;
                }
            }
        }
        true
    }
    fn session(&self) -> Option<Arc<crate::session::SessionLog>> {
        // No calling agent means no temporary authority, even if the root has
        // a session service. The existing task-local also covers parallel tools.
        crate::agent::current()?.service::<SessionSvc>()
    }

    fn committed(&self, fact: &Committed) {
        let mut all = self.grants.lock().expect("skill grants poisoned");
        match &fact.event {
            SessionEvent::TurnStart { turn } => {
                all.insert(
                    fact.session.clone(),
                    Grants {
                        turn: *turn,
                        ..Default::default()
                    },
                );
            }
            SessionEvent::TurnEnd { .. } => {
                all.remove(&fact.session);
            }
            SessionEvent::Injected {
                turn,
                origin: InjectionOrigin::Skill { name },
                ..
            } => {
                let Some(grants) = all.get_mut(&fact.session).filter(|g| g.turn == *turn) else {
                    return;
                };
                if let Some(skills) = self.ctx.service::<SkillsSvc>() {
                    for name in name.split(" · ") {
                        let skill = if let Some(name) = name.strip_prefix("[command] ") {
                            skills
                                .slash_commands()
                                .into_iter()
                                .find(|command| command.name == name)
                        } else {
                            skills.get(name)
                        };
                        if let Some(skill) = skill {
                            if let Ok(rules) = rules(&skill) {
                                grants.rules.push(rules);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

#[async_trait]
impl Waterfall<PreStep> for SkillPermissions {
    async fn handle(&self, step: &mut StepDecision, next: Next<'_, PreStep>) -> StepDecision {
        // Revoke before processing a new human message, including a steer.
        // Skill injections for this input are committed afterward and can grant
        // fresh authority. A rejected input must not keep the previous grant.
        if step.message.is_some() && matches!(step.origin, crate::agent::MessageOrigin::User) {
            if let Some(session) = self.session() {
                if let Some(grants) = self
                    .grants
                    .lock()
                    .expect("skill grants poisoned")
                    .get_mut(session.id())
                {
                    grants.epoch += 1;
                    grants.rules.clear();
                }
            }
        }
        next.run(step).await
    }
}

#[async_trait]
impl Waterfall<ToolsExecute> for SkillPermissions {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        let session = self.session();
        let epoch = session.as_ref().and_then(|s| {
            let all = self.grants.lock().expect("skill grants poisoned");
            let grants = all.get(s.id()).filter(|g| g.turn == exec.turn)?;
            if self.may_preauthorize(exec)
                && grants.rules.iter().any(|rules| {
                    rules.decide(
                        &self.sensitive,
                        &exec.call.name,
                        &exec.call.arguments,
                        &exec.working_dir,
                    ) == RuleDecision::Allow
                })
            {
                // Do not downgrade explicit human consent from an upstream gate.
                if !exec.authorization.settled() {
                    exec.authorization = Authorization::Presumed;
                }
            }
            Some(grants.epoch)
        });

        // Activate only a real, successful invocation. Validate before running
        // expansion (which can include trusted shell snippets).
        let invoked = if exec.call.name == "use_skill" {
            serde_json::from_str::<Value>(&exec.call.arguments)
                .ok()
                .and_then(|args| args.get("name").and_then(Value::as_str).map(str::to_owned))
                .and_then(|name| {
                    self.ctx
                        .service::<SkillsSvc>()
                        .and_then(|skills| skills.get(&name))
                })
        } else {
            None
        };
        let invoked = match invoked.as_ref().map(|skill| rules(skill)).transpose() {
            Ok(rules) => rules,
            Err(error) => return crate::exec::error_result(&exec.call.id, &error),
        };
        let result = next.run(exec).await;
        if !result.is_error {
            if let (Some(session), Some(epoch), Some(rules)) = (session, epoch, invoked) {
                if let Some(grants) = self
                    .grants
                    .lock()
                    .expect("skill grants poisoned")
                    .get_mut(session.id())
                    .filter(|g| g.turn == exec.turn && g.epoch == epoch)
                {
                    grants.rules.push(rules);
                }
            }
        }
        result
    }
}

pub struct SkillPermissionsPlugin;

#[async_trait]
impl Plugin for SkillPermissionsPlugin {
    fn name(&self) -> &'static str {
        "skill-permissions"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["product-dirs"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // SessionSvc is provided by the calling agent's realm, not a row at
        // mount time (AGENT_PROVIDED); a tree with no agent grants nothing.
        &["skills", "operations"]
    }
    fn description(&self) -> &'static str {
        "turn-scoped skill tool preauthorization"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let policy = Arc::new(SkillPermissions {
            ctx: ctx.clone(),
            sensitive: SensitivePaths::of(&*crate::product_dirs(ctx)?),
            grants: Mutex::new(HashMap::new()),
        });
        let events = policy.clone();
        let _ =
            ctx.on_emit::<SessionEventCommitted>(move |fact: &Committed| events.committed(fact));
        let removed = policy.clone();
        let _ = ctx.on_emit::<AgentRemoved>(move |agent: &crate::events::AgentChange| {
            removed
                .grants
                .lock()
                .expect("skill grants poisoned")
                .remove(&agent.session);
        });
        let _ = ctx.on_waterfall::<PreStep>(policy.clone(), false);
        let _ = ctx.on_waterfall::<ToolsExecute>(policy, true);
        super::self_knowledge::describes(ctx, "skill-permissions", 14,
            "SKILL PERMISSIONS — `allowed-tools` in SKILL.md accepts tool names and permission patterns such as `Bash(pip3 *)`, as a list or comma/space-separated string. A successful skill invocation preauthorizes matching tools for this turn, until your next message; unlisted tools keep normal approval. Static deny rules and security boundaries still apply; destructive shell commands retain normal approval. Grants are not restored on resume. This does not sandbox shell commands or plugin hooks.");
        Ok(())
    }
}
