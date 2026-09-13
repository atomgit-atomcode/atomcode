//! Subagents: the use case both composability axes exist for.
//!
//! A subagent is a second agent in the same process with **its own world** —
//! its own tool catalog, potentially its own filesystem, its own policy — and
//! it must not be able to escape the constraints its parent runs under. That is
//! exactly the pair of properties realms provide:
//!
//! - **spatially**, the child mounts into a realm layered over the parent's, so
//!   the tools and listeners it installs are invisible to the parent and to its
//!   siblings;
//! - **temporally**, the child's whole footprint is one fiber, so finishing (or
//!   failing) reverts every registration it made, with nothing to clean up by
//!   hand.
//!
//! The escape-proofing is not a check this module performs — it falls out of
//! realm visibility running one way. A credential gate or an approval policy
//! installed at the root is still in the child's chain; nothing the child
//! installs reaches back up.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::events::{TurnProgress, TurnStopping};
use crate::seams::{
    AgentLoopSvc, AgentsSvc, SessionSvc, StopReason, SubagentOutcome, Subagents, SubagentsSvc,
    SystemPromptSvc, ToolBox, ToolsSvc,
};

use super::tools::{contribute_prompt, mount};

/// Ends a delegated turn after its own budget, independent of the parent's.
pub(crate) struct ChildRoundCap {
    pub(crate) max_steps: u32,
}

#[async_trait]
impl atomcode_plexus::Listener<TurnStopping> for ChildRoundCap {
    async fn call(&self, progress: &TurnProgress) -> Option<StopReason> {
        (progress.rounds >= self.max_steps).then_some(StopReason::MaxRounds)
    }
}

/// Runs a child agent in an isolated realm of the same process.
struct InProcessSubagents {
    ctx: Context,
    /// Tools a child may use. A subset of the parent's catalog, resolved by
    /// name at spawn time so a tool unmounted since is simply not offered.
    allowed_tools: Vec<String>,
    max_rounds: u32,
}

#[async_trait]
impl Subagents for InProcessSubagents {
    fn describe(&self) -> String {
        format!(
            "in-process, isolated realm, tools: {}",
            self.allowed_tools.join(", ")
        )
    }

    async fn spawn(&self, task: &str, instructions: &str) -> SubagentOutcome {
        let (Some(agents), Some(driver), Some(parent_tools)) = (
            self.ctx.service::<AgentsSvc>(),
            self.ctx.service::<AgentLoopSvc>(),
            self.ctx.service::<ToolsSvc>(),
        ) else {
            return SubagentOutcome::failed("subagents need `agents`, `agent-loop` and `tools`");
        };

        // A real agent, in a realm of its own, visible in the registry — a UI
        // watching delegated work sees it the same way it sees any other agent.
        let restricted = Arc::new(ToolBox::new());
        for name in &self.allowed_tools {
            if let Some(tool) = parent_tools.get(name) {
                if let Err(e) = restricted.register(tool) {
                    return SubagentOutcome::failed(e);
                }
            }
        }
        let prompts = Arc::new(crate::seams::PromptRegistry::new());
        prompts.contribute("subagent", 0, instructions);

        // Its own conversation, its own tools and its own prompt, composed
        // before anyone can see it. The parent's session is the one whose turn
        // this tool call is running in. Not persisted: a delegated child's
        // transcript is the parent's business, not a session of its own.
        let parent = crate::agent::current()
            .and_then(|c| c.service::<SessionSvc>())
            .map(|log| log.id().to_string());
        let mut req = crate::agent::CreateAgent::new()
            .id(format!("sub-{}", crate::agent::mint_session_id()))
            .persist(false)
            .setup(Box::new(move |realm: &Context| {
                Ok(vec![
                    realm
                        .provide::<ToolsSvc>(restricted)
                        .map_err(|e| e.to_string())?,
                    realm
                        .provide::<SystemPromptSvc>(prompts)
                        .map_err(|e| e.to_string())?,
                ])
            }));
        if let Some(parent) = parent {
            req = req.parent(parent);
        }
        let child = match agents.create(&self.ctx, req).await {
            Ok(child) => child,
            Err(e) => return SubagentOutcome::failed(e),
        };
        let log = child.session();

        let round_cap = child
            .ctx()
            .on_serial::<TurnStopping>(Arc::new(ChildRoundCap {
                max_steps: self.max_rounds,
            }));

        child.send(task);
        let outcome = driver.drive(&child).await;

        round_cap.dispose();
        // Removing the agent tears down what was mounted for it alone.
        agents.remove(child.id());

        SubagentOutcome {
            text: outcome.text,
            rounds: outcome.steps,
            tool_calls: outcome.tool_calls,
            stop: outcome.stop,
            error: outcome.error,
            transcript_len: log.len(),
        }
    }
}

// ---- the model-facing tool ----------------------------------------------

#[derive(Deserialize)]
struct TaskArgs {
    /// What the child should accomplish.
    task: String,
    /// Optional extra standing instructions for the child.
    #[serde(default)]
    instructions: Option<String>,
}

const DEFAULT_INSTRUCTIONS: &str = "\
You are a subagent handling one delegated task. You have a reduced tool set and no \
ability to change the parent's state. Investigate, do the work the task describes, and \
finish with a compact report of what you found or changed. Do not ask questions — you \
have no one to ask.";

struct TaskTool {
    ctx: Context,
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "Delegate a self-contained task to a subagent with its own reduced tool set. Use it \
         for work that would otherwise flood this conversation with intermediate output — a \
         broad search, a survey of unfamiliar code. The subagent cannot ask you questions, so \
         state the task completely."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": { "type": "string", "description": "The complete task, stated so it needs no follow-up" },
                "instructions": { "type": "string", "description": "Extra standing instructions for the subagent" }
            },
            "required": ["task"]
        })
    }
    /// The child runs under the same root policy, but it does run real tools,
    /// so the call itself is a decision worth gating.
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        "task".into()
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let args: TaskArgs = match serde_json::from_str(args) {
            Ok(args) => args,
            Err(e) => {
                return ToolResult {
                    call_id: String::new(),
                    content: format!("invalid arguments: {e}"),
                    is_error: true,
                    images: vec![],
                }
            }
        };
        let Some(subagents) = self.ctx.service::<SubagentsSvc>() else {
            return ToolResult {
                call_id: String::new(),
                content: "no `subagents` provider is mounted".into(),
                is_error: true,
                images: vec![],
            };
        };
        let instructions = args.instructions.as_deref().unwrap_or(DEFAULT_INSTRUCTIONS);
        let outcome = subagents.spawn(&args.task, instructions).await;
        ToolResult {
            call_id: String::new(),
            content: outcome.report(),
            is_error: outcome.error.is_some(),
            images: vec![],
        }
    }
}

#[derive(Debug, Deserialize)]
struct SubagentRow {
    /// Tools a child may use. Deliberately explicit rather than "everything
    /// minus a deny list": a tool added later should not silently widen what
    /// delegated work can do.
    #[serde(default = "default_tools")]
    allowed_tools: Vec<String>,
    #[serde(default = "default_rounds")]
    max_rounds: u32,
}

impl Default for SubagentRow {
    fn default() -> Self {
        Self {
            allowed_tools: default_tools(),
            max_rounds: default_rounds(),
        }
    }
}

fn default_tools() -> Vec<String> {
    [
        "read_file",
        "list_directory",
        "grep",
        "glob",
        "list_symbols",
        "read_symbol",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn default_rounds() -> u32 {
    12
}

pub struct SubagentPlugin;

#[async_trait]
impl Plugin for SubagentPlugin {
    fn name(&self) -> &'static str {
        "subagent-in-process"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "llm", "agents"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Read at spawn time to derive the child's world from the parent's.
        &["subagents", "agent-loop", "system-prompt"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["subagents"]
    }
    fn description(&self) -> &'static str {
        "run a child agent in an isolated realm, with a reduced tool set"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: SubagentRow = if config.is_null() {
            SubagentRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let _ = ctx
            .provide::<SubagentsSvc>(Arc::new(InProcessSubagents {
                ctx: ctx.clone(),
                allowed_tools: row.allowed_tools.clone(),
                max_rounds: row.max_rounds,
            }))
            .map_err(|e| e.to_string())?;
        mount(
            ctx,
            vec![Arc::new(TaskTool { ctx: ctx.clone() }) as Arc<dyn Tool>],
        )?;
        contribute_prompt(
            ctx,
            "subagent",
            57,
            &format!(
                "`task` delegates a self-contained job to a subagent with a reduced tool set \
                 ({}). Use it for work whose intermediate output you do not need to see.",
                row.allowed_tools.join(", ")
            ),
        );
        Ok(())
    }
}
