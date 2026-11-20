//! Product approval for broad staging and the actual index a commit would use.
//! Read-only inspection belongs to capabilities; this row owns no Git state.
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::git_index;
use atomcode_harness::events::{Authorization, ToolExec, ToolsExecute};
use atomcode_harness::seams::{ApprovalSvc, Decision, ShellSvc, ToolsSvc, NEVER_GRANT};
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde_json::Value;

struct ReviewTool {
    inner: Arc<dyn Tool>,
    name: String,
}
#[async_trait]
impl Tool for ReviewTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        self.inner.description()
    }
    fn parameters_schema(&self) -> Value {
        self.inner.parameters_schema()
    }
    fn risk(&self, _: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    fn always_grant_scope(&self, _: &str) -> String {
        NEVER_GRANT.into()
    }
    fn allow_all_group(&self, _: &str) -> Option<String> {
        None
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        self.inner.execute(args, ctx).await
    }
}

struct GitIndexGate {
    ctx: Context,
}
#[async_trait]
impl Waterfall<ToolsExecute> for GitIndexGate {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        let refuse = |reason: &str| ToolResult {
            call_id: exec.call.id.clone(),
            content: format!("Refused: {reason}"),
            is_error: true,
            images: vec![],
        };
        let powershell =
            atomcode_capabilities::tools::powershell::is_powershell_tool(&exec.call.name);
        if !matches!(exec.call.name.as_str(), "bash" | "bash_start") && !powershell {
            return next.run(exec).await;
        }
        let Ok(value) = serde_json::from_str::<Value>(&exec.call.arguments) else {
            return next.run(exec).await;
        };
        let Some(command) = value.get("command").and_then(Value::as_str) else {
            return next.run(exec).await;
        };
        let world = self.ctx.service::<ShellSvc>();
        let dialect = if powershell {
            atomcode_capabilities::world::ShellDialect::PowerShell
        } else {
            world
                .as_ref()
                .map(|shell| shell.dialect())
                .unwrap_or(atomcode_capabilities::world::ShellDialect::Bash)
        };
        let plan = match git_index::classify_in(command, dialect) {
            Ok(None) => return next.run(exec).await,
            Ok(Some(plan)) => plan,
            Err(reason) => return refuse(&reason),
        };
        let Some(mut shell) = world else {
            return refuse("Git index inspection is unavailable; no command was executed.");
        };
        if powershell {
            let Some(native) = shell.powershell().await else {
                return refuse("PowerShell is unavailable in this execution world.");
            };
            shell = native;
        }
        let review = match git_index::inspect(&*shell, &plan, &exec.working_dir).await {
            Ok(None) => return next.run(exec).await,
            Ok(Some(review)) => review,
            Err(reason) => return refuse(&reason),
        };
        let (Some(policy), Some(tools)) = (
            self.ctx.service::<ApprovalSvc>(),
            self.ctx.service::<ToolsSvc>(),
        ) else {
            return refuse("Git index review requires an approval policy.");
        };
        let Some(tool) = tools.get(&exec.call.name) else {
            return refuse("Shell tool is unavailable.");
        };
        // A previous rule/grant does not approve this particular index. Auto's
        // explicit bypass policy still owns its normal decision.
        let asking: Arc<dyn Tool> = Arc::new(ReviewTool {
            name: format!("{} — {}", tool.name(), review.summary),
            inner: tool,
        });
        if let Decision::Deny(why) = policy.decide(&exec.call, &asking).await {
            return refuse(&why);
        }
        match git_index::inspect(&*shell, &plan, &exec.working_dir).await {
            Ok(Some(current)) if current == review => {}
            _ => return refuse("Git index or staging candidates changed during review. Inspect the changes and retry for fresh approval."),
        }
        exec.authorization = Authorization::ByPerson;
        next.run(exec).await
    }
}

pub struct GitIndexPlugin;
#[async_trait]
impl Plugin for GitIndexPlugin {
    fn name(&self) -> &'static str {
        "tool-git-index-review"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "shell"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["approval"]
    }
    fn description(&self) -> &'static str {
        "review bulk staging and the current Git index before committing"
    }
    async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
        let _ = ctx.on_waterfall::<ToolsExecute>(Arc::new(GitIndexGate { ctx: ctx.clone() }), true);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_harness::seams::{ApprovalPolicy, ToolBox};
    use atomcode_kernel::tool::ToolCall;
    use atomcode_plexus::{App, ConfigTree, PluginRegistry};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    struct Bash(&'static str);
    #[async_trait]
    impl Tool for Bash {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test shell"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({})
        }
        fn allow_all_group(&self, _: &str) -> Option<String> {
            Some("all-bash".into())
        }
        async fn execute(&self, _: &str, _: &ToolContext) -> ToolResult {
            unreachable!()
        }
    }
    struct Policy {
        seen: Mutex<Vec<String>>,
        change: Option<PathBuf>,
        deny: bool,
    }
    #[async_trait]
    impl ApprovalPolicy for Policy {
        async fn decide(&self, _: &ToolCall, tool: &Arc<dyn Tool>) -> Decision {
            assert_eq!(tool.always_grant_scope("{}"), NEVER_GRANT);
            assert_eq!(tool.allow_all_group("{}"), None);
            self.seen.lock().unwrap().push(tool.name().into());
            if let Some(root) = &self.change {
                std::fs::write(root.join("user.txt"), "edited during approval").unwrap();
                git(root, &["add", "--", "user.txt"]);
            }
            if self.deny {
                Decision::Deny("user denied".into())
            } else {
                Decision::Allow
            }
        }
    }
    fn git(root: &Path, argv: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(root)
            .args(argv)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    async fn run_in(
        root: &Path,
        command: &str,
        authorization: Authorization,
        policy: Option<Arc<Policy>>,
        dialect: atomcode_capabilities::world::ShellDialect,
        tool_name: &'static str,
    ) -> ToolResult {
        let app = App::new(
            PluginRegistry::new(),
            ConfigTree::from_layers(Vec::new()).unwrap(),
        );
        let ctx = app.context();
        let tools = Arc::new(ToolBox::default());
        tools.register(Arc::new(Bash(tool_name))).unwrap();
        let _ = ctx.provide::<ToolsSvc>(tools).unwrap();
        let _ = ctx
            .provide::<ShellSvc>(Arc::new(NativeWorld(dialect)))
            .unwrap();
        if let Some(policy) = policy {
            let _ = ctx.provide::<ApprovalSvc>(policy).unwrap();
        }
        GitIndexPlugin.apply(&ctx, &Value::Null).await.unwrap();
        let mut exec = ToolExec {
            call: ToolCall {
                id: "call".into(),
                name: tool_name.into(),
                arguments: serde_json::json!({"command":command}).to_string(),
            },
            turn: 1,
            round: 1,
            authorization,
            working_dir: root.into(),
        };
        ctx.waterfall::<ToolsExecute, _>(&mut exec, |_| {
            Box::pin(async {
                ToolResult {
                    content: "executed".into(),
                    ..Default::default()
                }
            })
        })
        .await
    }
    struct NativeWorld(atomcode_capabilities::world::ShellDialect);
    #[async_trait]
    impl atomcode_capabilities::world::Shell for NativeWorld {
        fn describe(&self) -> String {
            "native test world without Bash".into()
        }
        fn dialect(&self) -> atomcode_capabilities::world::ShellDialect {
            self.0
        }
        async fn powershell(&self) -> Option<Arc<dyn atomcode_capabilities::world::Shell>> {
            Some(Arc::new(Self(
                atomcode_capabilities::world::ShellDialect::PowerShell,
            )))
        }
        async fn spawn(
            &self,
            _: &str,
            _: &atomcode_capabilities::world::SpawnOptions,
        ) -> Result<
            Arc<dyn atomcode_capabilities::world::Process>,
            atomcode_capabilities::world::SpawnError,
        > {
            panic!("Git inspection must not start a shell")
        }
        async fn spawn_program(
            &self,
            program: &str,
            arguments: &[String],
            options: &atomcode_capabilities::world::SpawnOptions,
        ) -> Result<
            Arc<dyn atomcode_capabilities::world::Process>,
            atomcode_capabilities::world::SpawnError,
        > {
            atomcode_capabilities::world::Shell::spawn_program(
                &atomcode_capabilities::world::LocalShell,
                program,
                arguments,
                options,
            )
            .await
        }
    }
    async fn run(
        root: &Path,
        command: &str,
        authorization: Authorization,
        policy: Option<Arc<Policy>>,
    ) -> ToolResult {
        run_in(
            root,
            command,
            authorization,
            policy,
            atomcode_capabilities::world::ShellDialect::Bash,
            "bash",
        )
        .await
    }
    #[tokio::test]
    async fn cmd_and_powershell_review_index_without_git_bash() {
        use atomcode_capabilities::world::ShellDialect;
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        std::fs::write(root.path().join("user.txt"), "original").unwrap();
        git(root.path(), &["add", "--", "user.txt"]);
        for (dialect, tool, command) in [
            (ShellDialect::Cmd, "bash", "git commit -m \"fix\""),
            (ShellDialect::Cmd, "bash_start", "git add -A"),
            (ShellDialect::Cmd, "pwsh", "& git commit -m 'fix'"),
            (ShellDialect::Cmd, "pwsh_start", "git add -A"),
        ] {
            let policy = Arc::new(Policy {
                seen: Mutex::new(vec![]),
                change: None,
                deny: false,
            });
            let result = run_in(
                root.path(),
                command,
                Authorization::No,
                Some(policy.clone()),
                dialect,
                tool,
            )
            .await;
            assert_eq!(result.content, "executed", "{tool}: {}", result.content);
            assert!(policy.seen.lock().unwrap()[0].contains("user.txt"));
        }
    }
    #[tokio::test]
    async fn fresh_index_review_cannot_inherit_a_blanket_or_earlier_authorization() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        std::fs::write(root.path().join("user.txt"), "original").unwrap();
        git(root.path(), &["add", "--", "user.txt"]);
        for auth in [
            Authorization::No,
            Authorization::Presumed,
            Authorization::ByPerson,
        ] {
            let policy = Arc::new(Policy {
                seen: Mutex::new(vec![]),
                change: None,
                deny: true,
            });
            let result = run(root.path(), "git commit -m fix", auth, Some(policy.clone())).await;
            assert!(result.is_error);
            assert!(policy.seen.lock().unwrap()[0].contains("user.txt"));
        }
        let policy = Arc::new(Policy {
            seen: Mutex::new(vec![]),
            change: Some(root.path().into()),
            deny: false,
        });
        let result = run(
            root.path(),
            "git commit -m fix",
            Authorization::No,
            Some(policy),
        )
        .await;
        assert!(result.is_error && result.content.contains("changed during review"));
        let result = run(root.path(), "git commit -m fix", Authorization::No, None).await;
        assert!(result.is_error && result.content.contains("approval policy"));
    }
    #[tokio::test]
    async fn explicit_stage_passes_and_an_unchanged_review_may_execute() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        std::fs::write(root.path().join("user.txt"), "original").unwrap();
        let policy = Arc::new(Policy {
            seen: Mutex::new(vec![]),
            change: None,
            deny: false,
        });
        let result = run(
            root.path(),
            "git add -- user.txt",
            Authorization::No,
            Some(policy.clone()),
        )
        .await;
        assert_eq!(result.content, "executed");
        assert!(policy.seen.lock().unwrap().is_empty());
        let result = run(
            root.path(),
            "git add -A",
            Authorization::No,
            Some(policy.clone()),
        )
        .await;
        assert_eq!(result.content, "executed");
        assert!(policy.seen.lock().unwrap()[0].contains("user.txt"));
        git(root.path(), &["add", "--", "user.txt"]);
        let result = run(
            root.path(),
            "git commit -m fix",
            Authorization::No,
            Some(policy),
        )
        .await;
        assert_eq!(result.content, "executed");
    }
}
