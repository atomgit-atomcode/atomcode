//! A team: child agents the lead keeps around and talks to.
//!
//! The `task` row runs one child to completion and hands back a report. A team
//! member is created once, given a role, and stays: the lead delegates, tells
//! it more, waits on it, stops it. Every exchange is a message in an inbox and
//! a fact in both logs — the sender's, the receiver's — under
//! [`InjectionOrigin::Peer`], so a resumed session still shows who said what
//! to whom.
//!
//! Members answer the lead and only the lead. The `tell_parent` tool mounted
//! in a member's realm holds the lead's id and nothing else; there is no tool
//! that names a sibling. Sibling-to-sibling traffic goes through the lead,
//! which is what makes the lead a lead.
//!
//! A role fixes two things the model should not pick per call: which tools
//! (read-only or scoped writes, never bash) and which model tier (the utility
//! model for simple roles when one is mounted, the conversation's otherwise).
//! deepseek-harness and the old runtime both hide the tier behind the role
//! for the same reason: the model knows what the job is, not what it costs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::{Agent, AgentId, CreateAgent, MessageOrigin};
use crate::events::{SessionEventCommitted, TurnStopping};
use crate::seams::{AgentsSvc, LlmSvc, LlmUtilitySvc, SessionSvc, ToolBox, ToolsSvc};
use crate::session::{Committed, SessionEvent};

use super::agent_loop::{keep_driven, Driving};
use super::subagent::ChildRoundCap;
use super::tools::{contribute_prompt, mount};

// ---- roles ----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Permission {
    /// Read the tree; change nothing.
    Explore,
    /// Read, and write files. Never a shell.
    Worker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Difficulty {
    /// The utility model will do, when one is mounted.
    Simple,
    /// The conversation's model.
    Hard,
}

struct Role {
    id: &'static str,
    permission: Permission,
    difficulty: Difficulty,
    persona: &'static str,
    when: &'static str,
}

const ROLES: [Role; 5] = [
    Role {
        id: "explorer",
        permission: Permission::Explore,
        difficulty: Difficulty::Simple,
        persona: "You find things: code paths, call chains, where a symbol lives. You report \
                  locations with file and line, and you do not speculate.",
        when: "code search and call-chain discovery",
    },
    Role {
        id: "reviewer",
        permission: Permission::Explore,
        difficulty: Difficulty::Hard,
        persona: "You review code for defects and risks. You report each issue with the file, \
                  the line range and why it matters, and you change nothing.",
        when: "reviewing a change or a file for problems",
    },
    Role {
        id: "implementer",
        permission: Permission::Worker,
        difficulty: Difficulty::Hard,
        persona: "You implement what you are asked, in the files you are told about. You make \
                  the smallest change that does the job and report exactly what you changed.",
        when: "a self-contained change with a clear scope",
    },
    Role {
        id: "tester",
        permission: Permission::Worker,
        difficulty: Difficulty::Hard,
        persona: "You write and adjust tests. You report which tests you touched and what each \
                  one proves.",
        when: "adding or fixing tests for a change",
    },
    Role {
        id: "docs_writer",
        permission: Permission::Worker,
        difficulty: Difficulty::Simple,
        persona: "You write and edit documentation. You keep to the facts you were given and \
                  report which files you touched.",
        when: "documentation for something already decided",
    },
];

fn role(id: &str) -> Option<&'static Role> {
    ROLES.iter().find(|r| r.id == id)
}

const EXPLORE_TOOLS: &[&str] = &[
    "read_file",
    "list_directory",
    "grep",
    "glob",
    "list_symbols",
    "read_symbol",
];
const WORKER_TOOLS: &[&str] = &["edit_file", "write_file"];

fn tools_for(permission: Permission) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = EXPLORE_TOOLS.to_vec();
    if permission == Permission::Worker {
        names.extend(WORKER_TOOLS);
    }
    names
}

// ---- the members ----------------------------------------------------------

struct Member {
    role: &'static str,
    agent: Arc<Agent>,
    /// Whether this member said something to the lead during its current
    /// turn. A member that ends a turn silently is reported on by the team,
    /// so the lead is never left waiting on a member that forgot to speak.
    told: Arc<Mutex<bool>>,
    _driving: Driving,
}

/// Members, by lead session and then by name.
#[derive(Default)]
struct Members {
    by_lead: Mutex<HashMap<String, BTreeMap<String, Member>>>,
    /// Member session id → (lead session id, member name), for the finish
    /// listener, which sees facts by session.
    leads: Mutex<HashMap<String, (String, String)>>,
}

/// The member's one way of talking: to the lead, and only the lead.
struct TellParent {
    agents: Arc<crate::agent::Agents>,
    lead: AgentId,
    me: AgentId,
    name: String,
    told: Arc<Mutex<bool>>,
}

#[async_trait]
impl Tool for TellParent {
    fn name(&self) -> &str {
        "tell_parent"
    }
    fn description(&self) -> &str {
        "Send a message to the lead agent that delegated to you: a progress note, a question, \
         or your final report. The lead is the only agent you can address."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "What to tell the lead" }
            },
            "required": ["text"]
        })
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        #[derive(Deserialize)]
        struct Args {
            text: String,
        }
        let text = match serde_json::from_str::<Args>(args) {
            Ok(a) => a.text,
            Err(e) => return fail(format!("invalid arguments: {e}")),
        };
        let Some(lead) = self.agents.get(self.lead) else {
            return fail("the lead agent is gone");
        };
        *self.told.lock().expect("told poisoned") = true;
        lead.send_from(
            format!("[{}] {text}", self.name),
            MessageOrigin::Peer(self.me),
        );
        ok("delivered to the lead")
    }
}

fn ok(text: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: text.into(),
        is_error: false,
        images: vec![],
    }
}

fn fail(text: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: text.into(),
        is_error: true,
        images: vec![],
    }
}

// ---- the lead's tool --------------------------------------------------------

struct TeamTool {
    ctx: Context,
    members: Arc<Members>,
    max_members: usize,
    max_rounds: u32,
}

#[derive(Deserialize)]
struct TeamArgs {
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl TeamTool {
    /// The agent whose turn is calling: the lead.
    fn lead(&self) -> Result<Arc<Agent>, String> {
        let current = crate::agent::current().ok_or("`team` must be called from a running agent")?;
        let log = current
            .service::<SessionSvc>()
            .ok_or("the calling agent has no session")?;
        let agents = self.ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
        agents
            .by_session(log.id())
            .ok_or_else(|| "the calling agent is not registered".to_string())
    }

    async fn delegate(&self, lead: &Arc<Agent>, args: TeamArgs) -> Result<String, String> {
        let name = args.name.filter(|n| !n.trim().is_empty()).ok_or("`name` is required")?;
        let role_id = args.role.ok_or("`role` is required")?;
        let role = role(&role_id).ok_or_else(|| {
            format!(
                "unknown role `{role_id}`; roles: {}",
                ROLES.iter().map(|r| r.id).collect::<Vec<_>>().join(", ")
            )
        })?;
        let task = args.task.filter(|t| !t.trim().is_empty()).ok_or("`task` is required")?;
        let lead_session = lead.session_id().to_string();
        {
            let all = self.members.by_lead.lock().expect("members poisoned");
            let mine = all.get(&lead_session);
            if mine.is_some_and(|m| m.contains_key(&name)) {
                return Err(format!("a member named `{name}` already exists; `tell` it instead"));
            }
            if mine.map(|m| m.len()).unwrap_or(0) >= self.max_members {
                return Err(format!("the team is full ({} members)", self.max_members));
            }
        }

        let agents = self.ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
        let parent_tools = lead
            .ctx()
            .service::<ToolsSvc>()
            .ok_or("the lead has no tool catalog")?;
        let restricted = Arc::new(ToolBox::new());
        for tool_name in tools_for(role.permission) {
            if let Some(tool) = parent_tools.get(tool_name) {
                restricted.register(tool)?;
            }
        }
        let told = Arc::new(Mutex::new(false));
        let prompts = Arc::new(crate::seams::PromptRegistry::new());
        prompts.contribute(
            "team-member",
            0,
            format!(
                "{}\n\nYou are `{name}`, a {} on a team. The lead delegated this to you. Use \
                 `tell_parent` for questions and progress, and call it with a compact report \
                 when you are done — the lead sees nothing else you do.",
                role.persona, role.id
            ),
        );
        let utility = match role.difficulty {
            Difficulty::Simple => self.ctx.service::<LlmUtilitySvc>(),
            Difficulty::Hard => None,
        };
        let member_id = format!("{lead_session}/{name}");
        let lead_id = lead.id();
        let member_name = name.clone();
        let told_for_tool = told.clone();
        let agents_for_tool = agents.clone();
        let max_rounds = self.max_rounds;
        let tools_for_realm = restricted.clone();
        let child = agents
            .create(
                &self.ctx,
                CreateAgent::new()
                    .id(member_id)
                    .parent(lead_session.clone())
                    .persist(false)
                    .setup(Box::new(move |realm: &Context| {
                        let mut held = Vec::new();
                        held.push(
                            realm
                                .provide::<ToolsSvc>(tools_for_realm)
                                .map_err(|e| e.to_string())?,
                        );
                        held.push(
                            realm
                                .provide::<crate::seams::SystemPromptSvc>(prompts.clone())
                                .map_err(|e| e.to_string())?,
                        );
                        if let Some(model) = utility.clone() {
                            held.push(realm.provide::<LlmSvc>(model).map_err(|e| e.to_string())?);
                        }
                        held.push(realm.on_serial::<TurnStopping>(Arc::new(ChildRoundCap {
                            max_steps: max_rounds,
                        })));
                        Ok(held)
                    })),
            )
            .await?;
        // The tool needs the member's registry id, which exists only now.
        restricted.register(Arc::new(TellParent {
            agents: agents_for_tool,
            lead: lead_id,
            me: child.id(),
            name: member_name,
            told: told_for_tool,
        }))?;
        let driving = keep_driven(child.clone())?;
        self.members
            .leads
            .lock()
            .expect("leads poisoned")
            .insert(child.session_id().to_string(), (lead_session.clone(), name.clone()));
        self.members
            .by_lead
            .lock()
            .expect("members poisoned")
            .entry(lead_session)
            .or_default()
            .insert(
                name.clone(),
                Member {
                    role: role.id,
                    agent: child.clone(),
                    told,
                    _driving: driving,
                },
            );
        child.send_from(task, MessageOrigin::Peer(lead_id));
        Ok(format!(
            "delegated to `{name}` ({}). It will report through `tell_parent`; use `wait` to \
             block on it or `status` to look.",
            role.id
        ))
    }

    fn with_member<R>(
        &self,
        lead: &Arc<Agent>,
        name: &str,
        f: impl FnOnce(&Member) -> R,
    ) -> Result<R, String> {
        let all = self.members.by_lead.lock().expect("members poisoned");
        all.get(lead.session_id())
            .and_then(|m| m.get(name))
            .map(f)
            .ok_or_else(|| format!("no member named `{name}`"))
    }

    fn status(&self, lead: &Arc<Agent>) -> String {
        let all = self.members.by_lead.lock().expect("members poisoned");
        let Some(mine) = all.get(lead.session_id()).filter(|m| !m.is_empty()) else {
            return "no members".into();
        };
        mine.iter()
            .map(|(name, m)| {
                let log = m.agent.session();
                format!(
                    "{name} ({}): {:?}, turn {}, {} event(s){}",
                    m.role,
                    m.agent.status(),
                    log.current_turn(),
                    log.len(),
                    if m.agent.inbox().has_waking_input() {
                        ", work queued"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn wait(&self, lead: &Arc<Agent>, name: &str, timeout: Duration) -> Result<String, String> {
        let agent = self.with_member(lead, name, |m| m.agent.clone())?;
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let idle = agent.status() == crate::agent::AgentStatus::Idle
                && !agent.inbox().has_waking_input();
            if idle && agent.session().current_turn() > 0 {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(format!("`{name}` is still working after {}s", timeout.as_secs()));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(last_said(&agent.session()).unwrap_or_else(|| format!("`{name}` finished without saying anything")))
    }

    fn stop(&self, lead: &Arc<Agent>, name: Option<&str>) -> Result<String, String> {
        let agents = self.ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
        let mut all = self.members.by_lead.lock().expect("members poisoned");
        let Some(mine) = all.get_mut(lead.session_id()) else {
            return Ok("no members".into());
        };
        let names: Vec<String> = match name {
            Some(n) => vec![n.to_string()],
            None => mine.keys().cloned().collect(),
        };
        let mut stopped = Vec::new();
        for n in names {
            let Some(member) = mine.remove(&n) else {
                return Err(format!("no member named `{n}`"));
            };
            member.agent.cancel();
            self.members
                .leads
                .lock()
                .expect("leads poisoned")
                .remove(member.agent.session_id());
            agents.remove(member.agent.id());
            stopped.push(n);
        }
        Ok(format!("stopped: {}", stopped.join(", ")))
    }
}

/// The member's last words, from its own log.
fn last_said(log: &crate::session::SessionLog) -> Option<String> {
    log.events().into_iter().rev().find_map(|e| match e.event {
        SessionEvent::AssistantMessage { text, .. } if !text.trim().is_empty() => Some(text),
        _ => None,
    })
}

#[async_trait]
impl Tool for TeamTool {
    fn name(&self) -> &str {
        "team"
    }
    fn description(&self) -> &str {
        "Run a team of child agents that stay around. `delegate` creates a named member with a \
         role and a task; `tell` sends it more; `status` lists members; `wait` blocks until one \
         is idle and returns its last report; `stop` ends one or all. Members report back \
         through messages you receive between turns."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["delegate", "tell", "status", "wait", "stop"] },
                "name": { "type": "string", "description": "The member's name (delegate, tell, wait, stop)" },
                "role": {
                    "type": "string",
                    "enum": ROLES.iter().map(|r| r.id).collect::<Vec<_>>(),
                    "description": ROLES.iter().map(|r| format!("{}: {}", r.id, r.when)).collect::<Vec<_>>().join("; ")
                },
                "task": { "type": "string", "description": "The complete task (delegate)" },
                "text": { "type": "string", "description": "What to tell the member (tell)" },
                "timeout_secs": { "type": "integer", "description": "How long `wait` may block; default 120" }
            },
            "required": ["action"]
        })
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        "team".into()
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let args: TeamArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return fail(format!("invalid arguments: {e}")),
        };
        let lead = match self.lead() {
            Ok(l) => l,
            Err(e) => return fail(e),
        };
        let result = match args.action.as_str() {
            "delegate" => self.delegate(&lead, args).await,
            "tell" => {
                let name = args.name.as_deref().unwrap_or_default();
                let text = args.text.clone().unwrap_or_default();
                if text.trim().is_empty() {
                    Err("`text` is required".to_string())
                } else {
                    self.with_member(&lead, name, |m| {
                        *m.told.lock().expect("told poisoned") = false;
                        m.agent.send_from(text, MessageOrigin::Peer(lead.id()));
                    })
                    .map(|_| format!("told `{name}`"))
                }
            }
            "status" => Ok(self.status(&lead)),
            "wait" => {
                let name = args.name.clone().unwrap_or_default();
                self.wait(
                    &lead,
                    &name,
                    Duration::from_secs(args.timeout_secs.unwrap_or(120)),
                )
                .await
            }
            "stop" => self.stop(&lead, args.name.as_deref()),
            other => Err(format!("unknown action `{other}`")),
        };
        match result {
            Ok(text) => ok(text),
            Err(e) => fail(e),
        }
    }
}

// ---- the row ----------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TeamRow {
    #[serde(default = "default_members")]
    max_members: usize,
    #[serde(default = "default_rounds")]
    max_rounds: u32,
}

fn default_members() -> usize {
    6
}

fn default_rounds() -> u32 {
    24
}

impl Default for TeamRow {
    fn default() -> Self {
        Self {
            max_members: default_members(),
            max_rounds: default_rounds(),
        }
    }
}

pub struct TeamPlugin;

#[async_trait]
impl Plugin for TeamPlugin {
    fn name(&self) -> &'static str {
        "team-in-process"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "agents", "agent-loop"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["llm-utility", "system-prompt"]
    }
    fn description(&self) -> &'static str {
        "the `team` tool: named child agents with roles that stay, report back, and can be told more"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: TeamRow = if config.is_null() {
            TeamRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let members = Arc::new(Members::default());
        mount(
            ctx,
            vec![Arc::new(TeamTool {
                ctx: ctx.clone(),
                members: members.clone(),
                max_members: row.max_members,
                max_rounds: row.max_rounds,
            }) as Arc<dyn Tool>],
        )?;

        // A member that ends a turn without having spoken is reported on, so
        // the lead learns it finished. Facts are seen by session here — this
        // listener is above every member — and routed to the lead by name.
        let finish_ctx = ctx.clone();
        let finish_members = members.clone();
        let _ = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            let SessionEvent::TurnEnd { turn, stop, .. } = &committed.event else {
                return;
            };
            let Some((lead_session, name)) = finish_members
                .leads
                .lock()
                .expect("leads poisoned")
                .get(&committed.session)
                .cloned()
            else {
                return;
            };
            let Some(agents) = finish_ctx.service::<AgentsSvc>() else {
                return;
            };
            let (member, told) = {
                let all = finish_members.by_lead.lock().expect("members poisoned");
                let Some(m) = all.get(&lead_session).and_then(|m| m.get(&name)) else {
                    return;
                };
                let told = std::mem::replace(&mut *m.told.lock().expect("told poisoned"), false);
                (m.agent.clone(), told)
            };
            if told {
                return;
            }
            let Some(lead) = agents.by_session(&lead_session) else {
                return;
            };
            let said = last_said(&member.session()).unwrap_or_else(|| "(nothing)".into());
            lead.send_from(
                format!("[{name} finished turn {turn}: {stop:?}]\n{said}"),
                MessageOrigin::Peer(member.id()),
            );
        });

        contribute_prompt(
            ctx,
            "team",
            57,
            &format!(
                "`team` runs named child agents that stay around: delegate with a role ({}), \
                 tell them more, wait on them, stop them. They report to you through messages \
                 marked `[message from …]`; act on those.",
                ROLES
                    .iter()
                    .map(|r| format!("{} — {}", r.id, r.when))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        );
        Ok(())
    }
}

#[allow(dead_code)]
fn _roles_are_unique() {
    let mut seen = HashSet::new();
    for r in ROLES.iter() {
        assert!(seen.insert(r.id));
    }
}
