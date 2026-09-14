//! Policy plugins: everything the loop deliberately does not know about.
//!
//! All three attach to the same `tools/execute` waterfall, and the order they
//! are listed in the config is the order they wrap the execution:
//!
//! ```text
//! repair-args  -> approval -> result-cap -> [ the tool runs ]
//!      rewrites      gates       truncates on the way back out
//! ```
//!
//! Repair must precede approval, so a human (or a rule) approves the exact bytes
//! that will execute. That is a real ordering contract, and it is expressed by
//! `prepend` plus row order rather than by a hardcoded chain in the loop.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::repair_tool_args;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolResult};
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{ToolExec, ToolsExecute};
use crate::seams::{ApprovalPolicy, ApprovalSvc, Decision, ToolsSvc};

// ---- argument repair ----------------------------------------------------

struct RepairArgs;

#[async_trait]
impl Waterfall<ToolsExecute> for RepairArgs {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        let repaired = repair_tool_args(&exec.call.name, &exec.call.arguments);
        if repaired != exec.call.arguments {
            exec.call.arguments = repaired;
        }
        next.run(exec).await
    }
}

pub struct RepairArgsPlugin;

#[async_trait]
impl Plugin for RepairArgsPlugin {
    fn name(&self) -> &'static str {
        "tool-args-repair"
    }
    fn description(&self) -> &'static str {
        "normalize model-produced tool arguments before anything inspects them"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        // Prepended: no policy listener may see arguments that differ from the
        // ones the tool will actually receive.
        let _ = ctx.on_waterfall::<ToolsExecute>(Arc::new(RepairArgs), true);
        Ok(())
    }
}

// ---- approval -----------------------------------------------------------

#[derive(Clone, Copy, Debug, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalMode {
    /// Everything runs. For sandboxes and evals.
    Yolo,
    /// Tools that declare themselves risky for these arguments are refused.
    #[default]
    DenyRisky,
    /// Only tools that declare themselves read-only run.
    ReadOnly,
}

#[derive(Debug, Deserialize, Default)]
struct ApprovalRow {
    #[serde(default)]
    mode: ApprovalMode,
    /// Tool names that always run regardless of mode.
    #[serde(default)]
    allow: Vec<String>,
    /// Tool names that never run.
    #[serde(default)]
    deny: Vec<String>,
}

struct RulePolicy {
    mode: ApprovalMode,
    allow: Vec<String>,
    deny: Vec<String>,
}

#[async_trait]
impl ApprovalPolicy for RulePolicy {
    async fn decide(&self, call: &ToolCall, tool: &Arc<dyn Tool>) -> Decision {
        if self.deny.iter().any(|n| n == &call.name) {
            return Decision::Deny(format!("`{}` is denied by configuration", call.name));
        }
        if self.allow.iter().any(|n| n == &call.name) {
            return Decision::Allow;
        }
        match self.mode {
            ApprovalMode::Yolo => Decision::Allow,
            ApprovalMode::DenyRisky => match tool.risk(&call.arguments) {
                RiskLevel::Safe => Decision::Allow,
                // The model reads this, and so does the person watching. Both
                // need to know it was policy rather than capability, and the
                // person needs to know which row to change.
                RiskLevel::Risky => Decision::Deny(format!(
                    "`{}` is risky and this harness refuses risky calls without asking \
                     (approval mode `deny-risky`, and nothing here can ask a human).\n  \
                     To let it run: rerun with `--profile repl` or `--tui` to be asked, \
                     `--yolo` to allow everything, or allow just this tool with \
                     `--patch` on the `permissions` row.",
                    call.name
                )),
            },
            ApprovalMode::ReadOnly => {
                if tool.read_only_hint() {
                    Decision::Allow
                } else {
                    Decision::Deny(format!(
                        "`{}` may have side effects and this harness is read-only \
                         (approval mode `read-only`). Rerun without `--plan`/`--read-only` \
                         to allow changes.",
                        call.name
                    ))
                }
            }
        }
    }
}

pub(super) struct ApprovalGate {
    pub(super) ctx: Context,
}

#[async_trait]
impl Waterfall<ToolsExecute> for ApprovalGate {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        // Someone upstream already authorized this call — an allow rule, a
        // remembered grant. Asking again would be noise.
        if exec.pre_approved {
            return next.run(exec).await;
        }
        let (Some(policy), Some(toolbox)) = (
            self.ctx.service::<ApprovalSvc>(),
            self.ctx.service::<ToolsSvc>(),
        ) else {
            return next.run(exec).await;
        };
        let Some(tool) = toolbox.get(&exec.call.name) else {
            // Unknown tool: let the terminal produce the canonical error rather
            // than inventing a second wording for it here.
            return next.run(exec).await;
        };
        match policy.decide(&exec.call, &tool).await {
            Decision::Allow => next.run(exec).await,
            // Short-circuit: the decision is owned here, and the model gets a
            // result it can react to instead of a silent gap in the history.
            Decision::Deny(reason) => ToolResult {
                call_id: exec.call.id.clone(),
                content: format!("Refused: {reason}"),
                is_error: true,
                images: vec![],
            },
        }
    }
}

pub struct ApprovalPlugin;

#[async_trait]
impl Plugin for ApprovalPlugin {
    fn name(&self) -> &'static str {
        "approval"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // The gate resolves the policy per call rather than capturing it, so a
        // patch that replaces the policy is picked up without remounting.
        &["approval"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["approval"]
    }
    fn description(&self) -> &'static str {
        "gate tool calls on their declared risk"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ApprovalRow = if config.is_null() {
            ApprovalRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let _ = ctx
            .provide::<ApprovalSvc>(Arc::new(RulePolicy {
                mode: row.mode,
                allow: row.allow,
                deny: row.deny,
            }))
            .map_err(|e| e.to_string())?;
        let _ =
            ctx.on_waterfall::<ToolsExecute>(Arc::new(ApprovalGate { ctx: ctx.clone() }), false);
        Ok(())
    }
}

// ---- result capping -----------------------------------------------------

#[derive(Debug, Deserialize)]
struct CapRow {
    #[serde(default = "default_cap")]
    max_bytes: usize,
}

fn default_cap() -> usize {
    64 * 1024
}

struct CapResult {
    max_bytes: usize,
}

#[async_trait]
impl Waterfall<ToolsExecute> for CapResult {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        // Everything here happens on the way *out* — the `after` half of the old
        // middleware pair, with no second registration point.
        let mut result = next.run(exec).await;
        if self.max_bytes > 0 && result.content.len() > self.max_bytes {
            let keep = self.max_bytes / 2;
            let head: String = result.content.chars().take(keep).collect();
            let tail: String = {
                let chars: Vec<char> = result.content.chars().collect();
                chars[chars.len().saturating_sub(keep)..].iter().collect()
            };
            let dropped = result.content.len();
            result.content = format!(
                "{head}\n\n[... {dropped} bytes truncated by tool-result-cap ...]\n\n{tail}"
            );
        }
        result
    }
}

pub struct ResultCapPlugin;

#[async_trait]
impl Plugin for ResultCapPlugin {
    fn name(&self) -> &'static str {
        "tool-result-cap"
    }
    fn description(&self) -> &'static str {
        "bound a single tool result so one runaway output cannot fill the window"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: CapRow = if config.is_null() {
            CapRow {
                max_bytes: default_cap(),
            }
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let _ = ctx.on_waterfall::<ToolsExecute>(
            Arc::new(CapResult {
                max_bytes: row.max_bytes,
            }),
            false,
        );
        Ok(())
    }
}

// ---- sensitive paths --------------------------------------------------------

/// A tool as the approval seam should see it when its arguments name a
/// sensitive path: risky, whatever it says about itself.
///
/// Approval is risk-based, and `read_file` / `grep` / `glob` are `Safe` — so
/// they never ask, and `~/.ssh/id_rsa` rides a tool result straight to the
/// provider. This view changes nothing but the answer to `risk`, so the same
/// asker, the same modes (deny-risky refuses, interactive asks, yolo allows)
/// and the same remembered grants apply. No second approval seam.
struct AsRisky {
    inner: Arc<dyn Tool>,
    name: String,
    /// What an `allow always` should be remembered against, when the gate has a
    /// narrower idea of "the same call" than the raw arguments. `None` = whatever
    /// the wrapped tool says.
    ///
    /// It matters because the grant store is keyed `{tool}::{scope}`: a gate that
    /// remembered raw arguments would re-ask for every spelling of one decision.
    scope: Option<String>,
    /// Whether "allow always" may be offered at all.
    ///
    /// `false` for decisions that must be taken every time — writing to a
    /// credential file is the case that forced it. The honest way to express that
    /// is to NOT OFFER the option: a person who is shown "always allow", presses
    /// it, and is asked again next time has been told something untrue about
    /// their own permission. An empty grant scope is how that reaches the
    /// approval policy, which reads it and drops the option.
    grantable: bool,
}

#[async_trait]
impl Tool for AsRisky {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    fn read_only_hint(&self) -> bool {
        self.inner.read_only_hint()
    }
    fn self_bounds_output(&self) -> bool {
        self.inner.self_bounds_output()
    }
    fn parallel_safe(&self, args: &str) -> bool {
        self.inner.parallel_safe(args)
    }
    fn always_grant_scope(&self, args: &str) -> String {
        if !self.grantable {
            return crate::seams::NEVER_GRANT.to_string();
        }
        self.scope
            .clone()
            .unwrap_or_else(|| self.inner.always_grant_scope(args))
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        self.inner.execute(args, ctx).await
    }
}

/// Before a `Safe` tool touches a sensitive path, ask the approval seam as if
/// the tool were risky. Runs ahead of the approval gate; a risky tool is left
/// to that gate, so nothing is asked twice.
pub struct SensitivePathGate {
    pub ctx: Context,
}

#[async_trait]
impl Waterfall<ToolsExecute> for SensitivePathGate {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        if exec.pre_approved {
            return next.run(exec).await;
        }
        let (Some(policy), Some(toolbox)) = (
            self.ctx.service::<ApprovalSvc>(),
            self.ctx.service::<ToolsSvc>(),
        ) else {
            return next.run(exec).await;
        };
        let Some(tool) = toolbox.get(&exec.call.name) else {
            return next.run(exec).await;
        };
        if tool.risk(&exec.call.arguments) != RiskLevel::Safe {
            return next.run(exec).await;
        }
        if !atomcode_capabilities::tools::sensitive_path::references_sensitive_path(
            &exec.call.arguments,
        ) {
            return next.run(exec).await;
        }
        let as_risky: Arc<dyn Tool> = Arc::new(AsRisky {
            name: format!("{} (sensitive path)", tool.name()),
            inner: tool,
            scope: None,
            grantable: true,
        });
        match policy.decide(&exec.call, &as_risky).await {
            Decision::Allow => next.run(exec).await,
            Decision::Deny(reason) => ToolResult {
                call_id: exec.call.id.clone(),
                content: format!(
                    "Refused: the arguments name a sensitive path (credentials, keys, `.env`) \
                     and {reason}"
                ),
                is_error: true,
                images: vec![],
            },
        }
    }
}

pub struct SensitivePathsPlugin;

#[async_trait]
impl Plugin for SensitivePathsPlugin {
    fn name(&self) -> &'static str {
        "sensitive-paths"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["approval"]
    }
    fn description(&self) -> &'static str {
        "ask before a read-only tool touches credentials, keys or `.env` — through the approval seam"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .on_waterfall::<ToolsExecute>(Arc::new(SensitivePathGate { ctx: ctx.clone() }), false);
        Ok(())
    }
}

// ---- open_file workspace pre-approval ------------------------------------
//
// The first of coding's `ToolMiddleware` gates to wear the harness shell.
//
// The judgement itself did not move and was not copied: it lives in
// `OpenFileWorkspaceGate::authorizes` (L1), which the kernel middleware also
// calls. Only the vocabulary for saying "already authorized" differs — there
// `BeforeOutcome::Allow`, here `ToolExec::pre_approved`, and both mean the same
// thing to the gate downstream: do not ask about this one.
//
// That split is the point. A gate rewritten rather than shared would be two
// judgements that agree today, which is a different and worse thing than one
// judgement with two shells.

struct OpenFileWorkspace {
    gate: Arc<atomcode_capabilities::tools::OpenFileWorkspaceGate>,
}

#[async_trait]
impl Waterfall<ToolsExecute> for OpenFileWorkspace {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        // Marking an already-marked call is not wrong, just work: something
        // upstream already settled it.
        if !exec.pre_approved
            && self
                .gate
                .authorizes(&exec.call.name, &exec.call.arguments)
                .await
        {
            exec.pre_approved = true;
        }
        next.run(exec).await
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OpenFileWorkspaceRow {
    /// The workspace boundary. The L1 gate can follow a live `change_dir` through
    /// a shared cwd handle; this row pins it at mount time, same as `agent-loop`
    /// and `fs` do with theirs. Following a mid-session move is a later
    /// refinement, and a visible one — not silent drift.
    working_dir: Option<String>,
}

pub struct OpenFileWorkspacePlugin;

#[async_trait]
impl Plugin for OpenFileWorkspacePlugin {
    fn name(&self) -> &'static str {
        "tool-open-file-workspace"
    }
    fn description(&self) -> &'static str {
        "an open_file target inside the workspace needs no approval"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        // `Value::Null` is what a row with no `config =` block gets, and
        // `from_value` rejects it — so the default has to be reached explicitly.
        let row: OpenFileWorkspaceRow = if config.is_null() {
            OpenFileWorkspaceRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let root = std::path::PathBuf::from(row.working_dir.as_deref().unwrap_or("."));
        let gate = Arc::new(atomcode_capabilities::tools::OpenFileWorkspaceGate::pinned(
            root,
        ));
        // NOT prepended: this only ever *widens* (marks pre-approved), so it must
        // run after anything that may narrow or rewrite the call.
        let _ = ctx.on_waterfall::<ToolsExecute>(Arc::new(OpenFileWorkspace { gate }), false);
        Ok(())
    }
}

// ---- oversized tool output spills to an artifact -------------------------
//
// The second gate to wear the harness shell, and the one that shows what the
// shape is actually worth. The kernel trait splits a tool call into two calls —
// `before` and `after` — so anything that needs to know in `after` what it saw
// in `before` has to carry state between them itself (`GitPushLabelMiddleware`
// keeps a `Mutex<HashSet<call_id>>` for exactly that, and for nothing else).
// A waterfall listener sits on BOTH sides of `next.run`, so that correlation is
// just a local variable.
//
// This one only needs the `after` side, so it reads as: run it, then spill.

struct OutputArtifact {
    ctx: Context,
    spill: Arc<atomcode_capabilities::tools::ArtifactMiddleware>,
}

#[async_trait]
impl Waterfall<ToolsExecute> for OutputArtifact {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        // Taken before the call: `exec` is borrowed for the duration of `run`.
        let name = exec.call.name.clone();
        let mut result = next.run(exec).await;
        // A tool that bounds and structures its own output (`read_file`: self-capped,
        // 1-based line numbers, paginated) must reach the model WHOLE — head/tail
        // truncation would corrupt it. Ask the resolved tool, not the call.
        let self_bounds = self
            .ctx
            .service::<ToolsSvc>()
            .and_then(|t| t.get(&name))
            .is_some_and(|t| t.self_bounds_output());
        self.spill.spill(&mut result, self_bounds).await;
        result
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct OutputArtifactRow {
    /// Where spilled outputs are written. Coding derives this from the session
    /// directory and mounts nothing when there is no session — same rule here:
    /// no `dir`, no row, because an artifact nobody can fetch later is worse
    /// than not truncating at all.
    dir: Option<String>,
}

pub struct OutputArtifactPlugin;

#[async_trait]
impl Plugin for OutputArtifactPlugin {
    fn name(&self) -> &'static str {
        "tool-output-artifact"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "an oversized tool result is saved whole and shown head + tail"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: OutputArtifactRow = if config.is_null() {
            OutputArtifactRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let Some(dir) = row.dir else {
            // Mounted without a place to put them: say so rather than silently
            // truncating into nowhere.
            return Err("`tool-output-artifact` needs `config = { dir = … }`".into());
        };
        let store = Arc::new(atomcode_capabilities::tools::ArtifactStore::new(dir));
        // The spill message tells the model, verbatim: "To read more:
        // fetch_output(artifact_id=…)". A row that truncated a result and named
        // a tool it had not mounted would be sending the model after something
        // that does not exist — which is what this row did until a catalog
        // comparison against the chain turned it up.
        super::tools::mount(
            ctx,
            vec![Arc::new(atomcode_capabilities::tools::FetchOutputTool::new(
                store.clone(),
            ))],
        )?;
        let spill = Arc::new(atomcode_capabilities::tools::ArtifactMiddleware::new(store));
        // Appended: it rewrites the RESULT, so it must see what every earlier
        // listener produced.
        let _ = ctx.on_waterfall::<ToolsExecute>(
            Arc::new(OutputArtifact {
                ctx: ctx.clone(),
                spill,
            }),
            false,
        );
        Ok(())
    }
}

// ---- credentials in the generic shell ------------------------------------
//
// The third gate to move, and the first that has to ASK. Everything about the
// asking is delegated: the row decides only WHETHER this call touches
// credentials (`credential_shell_verdict`, shared with the kernel shell), then
// hands the question to the `approval` seam, which owns the prompt, the wording
// and the `{tool}::{scope}` grant memory.
//
// That is the structural difference from coding, where each gate carried its own
// `PermissionStore`: here one store answers for every gate, so "allow always"
// means the same thing no matter which gate asked.

struct CredentialShell {
    ctx: Context,
    policy: atomcode_capabilities::tools::CredentialShellPolicy,
}

#[async_trait]
impl Waterfall<ToolsExecute> for CredentialShell {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        use atomcode_capabilities::tools::credential_bash_gate::{
            credential_shell_verdict, grant_scope, CredentialShellVerdict,
            CREDENTIAL_BASH_DENIAL_REASON,
        };
        if exec.pre_approved {
            return next.run(exec).await;
        }
        let policy = self.ctx.service::<ApprovalSvc>();
        // No `approval` row mounted is this shell's "nobody to ask": the same
        // condition the kernel gate reads off a missing `PermissionStore`.
        let verdict = credential_shell_verdict(
            self.policy,
            &exec.call.name,
            &exec.call.arguments,
            policy.is_some(),
        );
        let refuse = |reason: &str| ToolResult {
            call_id: exec.call.id.clone(),
            content: format!("Refused: {reason}"),
            is_error: true,
            images: vec![],
        };
        match verdict {
            CredentialShellVerdict::NotOurs => return next.run(exec).await,
            // The turn-ending variant of `strict` is a kernel notion (it carries a
            // `PolicyIntervention`); here the call is refused and the loop decides
            // what to do with a refusal, which is the harness's own vocabulary.
            CredentialShellVerdict::DenyTurn | CredentialShellVerdict::Deny => {
                return refuse(CREDENTIAL_BASH_DENIAL_REASON)
            }
            CredentialShellVerdict::Ask => {}
        }
        let (Some(policy), Some(toolbox)) = (policy, self.ctx.service::<ToolsSvc>()) else {
            return refuse(CREDENTIAL_BASH_DENIAL_REASON);
        };
        let Some(tool) = toolbox.get(&exec.call.name) else {
            return next.run(exec).await;
        };
        let asking: Arc<dyn Tool> = Arc::new(AsRisky {
            name: format!("{} (credential access)", tool.name()),
            inner: tool,
            // Keyed on the SAME normalized scope the kernel gate grants against,
            // so one "allow always" covers the same set of commands either way.
            scope: Some(grant_scope(&exec.call.arguments)),
            grantable: true,
        });
        match policy.decide(&exec.call, &asking).await {
            Decision::Allow => {
                exec.pre_approved = true;
                next.run(exec).await
            }
            Decision::Deny(why) => refuse(&format!("{CREDENTIAL_BASH_DENIAL_REASON} ({why})")),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CredentialShellRow {
    /// `off` | `prompt` | `strict`. Defaults to the L1 default (`prompt`).
    policy: Option<String>,
}

pub struct CredentialShellPlugin;

#[async_trait]
impl Plugin for CredentialShellPlugin {
    fn name(&self) -> &'static str {
        "tool-credential-shell"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["approval"]
    }
    fn description(&self) -> &'static str {
        "a shell command that would expose credentials is asked about, or refused"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        use atomcode_capabilities::tools::CredentialShellPolicy;
        let row: CredentialShellRow = if config.is_null() {
            CredentialShellRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let policy = match row.policy.as_deref() {
            None | Some("prompt") => CredentialShellPolicy::Prompt,
            Some("off") => CredentialShellPolicy::Off,
            Some("strict") => CredentialShellPolicy::Strict,
            Some(other) => {
                return Err(format!(
                    "`tool-credential-shell` policy must be off / prompt / strict, not `{other}`"
                ))
            }
        };
        let _ = ctx.on_waterfall::<ToolsExecute>(
            Arc::new(CredentialShell {
                ctx: ctx.clone(),
                policy,
            }),
            true,
        );
        Ok(())
    }
}

// ---- writes outside the workspace ---------------------------------------
//
// The fourth gate, and the first that needs "ask, but never remember": writing
// to a key or a `.env` is asked about EVERY time. That reaches the approval
// policy as an empty grant scope, which drops the "always allow" option rather
// than offering a button that would not hold.

struct WriteApproval {
    ctx: Context,
    working_dir: std::path::PathBuf,
    accept_edits: bool,
}

#[async_trait]
impl Waterfall<ToolsExecute> for WriteApproval {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        use atomcode_capabilities::tools::write_approval::{write_verdict, WriteVerdict};
        if exec.pre_approved {
            return next.run(exec).await;
        }
        let verdict = write_verdict(
            &exec.call.name,
            &exec.call.arguments,
            Some(self.working_dir.as_path()),
            self.accept_edits,
        )
        .await;
        let (grantable, scope) = match verdict {
            WriteVerdict::NotOurs => return next.run(exec).await,
            // Authorized without asking — say so downstream so no later gate asks.
            WriteVerdict::Allow(_) => {
                exec.pre_approved = true;
                return next.run(exec).await;
            }
            WriteVerdict::Ask { grantable, scope } => (grantable, scope),
        };
        let (Some(policy), Some(toolbox)) = (
            self.ctx.service::<ApprovalSvc>(),
            self.ctx.service::<ToolsSvc>(),
        ) else {
            return next.run(exec).await;
        };
        let Some(tool) = toolbox.get(&exec.call.name) else {
            return next.run(exec).await;
        };
        let asking: Arc<dyn Tool> = Arc::new(AsRisky {
            name: if grantable {
                format!("{} (outside the workspace)", tool.name())
            } else {
                format!("{} (sensitive target)", tool.name())
            },
            inner: tool,
            scope: Some(scope),
            grantable,
        });
        match policy.decide(&exec.call, &asking).await {
            Decision::Allow => {
                // The person said yes: say so downstream, or the ordinary
                // approval behind this row asks the same question again.
                exec.pre_approved = true;
                next.run(exec).await
            }
            Decision::Deny(why) => ToolResult {
                call_id: exec.call.id.clone(),
                content: format!("Refused: {why}"),
                is_error: true,
                images: vec![],
            },
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WriteApprovalRow {
    working_dir: Option<String>,
    /// Auto-approve non-sensitive edits. Coding carries this as a live flag the
    /// person toggles mid-session; here it is mount-time, like the other rows'
    /// working dir. Making it live is a later, visible change — not silent drift.
    accept_edits: bool,
}

pub struct WriteApprovalPlugin;

#[async_trait]
impl Plugin for WriteApprovalPlugin {
    fn name(&self) -> &'static str {
        "tool-write-approval"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["approval"]
    }
    fn description(&self) -> &'static str {
        "a write outside the workspace is asked about; a sensitive one, every time"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: WriteApprovalRow = if config.is_null() {
            WriteApprovalRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let working_dir = std::path::PathBuf::from(row.working_dir.as_deref().unwrap_or("."));
        let _ = ctx.on_waterfall::<ToolsExecute>(
            Arc::new(WriteApproval {
                ctx: ctx.clone(),
                working_dir,
                accept_edits: row.accept_edits,
            }),
            true,
        );
        Ok(())
    }
}

// ---- destructive bash outside the workspace ------------------------------
//
// The fifth and last of coding's approval gates.
//
// Unlike the others this one does NOT share its whole shell with the kernel
// gate. The DETECTION is shared (`bash_workspace_verdict` calls the same
// `scan_destructive_bash`, `mv_moves` and workspace classification); what
// differs is the bookkeeping. The kernel gate grants per out-of-workspace
// DIRECTORY and auto-allows only when every one of them is already granted;
// the harness store holds one scope per decision, so the verdict joins those
// directories into a single scope.
//
// That is narrower — a later command touching only one of those directories
// asks again — and narrower is the safe direction. It also makes the "ride
// along" hazard the kernel gate guards against by hand
// (`rm /granted/x && mv ws_file /tmp/stolen`) impossible by construction.

struct BashWorkspace {
    ctx: Context,
    working_dir: std::path::PathBuf,
}

#[async_trait]
impl Waterfall<ToolsExecute> for BashWorkspace {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        use atomcode_capabilities::tools::bash_workspace_gate::{
            bash_workspace_verdict, BashWorkspaceVerdict,
        };
        if exec.pre_approved {
            return next.run(exec).await;
        }
        let (Some(policy), Some(toolbox)) = (
            self.ctx.service::<ApprovalSvc>(),
            self.ctx.service::<ToolsSvc>(),
        ) else {
            return next.run(exec).await;
        };
        let Some(tool) = toolbox.get(&exec.call.name) else {
            return next.run(exec).await;
        };
        let verdict = bash_workspace_verdict(
            &exec.call.name,
            &exec.call.arguments,
            Some(self.working_dir.as_path()),
            &tool.always_grant_scope(&exec.call.arguments),
        )
        .await;
        let (grantable, scope) = match verdict {
            // In-workspace is a DEFER, not an allow: a recursive `rm` is still risky
            // and must reach the ordinary approval behind us.
            BashWorkspaceVerdict::Defer => return next.run(exec).await,
            BashWorkspaceVerdict::Ask { grantable, scope } => (grantable, scope),
        };
        let asking: Arc<dyn Tool> = Arc::new(AsRisky {
            name: if grantable {
                format!("{} (writes outside the workspace)", tool.name())
            } else {
                format!("{} (destructive, sensitive target)", tool.name())
            },
            inner: tool,
            scope: Some(scope),
            grantable,
        });
        match policy.decide(&exec.call, &asking).await {
            Decision::Allow => {
                exec.pre_approved = true;
                next.run(exec).await
            }
            Decision::Deny(why) => ToolResult {
                call_id: exec.call.id.clone(),
                content: format!("Refused: a destructive command was not approved ({why})"),
                is_error: true,
                images: vec![],
            },
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BashWorkspaceRow {
    working_dir: Option<String>,
}

pub struct BashWorkspacePlugin;

#[async_trait]
impl Plugin for BashWorkspacePlugin {
    fn name(&self) -> &'static str {
        "tool-bash-workspace"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["approval"]
    }
    fn description(&self) -> &'static str {
        "a destructive shell command reaching outside the workspace is asked about"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: BashWorkspaceRow = if config.is_null() {
            BashWorkspaceRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let working_dir = std::path::PathBuf::from(row.working_dir.as_deref().unwrap_or("."));
        let _ = ctx.on_waterfall::<ToolsExecute>(
            Arc::new(BashWorkspace {
                ctx: ctx.clone(),
                working_dir,
            }),
            true,
        );
        Ok(())
    }
}
