//! Policy and product rows that were flags, strings or middleware entries in the
//! builder-assembled agent.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::{PermissionRules, RuleDecision, TodoTool};
use atomcode_kernel::tool::{RiskLevel, Tool, ToolResult};
use atomcode_plexus::{Context, Listener, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{
    ToolExec, ToolsExecute, TurnEnd, TurnProgress, TurnStart, TurnStarted, TurnStopping,
};
use crate::seams::{
    Decision, SessionSvc, StopReason, ToolsSvc, TurnOutcome,
    UserQuestions, UserQuestionsSvc,
};

use super::tools::{contribute_prompt, mount};

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}

// ---- todo ---------------------------------------------------------------

pub struct TodoPlugin;

#[async_trait]
impl Plugin for TodoPlugin {
    fn name(&self) -> &'static str {
        "tool-todo"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "the model-facing task list"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        mount(ctx, vec![Arc::new(TodoTool::new()) as Arc<dyn Tool>])?;
        contribute_prompt(
            ctx,
            "tool-todo",
            53,
            "Use `todowrite` to plan multi-step work: add one todo per concrete step before you \
             start, and mark each completed as soon as it is done.",
        );
        Ok(())
    }
}

// ---- permission rules ---------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct PermissionsRow {
    /// Claude-Code-style rules, e.g. `Bash(git *)`, `write_file(src/**)`.
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    /// Root that relative path rules resolve against.
    #[serde(default)]
    root: Option<String>,
}

/// Declarative allow/deny rules, ahead of the approval prompt.
///
/// It runs *after* argument repair and *before* `approval`, because a rule must
/// match the bytes that will execute and must be able to settle a decision the
/// interactive gate would otherwise ask about.
struct PermissionGate {
    rules: PermissionRules,
    root: PathBuf,
}

#[async_trait]
impl Waterfall<ToolsExecute> for PermissionGate {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        match self
            .rules
            .decide(&exec.call.name, &exec.call.arguments, &self.root)
        {
            RuleDecision::Deny => ToolResult {
                call_id: exec.call.id.clone(),
                content: format!(
                    "Refused: `{}` is denied by a `[permissions] deny` rule",
                    exec.call.name
                ),
                is_error: true,
                images: vec![],
            },
            // An explicit allow settles the authorization question without
            // ending the chain: the result transformers below still run.
            RuleDecision::Allow => {
                exec.pre_approved = true;
                next.run(exec).await
            }
            RuleDecision::NoMatch => next.run(exec).await,
        }
    }
}

pub struct PermissionsPlugin;

#[async_trait]
impl Plugin for PermissionsPlugin {
    fn name(&self) -> &'static str {
        "permissions"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "declarative allow/deny rules over tool calls"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: PermissionsRow = parse(config)?;
        let (rules, bad) = PermissionRules::parse(&row.allow, &row.deny);
        if !bad.is_empty() {
            // A rule that does not parse is a rule that silently permits: fail
            // the row rather than run with a gap the user thinks is closed.
            return Err(format!("unparseable permission rules: {}", bad.join(", ")));
        }
        if rules.is_empty() {
            return Ok(());
        }
        let root = row
            .root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let _ = ctx.on_waterfall::<ToolsExecute>(Arc::new(PermissionGate { rules, root }), true);
        Ok(())
    }
}

// ---- plan mode ----------------------------------------------------------

/// Read-only exploration: every tool that can change something is refused, and
/// the model is told to produce a plan instead.
///
/// Note it does not need a `fs-readonly` world — it gates on the tools' own
/// declared read-only hint, so it covers MCP tools and anything else mounted
/// later without knowing they exist.
struct PlanModeGate {
    ctx: Context,
}

#[async_trait]
impl Waterfall<ToolsExecute> for PlanModeGate {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        let read_only = self
            .ctx
            .service::<ToolsSvc>()
            .and_then(|t| t.get(&exec.call.name))
            .map(|tool| tool.read_only_hint())
            .unwrap_or(false);
        if read_only {
            return next.run(exec).await;
        }
        ToolResult {
            call_id: exec.call.id.clone(),
            content: format!(
                "Refused: plan mode is active, so `{}` cannot run. Finish investigating with \
                 read-only tools and present a plan for approval.",
                exec.call.name
            ),
            is_error: true,
            images: vec![],
        }
    }
}

pub struct PlanModePlugin;

#[async_trait]
impl Plugin for PlanModePlugin {
    fn name(&self) -> &'static str {
        "plan-mode"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "read-only exploration: refuse every tool that can change something"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx.on_waterfall::<ToolsExecute>(Arc::new(PlanModeGate { ctx: ctx.clone() }), true);
        // Ahead of the persona: plan mode overrides the agent's normal
        // instructions, so it must be the first thing the model reads.
        contribute_prompt(
            ctx,
            "plan-mode",
            -100,
            "PLAN MODE IS ACTIVE. Investigate with read-only tools only, then present a concrete \
             plan and stop. Do not edit files, run mutating commands, or ask to proceed — the \
             user will decide.",
        );
        Ok(())
    }
}

// ---- session title ------------------------------------------------------

// ---- user questions -----------------------------------------------------

/// The unattended answer: refuse.
///
/// A seam with a fail-closed default rather than an optional one — an
/// automation that silently self-approves is the failure this prevents.
struct UnattendedQuestions;

#[async_trait]
impl UserQuestions for UnattendedQuestions {
    fn describe(&self) -> String {
        "unattended (every question is declined)".into()
    }
    async fn ask(&self, _question: &str, _options: &[String]) -> Option<String> {
        None
    }
}

pub struct UnattendedQuestionsPlugin;

#[async_trait]
impl Plugin for UnattendedQuestionsPlugin {
    fn name(&self) -> &'static str {
        "user-questions-unattended"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["user-questions"]
    }
    fn description(&self) -> &'static str {
        "decline every question — the safe default with no human present"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UserQuestionsSvc>(Arc::new(UnattendedQuestions))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Approval that asks a human through the `user-questions` seam.
///
/// This is the row that turns the harness from unattended into interactive, and
/// it is the same approval seam — the policy just has somewhere to ask.
pub struct InteractiveApprovalPlugin;

struct AskingPolicy {
    ctx: Context,
}

#[async_trait]
impl crate::seams::ApprovalPolicy for AskingPolicy {
    async fn decide(
        &self,
        call: &atomcode_kernel::tool::ToolCall,
        tool: &Arc<dyn Tool>,
    ) -> Decision {
        if matches!(tool.risk(&call.arguments), RiskLevel::Safe) {
            return Decision::Allow;
        }
        let Some(questions) = self.ctx.service::<UserQuestionsSvc>() else {
            return Decision::Deny("no way to ask for approval".into());
        };
        // The tool's own name, not the call's: a gate that presents a safe
        // tool as risky says why in the name, and the person should see it.
        let question = format!(
            "Allow `{}` to run with these arguments?\n{}",
            tool.name(),
            call.arguments
        );
        match questions
            .ask(&question, &["yes".to_string(), "no".to_string()])
            .await
            .as_deref()
        {
            Some("yes") => Decision::Allow,
            _ => Decision::Deny("the user declined".into()),
        }
    }
}

#[async_trait]
impl Plugin for InteractiveApprovalPlugin {
    fn name(&self) -> &'static str {
        "approval-interactive"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "user-questions"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["approval"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["approval"]
    }
    fn description(&self) -> &'static str {
        "ask a human before every risky call"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<crate::seams::ApprovalSvc>(Arc::new(AskingPolicy { ctx: ctx.clone() }))
            .map_err(|e| e.to_string())?;
        let _ = ctx.on_waterfall::<ToolsExecute>(
            Arc::new(super::policy::ApprovalGate { ctx: ctx.clone() }),
            false,
        );
        Ok(())
    }
}

// ---- telemetry ----------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct TelemetryRow {
    /// Write one JSON line per turn here. Empty means stderr.
    #[serde(default)]
    path: Option<String>,
}

/// Turn-level metrics, assembled from events nobody produced for it.
pub struct TelemetryPlugin;

#[async_trait]
impl Plugin for TelemetryPlugin {
    fn name(&self) -> &'static str {
        "telemetry"
    }
    fn uses(&self) -> &'static [&'static str] {
        // It reports whatever the projection registry has folded; with no
        // projections mounted the line simply carries no token totals.
        &["session-projections"]
    }
    fn description(&self) -> &'static str {
        "one metrics line per turn, derived from the session log"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: TelemetryRow = parse(config)?;
        let path = row.path.map(PathBuf::from);
        let started = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));

        let start_clock = started.clone();
        let _ = ctx.on_emit::<TurnStart>(move |_: &TurnStarted| {
            *start_clock.lock().expect("clock poisoned") = std::time::Instant::now();
        });

        let ctx_for_end = ctx.clone();
        let _ = ctx.on_emit::<TurnEnd>(move |outcome: &TurnOutcome| {
            let elapsed = started
                .lock()
                .expect("clock poisoned")
                .elapsed()
                .as_millis();
            let tokens = ctx_for_end
                .service::<crate::seams::SessionProjectionsSvc>()
                .and_then(|p| p.state_of("tokenTotals"))
                .unwrap_or(Value::Null);
            let title = crate::agent::scoped(&ctx_for_end)
                .service::<SessionSvc>()
                .map(|s| s.id().to_string())
                .unwrap_or_default();
            let line = serde_json::json!({
                "session": title,
                "turn": outcome.turn,
                "rounds": outcome.rounds,
                "tool_calls": outcome.tool_calls,
                "stop": format!("{:?}", outcome.stop),
                "error": outcome.error,
                "elapsed_ms": elapsed,
                "tokens": tokens,
            });
            let rendered = format!("{line}\n");
            match &path {
                Some(path) => {
                    use std::io::Write;
                    if let Some(parent) = path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                    {
                        let _ = file.write_all(rendered.as_bytes());
                    }
                }
                None => eprint!("{rendered}"),
            }
        });
        Ok(())
    }
}

// ---- cost ceiling -------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct BudgetRow {
    /// Stop a turn once its prompt tokens pass this. `0` disables.
    #[serde(default)]
    max_prompt_tokens: u32,
}

struct TokenBudget {
    max_prompt_tokens: u32,
}

#[async_trait]
impl Listener<TurnStopping> for TokenBudget {
    async fn call(&self, progress: &TurnProgress) -> Option<StopReason> {
        (self.max_prompt_tokens > 0 && progress.used_tokens >= self.max_prompt_tokens)
            .then_some(StopReason::StoppedByPolicy)
    }
}

/// A cost ceiling. Twelve lines, because the seam it needs already exists.
pub struct TokenBudgetPlugin;

#[async_trait]
impl Plugin for TokenBudgetPlugin {
    fn name(&self) -> &'static str {
        "token-budget"
    }
    fn description(&self) -> &'static str {
        "end a turn once it has spent enough prompt tokens"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: BudgetRow = parse(config)?;
        let _ = ctx.on_serial::<TurnStopping>(Arc::new(TokenBudget {
            max_prompt_tokens: row.max_prompt_tokens,
        }));
        Ok(())
    }
}
