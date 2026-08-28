//! Agent-invocable AtomGit tools: `atomgit_repo` / `atomgit_pr` / `atomgit_issue`.
//! Each dispatches on an `action` field and calls [`AtomgitClient`]. `risk()` is
//! arg-aware: read actions are `Safe`, writes are `Risky`. The client (and its token
//! provider) is injected at construction — see [`register_atomgit_tools`].

use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolRegistry, ToolResult};

use super::{err, ok};
use crate::atomgit::models::{Comment, CreatedComment, Issue, PullRequest, Repo, Tag};
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
    #[serde(default)]
    tag_name: Option<String>,
    #[serde(default)]
    refs: Option<String>,
    #[serde(default)]
    tag_message: Option<String>,
    #[serde(default)]
    label: Option<String>,
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
        if r.full_name.is_empty() {
            &r.name
        } else {
            &r.full_name
        },
        if r.private { "private" } else { "public" },
        if r.description.is_empty() {
            String::new()
        } else {
            format!("\n  {}", r.description)
        },
        r.html_url
    )
}

/// Render a created tag, falling back to the requested name when the response omits it.
fn render_tag(t: &Tag, requested: &str) -> String {
    let name = if t.tag_name.is_empty() {
        requested
    } else {
        &t.tag_name
    };
    if t.message.is_empty() {
        format!("Created tag {name}")
    } else {
        format!("Created tag {name}: {}", t.message)
    }
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
         \"clone\" (owner+repo; optional branch, dir — runs local `git clone`), \
         \"create_tag\" (owner+repo+tag_name; optional refs=start point (default main), \
         tag_message), \"labels\" (owner+repo — read project labels), \"ensure_label\" \
         (owner+repo; optional label, default atomcode)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["list","view","create","delete","fork","clone","create_tag","labels","ensure_label"] },
                "owner": { "type": "string", "description": "Repo owner (org for create). Omit on create for a personal repo." },
                "repo": { "type": "string", "description": "Repo name for view/delete/fork/clone." },
                "name": { "type": "string", "description": "New repo name (create) or fork target name." },
                "description": { "type": "string" },
                "private": { "type": "boolean" },
                "branch": { "type": "string", "description": "Branch to clone." },
                "dir": { "type": "string", "description": "Target dir for clone (relative to working dir)." },
                "tag_name": { "type": "string", "description": "New tag name (create_tag)." },
                "refs": { "type": "string", "description": "Start point for create_tag — branch/commit/tag (default main)." },
                "tag_message": { "type": "string", "description": "Tag description (create_tag, optional)." },
                "label": { "type": "string", "description": "Label for ensure_label (default \"atomcode\")." },
                "limit": { "type": "integer", "description": "Max repos for list (default 30)." }
            },
            "required": ["action"]
        })
    }
    fn risk(&self, args: &str) -> RiskLevel {
        match action_of(args).as_deref() {
            Some("list") | Some("view") | Some("labels") => RiskLevel::Safe,
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
                Ok(repos) => ok(repos
                    .iter()
                    .map(render_repo)
                    .collect::<Vec<_>>()
                    .join("\n\n")),
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
                    .repo_create(
                        a.owner.as_deref(),
                        &n,
                        a.description.as_deref().unwrap_or(""),
                        a.private.unwrap_or(false),
                    )
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
                    match self
                        .client
                        .repo_fork(&o, &r, a.name.as_deref(), a.private)
                        .await
                    {
                        Ok(repo) => ok(format!("Forked to {}", render_repo(&repo))),
                        Err(e) => err(e),
                    }
                }
                _ => err("atomgit_repo fork: owner and repo are required".to_string()),
            },
            "clone" => match (a.owner, a.repo) {
                (Some(o), Some(r)) => {
                    clone_repo(&o, &r, a.branch.as_deref(), a.dir.as_deref(), ctx).await
                }
                _ => err("atomgit_repo clone: owner and repo are required".to_string()),
            },
            "create_tag" => match (a.owner, a.repo, a.tag_name) {
                (Some(o), Some(r), Some(tn)) => {
                    let refs = a.refs.as_deref().unwrap_or("main");
                    let msg = a.tag_message.as_deref().unwrap_or("");
                    match self.client.repo_create_tag(&o, &r, &tn, refs, msg).await {
                        Ok(tag) => ok(render_tag(&tag, &tn)),
                        Err(e) => err(e),
                    }
                }
                _ => err(
                    "atomgit_repo create_tag: owner, repo and tag_name are required".to_string(),
                ),
            },
            "labels" => match (a.owner, a.repo) {
                (Some(o), Some(r)) => match self.client.repo_labels(&o, &r).await {
                    Ok(None) => ok("(project_labels not reported by this repo)".to_string()),
                    Ok(Some(labels)) if labels.is_empty() => ok("(no labels)".to_string()),
                    Ok(Some(labels)) => ok(labels.join(", ")),
                    Err(e) => err(e),
                },
                _ => err("atomgit_repo labels: owner and repo are required".to_string()),
            },
            "ensure_label" => match (a.owner, a.repo) {
                (Some(o), Some(r)) => {
                    let label = a.label.as_deref().unwrap_or("atomcode");
                    match self.client.repo_ensure_label(&o, &r, label).await {
                        Ok(true) => ok(format!("Added label {label:?} to {o}/{r}")),
                        Ok(false) => ok(format!("Label {label:?} already present on {o}/{r}")),
                        Err(e) => err(e),
                    }
                }
                _ => err("atomgit_repo ensure_label: owner and repo are required".to_string()),
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
    // Defense-in-depth: this runs with host authority and the args are model-
    // controlled. git parses options even after positionals, so a value starting
    // with '-' (e.g. dir="--upload-pack=...") would be taken as a git OPTION, not a
    // path/ref. Reject leading-dash values and pass `--` before the positional URL.
    // Mirrors the leading-`-` guard the plugin installer uses on git inputs.
    for (label, val) in [
        ("owner", Some(owner)),
        ("repo", Some(repo)),
        ("branch", branch),
        ("dir", dir),
    ] {
        if let Some(v) = val {
            if v.starts_with('-') {
                return err(format!(
                    "atomgit_repo clone: {label} must not start with '-'"
                ));
            }
        }
    }
    let url = format!("https://atomgit.com/{owner}/{repo}.git");
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone");
    if let Some(b) = branch {
        cmd.arg("--branch").arg(b);
    }
    cmd.arg("--").arg(&url);
    if let Some(d) = dir {
        cmd.arg(d);
    }
    cmd.current_dir(&ctx.working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // No console-window flash for git when spawned from a console-less daemon (Windows-only).
    crate::process_utils::suppress_console_window(&mut cmd);
    match cmd.output().await {
        Ok(out) if out.status.success() => ok(format!("Cloned {owner}/{repo}")),
        Ok(out) => err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => err(format!("failed to run git: {e}")),
    }
}

// ─────────────────────────── atomgit_issue ───────────────────────────

#[derive(Deserialize)]
struct IssueArgs {
    action: String,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    comment_id: Option<u64>,
    #[serde(default = "default_state")]
    state: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

/// `atomgit_issue` tool.
pub struct AtomgitIssueTool {
    client: Arc<AtomgitClient>,
}
impl AtomgitIssueTool {
    pub fn new(client: Arc<AtomgitClient>) -> Self {
        Self { client }
    }
}

fn render_issue(i: &Issue) -> String {
    format!("#{} [{}] {}\n  {}", i.number, i.state, i.title, i.html_url)
}

#[async_trait]
impl Tool for AtomgitIssueTool {
    fn name(&self) -> &str {
        "atomgit_issue"
    }
    fn description(&self) -> &str {
        "Operate on AtomGit issues. action: \"list\" (owner+repo; optional \
         state=open|closed|all, limit), \"view\" (owner+repo+number), \"create\" \
         (owner+repo+title; optional body), \"comment_create\"/\"comment_view\" \
         (owner+repo+number; body for create), \"comment_edit\"/\"comment_delete\" \
         (owner+repo+comment_id; body for edit)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": [
                    "list","view","create",
                    "comment_create","comment_view","comment_edit","comment_delete"
                ]},
                "owner": { "type": "string" },
                "repo": { "type": "string" },
                "number": { "type": "integer", "description": "Issue number." },
                "title": { "type": "string" },
                "body": { "type": "string" },
                "comment_id": { "type": "integer", "description": "Comment id for comment_edit/comment_delete." },
                "state": { "type": "string", "description": "list filter (default open)." },
                "limit": { "type": "integer", "description": "Max for list (default 30)." }
            },
            "required": ["action"]
        })
    }
    fn risk(&self, args: &str) -> RiskLevel {
        match action_of(args).as_deref() {
            Some("list") | Some("view") | Some("comment_view") => RiskLevel::Safe,
            _ => RiskLevel::Risky,
        }
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: IssueArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("atomgit_issue: invalid arguments: {e}")),
        };
        let c = &self.client;
        match a.action.as_str() {
            "list" => match (a.owner, a.repo) {
                (Some(o), Some(r)) => match c.issue_list(&o, &r, &a.state, a.limit).await {
                    Ok(is) if is.is_empty() => ok("No issues.".to_string()),
                    Ok(is) => ok(is.iter().map(render_issue).collect::<Vec<_>>().join("\n\n")),
                    Err(e) => err(e),
                },
                _ => err("atomgit_issue list: owner and repo are required".to_string()),
            },
            "view" => match need_owner_repo_number(a.owner, a.repo, a.number, "atomgit_issue view")
            {
                Ok((o, r, n)) => match c.issue_view(&o, &r, n).await {
                    Ok(i) => ok(render_issue(&i)),
                    Err(e) => err(e),
                },
                Err(e) => e,
            },
            "create" => match (a.owner, a.repo, a.title) {
                (Some(o), Some(r), Some(t)) => match c
                    .issue_create(&o, &r, &t, a.body.as_deref().unwrap_or(""))
                    .await
                {
                    Ok(i) => ok(format!("Created {}", render_issue(&i))),
                    Err(e) => err(e),
                },
                _ => err("atomgit_issue create: owner, repo and title are required".to_string()),
            },
            "comment_create" => match need_owner_repo_number(
                a.owner,
                a.repo,
                a.number,
                "atomgit_issue comment_create",
            ) {
                Ok((o, r, n)) => match a.body {
                    Some(b) => match c.issue_comment_create(&o, &r, n, &b).await {
                        Ok(cm) => ok(format!("Comment {} created", cm.id)),
                        Err(e) => err(e),
                    },
                    None => err("atomgit_issue comment_create: body is required".to_string()),
                },
                Err(e) => e,
            },
            "comment_view" => match need_owner_repo_number(
                a.owner,
                a.repo,
                a.number,
                "atomgit_issue comment_view",
            ) {
                Ok((o, r, n)) => match c.issue_comment_view(&o, &r, n).await {
                    Ok(cs) => ok(render_comments(&cs)),
                    Err(e) => err(e),
                },
                Err(e) => e,
            },
            "comment_edit" => match (a.owner, a.repo, a.comment_id, a.body) {
                (Some(o), Some(r), Some(id), Some(b)) => {
                    match c.issue_comment_edit(&o, &r, id, &b).await {
                        Ok(cm) => ok(format!("Edited comment {}", cm.id)),
                        Err(e) => err(e),
                    }
                }
                _ => err(
                    "atomgit_issue comment_edit: owner, repo, comment_id and body are required"
                        .to_string(),
                ),
            },
            "comment_delete" => match (a.owner, a.repo, a.comment_id) {
                (Some(o), Some(r), Some(id)) => match c.issue_comment_delete(&o, &r, id).await {
                    Ok(()) => ok(format!("Deleted comment {id}")),
                    Err(e) => err(e),
                },
                _ => err(
                    "atomgit_issue comment_delete: owner, repo and comment_id are required"
                        .to_string(),
                ),
            },
            other => err(format!("atomgit_issue: unknown action {other:?}")),
        }
    }
}

// ─────────────────────────── atomgit_api ───────────────────────────

#[derive(Deserialize)]
struct ApiArgs {
    method: String,
    path: String,
    #[serde(default)]
    query: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default)]
    body: Option<serde_json::Value>,
}

/// Pull `method` out of the raw args for `risk()` (which classifies before
/// `execute` parses strictly).
fn method_of(args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_string))
}

/// The AtomGit REST verbs `atomgit_api` accepts (case-insensitive).
fn parse_api_method(raw: &str) -> Option<reqwest::Method> {
    match raw.to_ascii_uppercase().as_str() {
        "GET" => Some(reqwest::Method::GET),
        "POST" => Some(reqwest::Method::POST),
        "PATCH" => Some(reqwest::Method::PATCH),
        "PUT" => Some(reqwest::Method::PUT),
        "DELETE" => Some(reqwest::Method::DELETE),
        _ => None,
    }
}

/// Reject anything that isn't a plain path under the fixed API base, so the
/// model can never redirect the request to another host (absolute or
/// protocol-relative URLs) through the `path` field.
fn validate_api_path(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        Err("path must start with '/' (e.g. \"/user/orgs\")".into())
    } else if path.starts_with("//") {
        Err("path must not start with '//' (a protocol-relative host)".into())
    } else if path.contains("://") {
        Err("path must be relative to the AtomGit API base, not an absolute URL".into())
    } else {
        Ok(())
    }
}

/// `atomgit_api` — credential-safe passthrough for AtomGit REST endpoints that
/// have no dedicated typed tool. The bearer token is injected by the client, so
/// it never appears in model-visible arguments (unlike a raw `bash`/`curl` call,
/// where the model would have to pass the credential through the shell itself —
/// which `CredentialBashGate` blocks).
pub struct AtomgitApiTool {
    client: Arc<AtomgitClient>,
}

impl AtomgitApiTool {
    pub fn new(client: Arc<AtomgitClient>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Tool for AtomgitApiTool {
    fn name(&self) -> &str {
        "atomgit_api"
    }
    fn description(&self) -> &str {
        "Call any AtomGit REST API endpoint that has no dedicated tool (prefer \
         atomgit_repo / atomgit_pr / atomgit_issue when they cover the action). \
         The auth token is attached server-side — NEVER put credentials in the \
         arguments. `path` is relative to the API base, e.g. \"/user/orgs\" or \
         \"/repos/{owner}/{repo}/branches\". Returns the raw JSON response."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "enum": ["GET","POST","PATCH","PUT","DELETE"] },
                "path": { "type": "string", "description": "API path relative to the AtomGit base, starting with '/'. Do NOT include the host or a token." },
                "query": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Optional query-string parameters." },
                "body": { "type": "object", "description": "Optional JSON request body (POST/PATCH/PUT/DELETE)." }
            },
            "required": ["method", "path"]
        })
    }
    fn risk(&self, args: &str) -> RiskLevel {
        // Reads are Safe; anything that could mutate is Risky (needs approval).
        match method_of(args)
            .as_deref()
            .map(str::to_ascii_uppercase)
            .as_deref()
        {
            Some("GET") => RiskLevel::Safe,
            _ => RiskLevel::Risky,
        }
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: ApiArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("atomgit_api: invalid arguments: {e}")),
        };
        let Some(method) = parse_api_method(&a.method) else {
            return err(format!(
                "atomgit_api: unsupported method {:?} (use GET/POST/PATCH/PUT/DELETE)",
                a.method
            ));
        };
        if let Err(e) = validate_api_path(&a.path) {
            return err(format!("atomgit_api: {e}"));
        }
        let query: Vec<(String, String)> = a.query.unwrap_or_default().into_iter().collect();
        match self
            .client
            .request_raw(method, &a.path, &query, a.body.as_ref())
            .await
        {
            Ok(body) if body.trim().is_empty() => ok("(empty response)".to_string()),
            Ok(body) => ok(body),
            Err(e) => err(e),
        }
    }
}

/// Register `atomgit_repo` / `atomgit_pr` / `atomgit_issue` / `atomgit_api` into
/// `reg`, all sharing one client. The embedder builds the [`AtomgitClient`] with its
/// own [`TokenProvider`](crate::atomgit::TokenProvider) and then `mount`s whichever
/// tool names it wants to expose.
pub fn register_atomgit_tools(reg: &mut ToolRegistry, client: Arc<AtomgitClient>) {
    reg.register(Arc::new(AtomgitRepoTool::new(client.clone())));
    reg.register(Arc::new(AtomgitPrTool::new(client.clone())));
    reg.register(Arc::new(AtomgitIssueTool::new(client.clone())));
    reg.register(Arc::new(AtomgitApiTool::new(client)));
}

/// The tool names registered by [`register_atomgit_tools`], for `mount`.
///
/// MUST stay in lockstep with `register_atomgit_tools` above — the embedder mounts
/// exactly these names, so omitting one registers a tool the model can never call.
/// `atomgit_api` in particular is where the bash gate steers the model after denying
/// raw curl/REST calls; if it isn't exposed the model hits a dead end.
pub fn atomgit_tool_names() -> &'static [&'static str] {
    &["atomgit_repo", "atomgit_pr", "atomgit_issue", "atomgit_api"]
}

// ─────────────────────────── atomgit_pr ───────────────────────────

#[derive(Deserialize)]
struct PrArgs {
    action: String,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    head: Option<String>,
    #[serde(default)]
    comment_id: Option<u64>,
    #[serde(default)]
    parent_id: Option<u64>,
    #[serde(default)]
    issues: Vec<u64>,
    #[serde(default = "default_state")]
    state: String,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_state() -> String {
    "open".to_string()
}

/// `atomgit_pr` tool.
pub struct AtomgitPrTool {
    client: Arc<AtomgitClient>,
}
impl AtomgitPrTool {
    pub fn new(client: Arc<AtomgitClient>) -> Self {
        Self { client }
    }
}

fn render_pr(p: &PullRequest) -> String {
    format!(
        "#{} [{}] {}\n  {} ← {}\n  {}",
        p.number, p.state, p.title, p.base.ref_, p.head.ref_, p.html_url
    )
}
fn render_comments(cs: &[Comment]) -> String {
    if cs.is_empty() {
        return "No comments.".to_string();
    }
    cs.iter()
        .map(|c| format!("[{}] @{}: {}", c.id, c.user.login, c.body))
        .collect::<Vec<_>>()
        .join("\n")
}
fn render_created_comment(c: &CreatedComment) -> String {
    format!("Comment {} created: {}", c.id, c.html_url)
}

/// Owner+repo+number are required by most pr actions; this extracts them with a
/// uniform error.
fn need_owner_repo_number(
    owner: Option<String>,
    repo: Option<String>,
    number: Option<u64>,
    what: &str,
) -> Result<(String, String, u64), ToolResult> {
    match (owner, repo, number) {
        (Some(o), Some(r), Some(n)) => Ok((o, r, n)),
        _ => Err(err(format!("{what}: owner, repo and number are required"))),
    }
}

#[async_trait]
impl Tool for AtomgitPrTool {
    fn name(&self) -> &str {
        "atomgit_pr"
    }
    fn description(&self) -> &str {
        "Operate on AtomGit pull requests. action: \"list\" (owner+repo; optional \
         state=open|closed|all, limit), \"view\" (owner+repo+number), \"create\" \
         (owner+repo+title+head+base; optional body), \"close\" (owner+repo+number), \
         \"comment_create\"/\"comment_view\" (owner+repo+number; body for create), \
         \"comment_edit\"/\"comment_delete\" (owner+repo+comment_id; body for edit), \
         \"comment_reply\" (owner+repo+number+parent_id+body), \"link_issues\"/\
         \"unlink_issues\" (owner+repo+number+issues=[numbers])."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": [
                    "list","view","create","close",
                    "comment_create","comment_view","comment_edit","comment_delete","comment_reply",
                    "link_issues","unlink_issues"
                ]},
                "owner": { "type": "string" },
                "repo": { "type": "string" },
                "number": { "type": "integer", "description": "PR number." },
                "title": { "type": "string" },
                "body": { "type": "string" },
                "base": { "type": "string", "description": "Base branch (create)." },
                "head": { "type": "string", "description": "Head branch (create), e.g. \"owner/repo:branch\"." },
                "comment_id": { "type": "integer", "description": "Comment id for comment_edit/comment_delete." },
                "parent_id": { "type": "integer", "description": "Parent comment id for comment_reply." },
                "issues": { "type": "array", "items": { "type": "integer" }, "description": "Issue numbers for link/unlink." },
                "state": { "type": "string", "description": "list filter (default open)." },
                "limit": { "type": "integer", "description": "Max for list (default 30)." }
            },
            "required": ["action"]
        })
    }
    fn risk(&self, args: &str) -> RiskLevel {
        match action_of(args).as_deref() {
            Some("list") | Some("view") | Some("comment_view") => RiskLevel::Safe,
            _ => RiskLevel::Risky,
        }
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: PrArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("atomgit_pr: invalid arguments: {e}")),
        };
        let c = &self.client;
        match a.action.as_str() {
            "list" => match (a.owner, a.repo) {
                (Some(o), Some(r)) => match c.pr_list(&o, &r, &a.state, a.limit).await {
                    Ok(prs) if prs.is_empty() => ok("No pull requests.".to_string()),
                    Ok(prs) => ok(prs.iter().map(render_pr).collect::<Vec<_>>().join("\n\n")),
                    Err(e) => err(e),
                },
                _ => err("atomgit_pr list: owner and repo are required".to_string()),
            },
            "view" => match need_owner_repo_number(a.owner, a.repo, a.number, "atomgit_pr view") {
                Ok((o, r, n)) => match c.pr_view(&o, &r, n).await {
                    Ok(pr) => ok(render_pr(&pr)),
                    Err(e) => err(e),
                },
                Err(e) => e,
            },
            "create" => match (a.owner, a.repo, a.title, a.head) {
                (Some(o), Some(r), Some(t), Some(h)) => {
                    let base = a.base.as_deref().unwrap_or("master");
                    match c.pr_create(&o, &r, &t, a.body.as_deref().unwrap_or(""), base, &h).await {
                        Ok(pr) => ok(format!("Created {}", render_pr(&pr))),
                        Err(e) => err(e),
                    }
                }
                _ => err("atomgit_pr create: owner, repo, title and head are required".to_string()),
            },
            "close" => match need_owner_repo_number(a.owner, a.repo, a.number, "atomgit_pr close") {
                Ok((o, r, n)) => match c.pr_close(&o, &r, n).await {
                    Ok(pr) => ok(format!("Closed {}", render_pr(&pr))),
                    Err(e) => err(e),
                },
                Err(e) => e,
            },
            "comment_create" => match need_owner_repo_number(a.owner, a.repo, a.number, "atomgit_pr comment_create") {
                Ok((o, r, n)) => match a.body {
                    Some(b) => match c.pr_comment_create(&o, &r, n, &b).await {
                        Ok(cc) => ok(render_created_comment(&cc)),
                        Err(e) => err(e),
                    },
                    None => err("atomgit_pr comment_create: body is required".to_string()),
                },
                Err(e) => e,
            },
            "comment_view" => match need_owner_repo_number(a.owner, a.repo, a.number, "atomgit_pr comment_view") {
                Ok((o, r, n)) => match c.pr_comment_view(&o, &r, n).await {
                    Ok(cs) => ok(render_comments(&cs)),
                    Err(e) => err(e),
                },
                Err(e) => e,
            },
            "comment_edit" => match (a.owner, a.repo, a.comment_id, a.body) {
                (Some(o), Some(r), Some(id), Some(b)) => match c.pr_comment_edit(&o, &r, id, &b).await {
                    Ok(cm) => ok(format!("Edited comment {}", cm.id)),
                    Err(e) => err(e),
                },
                _ => err("atomgit_pr comment_edit: owner, repo, comment_id and body are required".to_string()),
            },
            "comment_delete" => match (a.owner, a.repo, a.comment_id) {
                (Some(o), Some(r), Some(id)) => match c.pr_comment_delete(&o, &r, id).await {
                    Ok(()) => ok(format!("Deleted comment {id}")),
                    Err(e) => err(e),
                },
                _ => err("atomgit_pr comment_delete: owner, repo and comment_id are required".to_string()),
            },
            "comment_reply" => match (a.owner, a.repo, a.number, a.parent_id, a.body) {
                (Some(o), Some(r), Some(n), Some(pid), Some(b)) => match c.pr_comment_reply(&o, &r, n, pid, &b).await {
                    Ok(cm) => ok(format!("Replied (comment {})", cm.id)),
                    Err(e) => err(e),
                },
                _ => err("atomgit_pr comment_reply: owner, repo, number, parent_id and body are required".to_string()),
            },
            "link_issues" => match need_owner_repo_number(a.owner, a.repo, a.number, "atomgit_pr link_issues") {
                Ok((o, r, n)) if !a.issues.is_empty() => match c.pr_link_issues(&o, &r, n, &a.issues).await {
                    Ok(()) => ok(format!("Linked issues {:?} to PR #{n}", a.issues)),
                    Err(e) => err(e),
                },
                Ok(_) => err("atomgit_pr link_issues: issues=[...] is required".to_string()),
                Err(e) => e,
            },
            "unlink_issues" => match need_owner_repo_number(a.owner, a.repo, a.number, "atomgit_pr unlink_issues") {
                Ok((o, r, n)) if !a.issues.is_empty() => match c.pr_unlink_issues(&o, &r, n, &a.issues).await {
                    Ok(()) => ok(format!("Unlinked issues {:?} from PR #{n}", a.issues)),
                    Err(e) => err(e),
                },
                Ok(_) => err("atomgit_pr unlink_issues: issues=[...] is required".to_string()),
                Err(e) => e,
            },
            other => err(format!("atomgit_pr: unknown action {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomgit::testutil::StaticToken;
    use crate::atomgit::AtomgitConfig;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("."),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
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
        assert_eq!(t.risk(r#"{"action":"fork"}"#), RiskLevel::Risky);
        assert_eq!(t.risk(r#"{"action":"clone"}"#), RiskLevel::Risky);
        assert_eq!(t.risk(r#"{"action":"create_tag"}"#), RiskLevel::Risky);
        // malformed → fail safe to Risky
        assert_eq!(t.risk("not json"), RiskLevel::Risky);
    }

    #[tokio::test]
    async fn execute_create_tag_renders() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/repos/o/r/tags"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"tag_name":"v1.0.0"})))
            .mount(&server)
            .await;
        let r = tool(&server)
            .execute(
                r#"{"action":"create_tag","owner":"o","repo":"r","tag_name":"v1.0.0"}"#,
                &ctx(),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("Created tag v1.0.0"), "{}", r.content);
    }

    #[tokio::test]
    async fn execute_create_tag_requires_tag_name() {
        let server = MockServer::start().await;
        let r = tool(&server)
            .execute(r#"{"action":"create_tag","owner":"o","repo":"r"}"#, &ctx())
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("tag_name are required"), "{}", r.content);
    }

    fn api_tool(server: &MockServer) -> AtomgitApiTool {
        let client = AtomgitClient::new(AtomgitConfig {
            base_url: format!("{}/api/v5", server.uri()),
            user_agent: "atomcode/test".into(),
            token: Arc::new(StaticToken("t")),
        })
        .unwrap();
        AtomgitApiTool::new(Arc::new(client))
    }

    #[test]
    fn atomgit_api_risk_get_safe_others_risky() {
        let t = AtomgitApiTool::new(Arc::new(
            AtomgitClient::new(AtomgitConfig {
                base_url: "http://x/api/v5".into(),
                user_agent: "u".into(),
                token: Arc::new(StaticToken("t")),
            })
            .unwrap(),
        ));
        assert_eq!(
            t.risk(r#"{"method":"GET","path":"/user"}"#),
            RiskLevel::Safe
        );
        assert_eq!(
            t.risk(r#"{"method":"get","path":"/user"}"#),
            RiskLevel::Safe
        );
        assert_eq!(
            t.risk(r#"{"method":"POST","path":"/gists"}"#),
            RiskLevel::Risky
        );
        assert_eq!(
            t.risk(r#"{"method":"DELETE","path":"/x"}"#),
            RiskLevel::Risky
        );
        // malformed → fail safe to Risky
        assert_eq!(t.risk("not json"), RiskLevel::Risky);
    }

    #[tokio::test]
    async fn atomgit_api_rejects_host_redirecting_paths() {
        let server = MockServer::start().await;
        let t = api_tool(&server);
        for bad in [
            r#"{"method":"GET","path":"https://evil.test/x"}"#,
            r#"{"method":"GET","path":"//evil.test/x"}"#,
            r#"{"method":"GET","path":"user/orgs"}"#,
        ] {
            let r = t.execute(bad, &ctx()).await;
            assert!(r.is_error, "must reject non-relative path: {bad}");
        }
    }

    #[tokio::test]
    async fn atomgit_api_get_returns_raw_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v5/user/orgs"))
            .and(header("authorization", "Bearer t"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"[{"login":"acme"}]"#))
            .mount(&server)
            .await;
        let r = api_tool(&server)
            .execute(r#"{"method":"GET","path":"/user/orgs"}"#, &ctx())
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("acme"), "{}", r.content);
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
        let r = tool(&server)
            .execute(r#"{"action":"view","owner":"o"}"#, &ctx())
            .await;
        assert!(r.is_error);
        assert!(
            r.content.contains("owner and repo are required"),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn execute_unknown_action_errors() {
        let server = MockServer::start().await;
        let r = tool(&server)
            .execute(r#"{"action":"frobnicate"}"#, &ctx())
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("unknown action"), "{}", r.content);
    }

    fn pr_tool(server: &MockServer) -> AtomgitPrTool {
        let client = AtomgitClient::new(AtomgitConfig {
            base_url: format!("{}/api/v5", server.uri()),
            user_agent: "atomcode/test".into(),
            token: Arc::new(StaticToken("t")),
        })
        .unwrap();
        AtomgitPrTool::new(Arc::new(client))
    }

    #[test]
    fn pr_risk_classification() {
        let server_url = "http://x/api/v5".to_string();
        let t = AtomgitPrTool::new(Arc::new(
            AtomgitClient::new(AtomgitConfig {
                base_url: server_url,
                user_agent: "u".into(),
                token: Arc::new(StaticToken("t")),
            })
            .unwrap(),
        ));
        assert_eq!(t.risk(r#"{"action":"list"}"#), RiskLevel::Safe);
        assert_eq!(t.risk(r#"{"action":"comment_view"}"#), RiskLevel::Safe);
        assert_eq!(t.risk(r#"{"action":"create"}"#), RiskLevel::Risky);
        assert_eq!(t.risk(r#"{"action":"comment_delete"}"#), RiskLevel::Risky);
        assert_eq!(t.risk(r#"{"action":"link_issues"}"#), RiskLevel::Risky);
    }

    #[tokio::test]
    async fn pr_create_requires_head() {
        let server = MockServer::start().await;
        let r = pr_tool(&server)
            .execute(
                r#"{"action":"create","owner":"o","repo":"r","title":"T"}"#,
                &ctx(),
            )
            .await;
        assert!(r.is_error);
        assert!(
            r.content.contains("title and head are required")
                || r.content.contains("head are required"),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn pr_close_renders() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/api/v5/repos/o/r/pulls/5"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"number":5,"state":"closed","title":"x"})),
            )
            .mount(&server)
            .await;
        let r = pr_tool(&server)
            .execute(
                r#"{"action":"close","owner":"o","repo":"r","number":5}"#,
                &ctx(),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("Closed"), "{}", r.content);
    }

    fn issue_tool(server: &MockServer) -> AtomgitIssueTool {
        let client = AtomgitClient::new(AtomgitConfig {
            base_url: format!("{}/api/v5", server.uri()),
            user_agent: "atomcode/test".into(),
            token: Arc::new(StaticToken("t")),
        })
        .unwrap();
        AtomgitIssueTool::new(Arc::new(client))
    }

    #[tokio::test]
    async fn issue_create_renders() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v5/repos/o/r/issues"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({"number":4,"title":"T","state":"open"})),
            )
            .mount(&server)
            .await;
        let r = issue_tool(&server)
            .execute(
                r#"{"action":"create","owner":"o","repo":"r","title":"T"}"#,
                &ctx(),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("#4"), "{}", r.content);
    }

    #[test]
    fn register_mounts_all_four() {
        let client = AtomgitClient::new(AtomgitConfig {
            base_url: "http://x/api/v5".into(),
            user_agent: "u".into(),
            token: Arc::new(StaticToken("t")),
        })
        .unwrap();
        let mut reg = ToolRegistry::new();
        register_atomgit_tools(&mut reg, Arc::new(client));
        let mounted = reg.mount(atomgit_tool_names());
        let names: Vec<String> = mounted.defs().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"atomgit_repo".to_string()));
        assert!(names.contains(&"atomgit_pr".to_string()));
        assert!(names.contains(&"atomgit_issue".to_string()));
        // Regression: `atomgit_api` was registered but missing from
        // `atomgit_tool_names()`, so it was never mounted — while the bash gate
        // steered the model to it. Every registered tool must be exposed.
        assert!(
            names.contains(&"atomgit_api".to_string()),
            "atomgit_api must be mounted (bash gate steers the model to it): {names:?}"
        );
        assert_eq!(
            names.len(),
            atomgit_tool_names().len(),
            "every name in atomgit_tool_names() must resolve to a registered tool: {names:?}"
        );
    }

    #[tokio::test]
    async fn clone_rejects_leading_dash_arg() {
        let server = MockServer::start().await;
        let r = tool(&server)
            .execute(
                r#"{"action":"clone","owner":"o","repo":"r","dir":"--upload-pack=evil"}"#,
                &ctx(),
            )
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(
            r.content.contains("must not start with '-'"),
            "{}",
            r.content
        );
    }
}

#[cfg(test)]
mod label_action_tests {
    use super::*;
    use crate::atomgit::TokenProvider;
    use atomcode_kernel::tool::RiskLevel;

    struct FixedTok(String);
    impl TokenProvider for FixedTok {
        fn token(&self) -> Result<String, String> {
            Ok(self.0.clone())
        }
    }

    fn tool() -> AtomgitRepoTool {
        // Build a client whose token/base are irrelevant for risk() (no network).
        let client = std::sync::Arc::new(
            crate::atomgit::AtomgitClient::new(crate::atomgit::AtomgitConfig {
                base_url: "https://api.atomgit.com/api/v5".into(),
                user_agent: "t".into(),
                token: std::sync::Arc::new(FixedTok("t".into())),
            })
            .unwrap(),
        );
        AtomgitRepoTool::new(client)
    }

    #[test]
    fn labels_is_safe_ensure_label_and_delete_are_risky() {
        let t = tool();
        assert_eq!(t.risk(r#"{"action":"labels"}"#), RiskLevel::Safe);
        assert_eq!(t.risk(r#"{"action":"ensure_label"}"#), RiskLevel::Risky);
        assert_eq!(t.risk(r#"{"action":"delete"}"#), RiskLevel::Risky);
    }
}
