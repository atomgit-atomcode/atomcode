//! Policy and product rows that were flags, strings or middleware entries in the
//! builder-assembled agent.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    Decision, SessionSvc, StopReason, ToolsSvc, TurnOutcome, UserQuestions, UserQuestionsSvc,
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
        // This row's guidance for this row's tool — including the judgment a parameter list
        // cannot carry. It lived in the coding persona as `## TASK TRACKING`, which meant it
        // described `todowrite` on BOTH assemblies and could not leave with this row when the
        // tool was patched out.
        contribute_prompt(
            ctx,
            "tool-todo",
            53,
            "Use `todowrite` to plan multi-step work: add one todo per concrete step before you \
             start, and mark each completed as soon as it is done. Call it FIRST when a task has \
             multiple requests, phases, files, dependencies, ambiguity, or needs investigation \
             followed by changes, and make the initial list cover the WHOLE request — \
             investigation, design, implementation, verification — not only the next action. \
             Each item names a concrete outcome a later turn can execute without re-planning \
             (`add retry to fetch_user`, not `fix networking`; never `task 1` or `step 2`). Then \
             keep it current ONE item at a time rather than resending the list: \
             `todowrite {\"action\":\"update\",\"id\":N,\"status\":\"in_progress\"}` when you \
             start #N, and `completed` the moment it is actually verified — never on intent, and \
             never batch-completing several at the end. Keep exactly one item in_progress. \
             The list is there to show where the work actually stands, so keep it true: if the \
             plan changed and an item is no longer part of the task, replace or drop it rather \
             than leaving it open. Do not declare done or hand back while an item is still open \
             and still wanted — finish it, or say plainly which are open and why (blocked, \
             needing a decision, ambiguous, or no longer wanted). If the \
             user pivots to unrelated multi-step work, call it with the new full list to REPLACE \
             the old one instead of carrying stale items forward. Not for a single quick edit, a \
             one-off command, or a purely informational reply.",
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
            //
            // PRESUMED, not the person: a rule is something they set up once to
            // stop being asked, not an answer to this call. A boundary below
            // will still stop the call, which is the point of writing one.
            RuleDecision::Allow => {
                exec.authorization = crate::events::Authorization::Presumed;
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
    async fn ask(&self, _question: &crate::seams::Question) -> Option<String> {
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
    /// Scopes the person answered `allow_always` to, `{tool}::{scope}`.
    ///
    /// In memory and session-lived on purpose: a grant that outlived the
    /// session would be a permission nobody remembers giving. The scope comes
    /// from the tool rather than the raw arguments, so `write_file` can be
    /// tool-wide while `bash` stays per-command — see
    /// [`atomcode_kernel::tool::Tool::always_grant_scope`].
    granted: Mutex<HashSet<String>>,
}

impl AskingPolicy {
    /// The member asking, when it is not the agent the person is driving.
    ///
    /// Taken from the task-local "current agent" rather than passed down the
    /// call: approval runs inside the tool call, which runs inside the agent's
    /// own turn, so the answer is already here. An agent with a parent is a
    /// delegated one; its name is the last segment of its session id, which is
    /// the name the lead gave it.
    fn asker(&self) -> Option<String> {
        crate::agent::current_member_name(&self.ctx)
    }
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
        // The tool's own name, not the call's: a gate that presents a safe
        // tool as risky says why in the name, and the person should see it.
        let scope = tool.always_grant_scope(&call.arguments);
        // The remembered key is the tool and its scope; the scope alone is what
        // the person is shown, because the tool is already on the card.
        // `NEVER_GRANT` is a gate saying "this one may never be remembered"
        // (writing to a credential file, say). Then there is nothing to look up,
        // and — below — nothing to offer: showing "always allow" for a decision
        // that will be asked again anyway tells the person something untrue.
        // NOT the empty scope, which several tools already use to mean the
        // opposite: a TOOL-WIDE grant.
        let grantable = scope != crate::seams::NEVER_GRANT;
        let grant = format!("{}::{scope}", tool.name());
        if grantable
            && self
                .granted
                .lock()
                .expect("grants poisoned")
                .contains(&grant)
        {
            return Decision::Allow;
        }
        // Refused before anything is asked, so a tree with nothing to ask
        // through denies rather than quietly allowing.
        if self.ctx.service::<UserQuestionsSvc>().is_none() {
            return Decision::Deny("no way to ask for approval".into());
        }
        let asker = self.asker();
        // One constructor with the driver's own approval path (`Asker::decide`
        // in `handle.rs`), so the same call reads the same way wherever the
        // question is asked and the log records one question rather than two
        // dialects for it.
        let question = crate::seams::Question::approval(
            tool.name(),
            &call.arguments,
            grantable.then_some(scope.as_str()),
            asker,
        );
        // `ask_person`, not the seam directly: the question and its answer are
        // one fact, and this is the row holding both. The transcript draws the
        // card from that fact, the way it draws every other block — see
        // [`SessionEvent::Answered`].
        match crate::agent::ask_person(&self.ctx, question)
            .await
            .as_deref()
        {
            Some(crate::seams::ANSWER_ALLOW) => Decision::Allow,
            Some(crate::seams::ANSWER_ALWAYS) => {
                self.granted.lock().expect("grants poisoned").insert(grant);
                Decision::Allow
            }
            // Anything else is a refusal, including an answer nobody offered:
            // consent is only ever the words that were offered for it.
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
            .provide::<crate::seams::ApprovalSvc>(Arc::new(AskingPolicy {
                ctx: ctx.clone(),
                granted: Mutex::new(HashSet::new()),
            }))
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
