use super::{fail, ok, Member, Members};
use crate::agent::{Agent, AgentId};
use crate::session::InjectionOrigin;
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

fn live_member<'a>(team: &'a BTreeMap<String, Member>, name: &str) -> Result<&'a Member, String> {
    team.get(name)
        .filter(|member| !member.stopping)
        .ok_or_else(|| format!("no live team member named `{name}`"))
}

pub(super) fn set_grant(
    members: &Members,
    lead: &Agent,
    from: &str,
    to: &str,
    allow: bool,
) -> Result<String, String> {
    if from.is_empty() || to.is_empty() || from == to {
        return Err("provide distinct member names in `name` and `to`".into());
    }
    let mut all = members.by_lead.lock().expect("members poisoned");
    let team = all
        .get_mut(lead.session_id())
        .ok_or("no live team members")?;
    let target = live_member(team, to)?.agent.id();
    live_member(team, from)?;
    let source = team.get_mut(from).expect("live sender under roster lock");
    let changed = if allow {
        source.peer_targets.insert(to.into(), target) != Some(target)
    } else {
        source.peer_targets.remove(to).is_some()
    };
    if changed {
        announce_grant(&source.agent, from, to, allow);
    }
    Ok(grant_description(from, to, allow))
}

fn grant_description(from: &str, to: &str, allow: bool) -> String {
    let state = if allow { "allowed" } else { "revoked" };
    format!("Team peer messages from `{from}` to `{to}` are {state}.")
}

pub(super) fn announce_grant(sender: &Agent, from: &str, to: &str, allow: bool) {
    sender.inject(
        format!(
            "{} The member tool for direct handoffs is `tell_sibling`.",
            grant_description(from, to, allow)
        ),
        InjectionOrigin::TeamNote { member: to.into() },
    );
}

pub(super) struct TellSibling {
    members: Weak<Members>,
    lead: Weak<Agent>,
    sender: AgentId,
    name: String,
}

impl TellSibling {
    pub(super) fn new(
        members: &Arc<Members>,
        lead: &Arc<Agent>,
        sender: AgentId,
        name: String,
    ) -> Self {
        Self {
            members: Arc::downgrade(members),
            lead: Arc::downgrade(lead),
            sender,
            name,
        }
    }

    fn deliver(&self, args: Args, ctx: &ToolContext) -> Result<&'static str, String> {
        let text = args.text.trim();
        if text.is_empty() {
            return Err("`text` is required".into());
        }
        let members = self.members.upgrade().ok_or("the team is gone")?;
        let lead = self.lead.upgrade().ok_or("the lead is gone")?;
        // Stop closes both endpoints under this same lock before cancelling a
        // turn. Keep validation and enqueue together so a stale send cannot race it.
        let all = members.by_lead.lock().expect("members poisoned");
        let team = all.get(lead.session_id()).ok_or("the team is gone")?;
        let sender = live_member(team, &self.name)?;
        if sender.agent.id() != self.sender {
            return Err("the sending member has been replaced".into());
        }
        let target = live_member(team, &args.to)?;
        if sender.peer_targets.get(&args.to) != Some(&target.agent.id()) {
            return Err(format!(
                "the lead has not authorized messages to `{}`",
                args.to
            ));
        }
        if ctx.cancel.is_cancelled() {
            return Err("peer message cancelled before enqueue".into());
        }

        let origin = sender.agent.team_message_origin();
        if args.wake {
            target.agent.send_from(text, origin);
        } else {
            let origin = origin
                .peer_fact(target.agent.ctx(), false)
                .expect("team messages have a peer origin");
            target
                .agent
                .inject(format!("[For your information]\n{text}"), origin);
        }
        lead.inject(
            format!(
                "`{}` queued {} for `{}`.",
                self.name,
                if args.wake { "a handoff" } else { "a note" },
                args.to
            ),
            InjectionOrigin::TeamNote {
                member: self.name.clone(),
            },
        );
        Ok("queued for the teammate")
    }
}

#[derive(Deserialize)]
struct Args {
    to: String,
    text: String,
    #[serde(default)]
    wake: bool,
}

#[async_trait]
impl Tool for TellSibling {
    fn name(&self) -> &str {
        "tell_sibling"
    }

    fn description(&self) -> &str {
        "Send a message to a teammate the lead explicitly authorized you to contact. Leave wake \
         off for information they can read in their next turn. Use wake only for a handoff \
         that needs action. A message is not permission to change scope or use other tools. \
         Report completion to the lead with tell_parent."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": { "type": "string", "description": "Authorized recipient's team member name" },
                "text": { "type": "string", "description": "The note or handoff to send" },
                "wake": { "type": "boolean", "description": "Request a turn for an actionable handoff; defaults to false" }
            },
            "required": ["to", "text"]
        })
    }

    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        match serde_json::from_str(args)
            .map_err(|e| e.to_string())
            .and_then(|args| self.deliver(args, ctx))
        {
            Ok(message) => ok(message),
            Err(error) => fail(error),
        }
    }
}
