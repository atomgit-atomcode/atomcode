//! Agent-invocable AtomGit tools: `atomgit_repo` / `atomgit_pr` / `atomgit_issue`.
//! Each dispatches on an `action` field and calls [`AtomgitClient`]. `risk()` is
//! arg-aware: read actions are `Safe`, writes are `Risky`. The client (and its token
//! provider) is injected at construction — see [`register_atomgit_tools`].

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult, ToolRegistry};

use super::{err, ok};
use crate::atomgit::models::Repo;
use crate::atomgit::AtomgitClient;

/// Pull `action` out of the raw args without failing the whole parse — used by
/// `risk()`, which must classify before `execute` parses strictly.
fn action_of(args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(str::to_string))
}

// ─────────────────────────── atomgit_repo ───────────────────────────

#[derive(Deserialize)]
struct RepoArgs {
    action: String,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    private: Option<bool>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    dir: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    30
}

/// `atomgit_repo` tool. Holds the shared client.
pub struct AtomgitRepoTool {
    client: Arc<AtomgitClient>,
}

impl AtomgitRepoTool {
    pub fn new(client: Arc<AtomgitClient>) -> Self {
        Self { client }
    }
}

fn render_repo(r: &Repo) -> String {
    format!(
        "{} ({}){}\n  {}",
        if r.full_name.is_empty() { &r.name } else { &r.full_name },
        if r.private { "private" } else { "public" },
        if r.description.is_empty() { String::new() } else { format!("\n  {}", r.description) },
        r.html_url
    )
}

#[async_trait]
impl Tool for AtomgitRepoTool {
    fn name(&self) -> &str {
        "atomgit_repo"
    }
    fn description(&self) -> &str {
        "Operate on AtomGit repositories. action: \"list\" (your repos), \"view\" \
         (owner+repo), \"create\" (name; optional owner=org, description, private), \
         \"delete\" (owner+repo), \"fork\" (owner+repo; optional name, private), \
         \"clone\" (owner+repo; optional branch, dir — runs local `git clone`)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list","view","create","delete","fork","clone"] },
                "owner": { "type": "string", "description": "Repo owner (org for create). Omit on create for a personal repo." },
                "repo": { "type": "string", "description": "Repo name for view/delete/fork/clone." },
                "name": { "type": "string", "description": "New repo name (create) or fork target name." },
                "description": { "type": "string" },
                "private": { "type": "boolean" },
                "branch": { "type": "string", "description": "Branch to clone." },
                "dir": { "type": "string", "description": "Target dir for clone (relative to working dir)." },
                "limit": { "type": "integer", "description": "Max repos for list (default 30)." }
            },
            "required": ["action"]
        })
    }
    fn risk(&self, args: &str) -> RiskLevel {
        match action_of(args).as_deref() {
            Some("list") | Some("view") => RiskLevel::Safe,
            _ => RiskLevel::Risky,
        }
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: RepoArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("atomgit_repo: invalid arguments: {e}")),
        };
        match a.action.as_str() {
            "list" => match self.client.repo_list(a.limit).await {
                Ok(repos) if repos.is_empty() => ok("No repositories.".to_string()),
                Ok(repos) => ok(repos.iter().map(render_repo).collect::<Vec<_>>().join("\n\n")),
                Err(e) => err(e),
            },
            "view" => match (a.owner, a.repo) {
                (Some(o), Some(r)) => match self.client.repo_view(&o, &r).await {
                    Ok(repo) => ok(render_repo(&repo)),
                    Err(e) => err(e),
                },
                _ => err("atomgit_repo view: owner and repo are required".to_string()),
            },
            "create" => match a.name {
                Some(n) => match self
                    .client
                    .repo_create(a.owner.as_deref(), &n, a.description.as_deref().unwrap_or(""), a.private.unwrap_or(false))
                    .await
                {
                    Ok(repo) => ok(format!("Created {}", render_repo(&repo))),
                    Err(e) => err(e),
                },
                None => err("atomgit_repo create: name is required".to_string()),
            },
            "delete" => match (a.owner, a.repo) {
                (Some(o), Some(r)) => match self.client.repo_delete(&o, &r).await {
                    Ok(()) => ok(format!("Deleted {o}/{r}")),
                    Err(e) => err(e),
                },
                _ => err("atomgit_repo delete: owner and repo are required".to_string()),
            },
            "fork" => match (a.owner, a.repo) {
                (Some(o), Some(r)) => {
                    match self.client.repo_fork(&o, &r, a.name.as_deref(), a.private).await {
                        Ok(repo) => ok(format!("Forked to {}", render_repo(&repo))),
                        Err(e) => err(e),
                    }
                }
                _ => err("atomgit_repo fork: owner and repo are required".to_string()),
            },
            "clone" => match (a.owner, a.repo) {
                (Some(o), Some(r)) => clone_repo(&o, &r, a.branch.as_deref(), a.dir.as_deref(), ctx).await,
                _ => err("atomgit_repo clone: owner and repo are required".to_string()),
            },
            other => err(format!("atomgit_repo: unknown action {other:?}")),
        }
    }
}

/// Local `git clone https://atomgit.com/{owner}/{repo}.git [dir]`, run in the tool's
/// working dir. Not an API call. Stdout/stderr captured into the result.
async fn clone_repo(
    owner: &str,
    repo: &str,
    branch: Option<&str>,
    dir: Option<&str>,
    ctx: &ToolContext,
) -> ToolResult {
    let url = format!("https://atomgit.com/{owner}/{repo}.git");
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone");
    if let Some(b) = branch {
        cmd.arg("--branch").arg(b);
    }
    cmd.arg(&url);
    if let Some(d) = dir {
        cmd.arg(d);
    }
    cmd.current_dir(&ctx.working_dir).stdout(Stdio::piped()).stderr(Stdio::piped());
    match cmd.output().await {
        Ok(out) if out.status.success() => ok(format!("Cloned {owner}/{repo}")),
        Ok(out) => err(format!("git clone failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
        Err(e) => err(format!("failed to run git: {e}")),
    }
}

// register fn + pr/issue tools are added in later tasks.
pub fn register_atomgit_tools(reg: &mut ToolRegistry, client: Arc<AtomgitClient>) {
    reg.register(Arc::new(AtomgitRepoTool::new(client.clone())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomgit::testutil::StaticToken;
    use crate::atomgit::AtomgitConfig;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("."),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
        }
    }

    fn tool(server: &MockServer) -> AtomgitRepoTool {
        let client = AtomgitClient::new(AtomgitConfig {
            base_url: format!("{}/api/v5", server.uri()),
            user_agent: "atomcode/test".into(),
            token: Arc::new(StaticToken("t")),
        })
        .unwrap();
        AtomgitRepoTool::new(Arc::new(client))
    }

    #[test]
    fn risk_reads_are_safe_writes_risky() {
        let t = AtomgitRepoTool::new(Arc::new(
            AtomgitClient::new(AtomgitConfig {
                base_url: "http://x/api/v5".into(),
                user_agent: "u".into(),
                token: Arc::new(StaticToken("t")),
            })
            .unwrap(),
        ));
        assert_eq!(t.risk(r#"{"action":"list"}"#), RiskLevel::Safe);
        assert_eq!(t.risk(r#"{"action":"view"}"#), RiskLevel::Safe);
        assert_eq!(t.risk(r#"{"action":"create"}"#), RiskLevel::Risky);
        assert_eq!(t.risk(r#"{"action":"delete"}"#), RiskLevel::Risky);
        // malformed → fail safe to Risky
        assert_eq!(t.risk("not json"), RiskLevel::Risky);
    }

    #[tokio::test]
    async fn execute_list_renders() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/user/repos"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"name":"a","full_name":"me/a","html_url":"https://atomgit.com/me/a","private":false}
            ])))
            .mount(&server)
            .await;
        let r = tool(&server).execute(r#"{"action":"list"}"#, &ctx()).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("me/a"), "{}", r.content);
    }

    #[tokio::test]
    async fn execute_view_requires_owner_repo() {
        let server = MockServer::start().await;
        let r = tool(&server).execute(r#"{"action":"view","owner":"o"}"#, &ctx()).await;
        assert!(r.is_error);
        assert!(r.content.contains("owner and repo are required"), "{}", r.content);
    }

    #[tokio::test]
    async fn execute_unknown_action_errors() {
        let server = MockServer::start().await;
        let r = tool(&server).execute(r#"{"action":"frobnicate"}"#, &ctx()).await;
        assert!(r.is_error);
        assert!(r.content.contains("unknown action"), "{}", r.content);
    }
}
