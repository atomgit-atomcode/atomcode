//! 这个产品自己的安全闸行。
//!
//! 三行,各守一条边界:凭据不能经 shell 捞出去、写不能落到工作区外、破坏性 bash
//! 不能打到工作区外。它们原来住在 `atomcode-harness` 的 `plugins/policy.rs` 里,
//! 而挂它们的只有这里(`on_harness.rs`)—— harness 自己的 bundle 一个都没挂。
//!
//! 搬过来是因为方向:**判断"什么算越界"是产品的事**。机制层提供的是通用件 ——
//! `Waterfall<ToolsExecute>` 这条瀑布、`ApprovalSvc` 这道缝、`AsRisky` 这个"当成
//! 危险的来问"的壳 —— 而"凭据长什么样""工作区边界在哪"是这个产品的答案。
//!
//! 闸的实现本身在 `atomcode-capabilities::tools` 下(`credential_bash_gate` 838 行、
//! `write_approval` 823 行、`bash_workspace_gate` 1746 行);这里只是把它们接到
//! 瀑布上的三个行。

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::events::{Authorization, ToolExec, ToolsExecute};
use atomcode_harness::plugins::policy::AsRisky;
use atomcode_harness::seams::{ApprovalSvc, Decision, ToolsSvc};
use atomcode_kernel::tool::{Tool, ToolResult};
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

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
        // A boundary, and the sharpest one: the person set `credential_shell`
        // precisely to be stopped here, so a presumed authorization — an allow
        // rule, a hook, a gate calling the call benign — does not pass.
        if exec.authorization.by_person() {
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
            // `strict`: refused, and the turn ends here — another spelling of
            // the same command would be just as unsafe to try. The person gets
            // the recovery choice as a logged fact; the loop's stopping
            // question (below) reads it and ends the turn after this round.
            CredentialShellVerdict::DenyTurn => {
                let scoped = atomcode_harness::agent::scoped(&self.ctx);
                if let Some(log) = scoped.service::<atomcode_harness::seams::SessionSvc>() {
                    atomcode_harness::session::commit(
                        &scoped,
                        &log,
                        atomcode_harness::session::SessionEvent::PolicyIntervention {
                            turn: log.current_turn(),
                            intervention:
                                atomcode_kernel::event::PolicyIntervention::credential_shell_blocked(
                                ),
                        },
                    );
                }
                return refuse(CREDENTIAL_BASH_DENIAL_REASON);
            }
            CredentialShellVerdict::Deny => return refuse(CREDENTIAL_BASH_DENIAL_REASON),
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
                exec.authorization = Authorization::ByPerson;
                next.run(exec).await
            }
            Decision::Deny(why) => refuse(&format!("{CREDENTIAL_BASH_DENIAL_REASON} ({why})")),
        }
    }
}

/// Ends the turn after a round that committed a policy intervention.
struct StopOnIntervention {
    ctx: Context,
}

#[async_trait]
impl atomcode_plexus::Listener<atomcode_harness::events::TurnStopping> for StopOnIntervention {
    async fn call(
        &self,
        progress: &atomcode_harness::events::TurnProgress,
    ) -> Option<atomcode_harness::seams::StopReason> {
        let log = atomcode_harness::agent::scoped(&self.ctx)
            .service::<atomcode_harness::seams::SessionSvc>()?;
        log.events()
            .iter()
            .rev()
            .take_while(|logged| {
                !matches!(logged.event, atomcode_harness::session::SessionEvent::TurnStart { .. })
            })
            .any(|logged| {
                matches!(
                    &logged.event,
                    atomcode_harness::session::SessionEvent::PolicyIntervention { turn, .. } if *turn == progress.turn
                )
            })
            .then_some(atomcode_harness::seams::StopReason::PolicyDenied)
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
        // A turn whose round committed a policy intervention ends with that round.
        let _ =
            ctx.on_serial::<atomcode_harness::events::TurnStopping>(Arc::new(StopOnIntervention {
                ctx: ctx.clone(),
            }));
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
        // This row guards a boundary AND a convenience, so the short-circuit
        // cannot come before the verdict that tells them apart. A sensitive
        // target is `Ask { grantable: false }` — not even "always allow" may
        // grant it, so a presumed authorization certainly may not. Everything
        // else is an ordinary prompt an allow rule is entitled to skip.
        if exec.authorization.by_person() {
            return next.run(exec).await;
        }
        // The row's own setting, or the person's live switch when a host
        // provides one: either is a yes.
        let accept_edits = self.accept_edits
            || self
                .ctx
                .service::<atomcode_harness::seams::ModesSvc>()
                .is_some_and(|modes| {
                    modes
                        .accept_edits
                        .load(std::sync::atomic::Ordering::Relaxed)
                });
        let verdict = write_verdict(
            &exec.call.name,
            &exec.call.arguments,
            Some(self.working_dir.as_path()),
            accept_edits,
        )
        .await;
        let (grantable, scope) = match verdict {
            WriteVerdict::NotOurs => return next.run(exec).await,
            // The gate's own judgement — in-workspace, or accept-edits — not
            // anyone's answer. Say so downstream so no later gate asks.
            WriteVerdict::Allow(_) => {
                exec.authorization = Authorization::Presumed;
                return next.run(exec).await;
            }
            WriteVerdict::Ask { grantable, scope } => (grantable, scope),
        };
        // Grantable means an ordinary prompt, which is the convenience half: a
        // presumed authorization is allowed to skip it. Ungrantable means the
        // sensitive half, and falls through to ask however it was marked.
        if grantable && exec.authorization.settled() {
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
                exec.authorization = Authorization::ByPerson;
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
    /// Auto-approve non-sensitive edits, from mount. A host that lets the person
    /// toggle this mid-session provides the `modes` service, which is read live.
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
        &["approval", "modes"]
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
        // A convenience: leaving the workspace is worth a prompt, not a
        // boundary — so any settled answer ends it. The destructive/sensitive
        // commands this would otherwise catch are a boundary, and they are
        // caught above by `sensitive-paths` and `tool-credential-shell`.
        if exec.authorization.settled() {
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
                exec.authorization = Authorization::ByPerson;
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
