//! Claude-Code-compatible EXTERNAL hooks for the new (kernel) stack.
//!
//! The legacy `atomcode-core` engine shipped a full CC-parity hook engine
//! (spawn an external command per lifecycle event, speak CC's stdin/stdout JSON
//! contract). The new default v2/kernel engine never ran it — it lives in core,
//! which L1 may not depend on. This module PORTS that executor onto the kernel's
//! seams ([`LifecycleHooks`] + [`ToolMiddleware`]) so user/plugin CC hooks run on
//! the default engine again.
//!
//! ONE [`CCExternalHooks`] instance implements BOTH traits — register it as a
//! lifecycle hook AND as a tool middleware (it carries the same loaded config):
//! - [`LifecycleHooks`]: `session_start` / `session_end` / `user_prompt_submit`
//! - [`ToolMiddleware`]: `before` (PreToolUse) / `after` (PostToolUse)
//!
//! SCOPE (M2a): user (`$ATOMCODE_HOME/hooks.json`) + project (`<root>/.hooks.json`).
//! Plugin-contributed inline CC hooks need the plugin loader (still in core) and
//! land in a later slice; the [`HookConfig::plugin_root`] field is already wired so
//! that port is additive.
//!
//! GRACEFUL BY DESIGN: a hook that times out, fails to spawn, or emits garbage is a
//! silent continue — a broken hook never wedges the turn. Only an explicit `deny` /
//! `block` (or a bare exit code `2`, per CC's contract) stops a tool / prompt; any
//! other non-zero exit is a non-blocking error and the turn proceeds.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use atomcode_kernel::hook::LifecycleHooks;
use atomcode_kernel::message::{Conversation, Message};
use atomcode_kernel::middleware::{AfterOutcome, BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall, ToolResult};

// ───────────────────────────── event + config ─────────────────────────────

/// The lifecycle events this port surfaces. A subset of CC's full surface — the
/// high-value events real plugins use. (PreCompact / subagent / batch etc. need
/// new kernel seams and are out of scope here.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
}

impl HookEvent {
    /// Accept BOTH spellings: CC PascalCase (`PreToolUse`) and the legacy
    /// atomcode snake_case (`pre_tool_use`). Unknown events return `None` so the
    /// loader skips them (logged by the caller, not silently dropped).
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "PreToolUse" | "pre_tool_use" => HookEvent::PreToolUse,
            "PostToolUse" | "post_tool_use" => HookEvent::PostToolUse,
            "SessionStart" | "session_start" => HookEvent::SessionStart,
            "SessionEnd" | "session_end" => HookEvent::SessionEnd,
            "UserPromptSubmit" | "user_prompt_submit" => HookEvent::UserPromptSubmit,
            _ => return None,
        })
    }

    /// The CC `hook_event_name` string (PascalCase) sent in the stdin payload.
    fn cc_name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
        }
    }
}

/// One resolved hook: an event + optional tool-name matcher + the shell command.
#[derive(Debug, Clone)]
pub struct HookConfig {
    pub event: HookEvent,
    /// Tool-name matcher (`PreToolUse`/`PostToolUse` only). `None`/`"*"` = all;
    /// `"foo_*"` = prefix; otherwise exact.
    pub matcher: Option<String>,
    pub command: String,
    pub timeout_ms: u64,
    /// Set for plugin-contributed hooks; exported as `CLAUDE_PLUGIN_ROOT` /
    /// `ATOMCODE_PLUGIN_ROOT` so a plugin script can locate resources alongside
    /// its manifest. We never substitute it into the command (injection-safe).
    pub plugin_root: Option<PathBuf>,
}

fn default_timeout_ms() -> u64 {
    10_000
}

// ───────────────────────────── config loading ─────────────────────────────

/// On-disk `hooks.json` shape: `{ "hooks": { "<name>": { event, matcher?,
/// command, timeout_ms?, disabled? } } }`. Mirrors the legacy core format so a
/// user's existing file works unchanged.
#[derive(Debug, Deserialize)]
struct HooksFile {
    #[serde(default)]
    hooks: BTreeMap<String, HookEntry>,
}

#[derive(Debug, Deserialize)]
struct HookEntry {
    event: String,
    #[serde(default)]
    matcher: Option<String>,
    command: String,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    disabled: bool,
}

fn load_hooks_file(path: &Path) -> Vec<HookConfig> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new(); // missing file → no hooks (not an error).
    };
    let Ok(parsed) = serde_json::from_str::<HooksFile>(&raw) else {
        return Vec::new(); // malformed → skip the file rather than wedge startup.
    };
    parsed
        .hooks
        .into_values()
        .filter(|e| !e.disabled)
        .filter_map(|e| {
            HookEvent::parse(&e.event).map(|event| HookConfig {
                event,
                matcher: e.matcher,
                command: e.command,
                timeout_ms: e.timeout_ms,
                plugin_root: None,
            })
        })
        .collect()
}

/// Resolve `$ATOMCODE_HOME` (fallback `~/.atomcode`).
fn atomcode_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("ATOMCODE_HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    dirs::home_dir().map(|h| h.join(".atomcode"))
}

/// Load user (`$ATOMCODE_HOME/hooks.json`) + project (`<root>/.hooks.json`) hooks.
pub fn load_hooks_config(project_dir: &Path) -> Vec<HookConfig> {
    let mut out = Vec::new();
    if let Some(home) = atomcode_home() {
        out.extend(load_hooks_file(&home.join("hooks.json")));
    }
    out.extend(load_hooks_file(&project_dir.join(".hooks.json")));
    out
}

/// Tool-name matcher. `None`/empty/`"*"` = all. Otherwise a `|`-separated list of
/// alternatives, each a glob where `*` matches any run of chars (incl. empty) and every
/// other char is literal — covering the patterns real hooks use: exact (`Bash`),
/// alternation (`Edit|Write`), and wildcards (`mcp__github__*`, `Notebook*`). Stays
/// zero-dependency and backward compatible with the legacy trailing-`*` form
/// (`edit_*` still matches `edit_file`). Full regex (`.`, `[...]`, `()`) is NOT
/// supported — such a matcher is treated literally and simply won't match.
fn matches_tool(matcher: &Option<String>, tool_name: &str) -> bool {
    let Some(pat) = matcher.as_deref() else { return true };
    if pat.is_empty() || pat == "*" {
        return true;
    }
    pat.split('|').any(|alt| glob_match(alt.trim(), tool_name))
}

/// Glob match where `*` matches any run (including empty) and every other byte is
/// literal. Linear two-pointer scan with a star-backtrack mark — no exponential blowup.
fn glob_match(pat: &str, s: &str) -> bool {
    let (pb, sb) = (pat.as_bytes(), s.as_bytes());
    let (mut p, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while si < sb.len() {
        if p < pb.len() && pb[p] == b'*' {
            star = Some(p);
            mark = si;
            p += 1;
        } else if p < pb.len() && pb[p] == sb[si] {
            p += 1;
            si += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while p < pb.len() && pb[p] == b'*' {
        p += 1;
    }
    p == pb.len()
}

// ───────────────────────────── command exec ─────────────────────────────

/// Build a shell invocation for a hook command (so pipes / `&&` / globs work,
/// matching CC). `sh -c` on unix, `cmd /C` on Windows.
fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(windows)]
    {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    }
}

/// Run one hook: pipe `stdin_json` to it (CC's `json.load(sys.stdin)` contract),
/// honor the timeout, and return `(exit_code, stdout)`. `exit_code` is the process
/// exit status — `None` when the child was killed by a signal. The OUTER `None`
/// (timeout / spawn-failure) → the caller treats it as a silent continue.
async fn run_command_hook(hook: &HookConfig, stdin_json: &str) -> Option<(Option<i32>, String)> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let mut cmd = shell_command(&hook.command);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Drop the future on timeout → kill the subprocess instead of leaking it.
        .kill_on_drop(true);
    if let Some(root) = &hook.plugin_root {
        cmd.env("CLAUDE_PLUGIN_ROOT", root);
        cmd.env("ATOMCODE_PLUGIN_ROOT", root);
    }

    let fut = async {
        let mut child = cmd.spawn().ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            // BEST-EFFORT delivery: a hook that ignores its stdin (echo, a simple exit,
            // an observation-only PostToolUse hook) may ALREADY have exited, so this
            // write races the child and can fail with EPIPE. That is expected — it must
            // NOT discard the hook's real exit code / stdout, so swallow the result.
            let _ = stdin.write_all(stdin_json.as_bytes()).await;
            // Explicit shutdown so a `read`/`json.load(sys.stdin)` returns now
            // rather than blocking until our timeout fires.
            let _ = stdin.shutdown().await;
            drop(stdin);
        }
        let out = child.wait_with_output().await.ok()?;
        Some((out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned()))
    };

    (tokio::time::timeout(Duration::from_millis(hook.timeout_ms), fut).await).ok().flatten()
}

/// CC's exit-code contract: exit **2** (and only 2) requests a BLOCK — stop the tool
/// or prompt. Every other code, INCLUDING other non-zero codes (a hook crash, a
/// generic error), is a NON-blocking signal: the tool/prompt proceeds. Matching this
/// exactly is what keeps the module's promise that a merely-broken hook (one that runs
/// but exits non-zero) never wedges the turn — only an explicit `2` or a parsed
/// `deny`/`block` decision stops anything.
fn exit_requests_block(code: Option<i32>) -> bool {
    code == Some(2)
}

/// The last non-empty line of stdout parsed as JSON (CC emits its decision as the
/// final line; earlier lines may be diagnostics).
fn last_json_line(stdout: &str) -> Option<Value> {
    let line = stdout.lines().rev().find(|l| !l.trim().is_empty())?;
    serde_json::from_str(line.trim()).ok()
}

// ───────────────────────────── decision parsing ─────────────────────────────

/// A permissive view over a hook's JSON decision, spanning the CC shape
/// (`hookSpecificOutput` / top-level `decision`) and the legacy atomcode shape
/// (`action`). Only the fields we act on are captured; unknown keys are ignored.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Decision {
    /// atomcode-native: `allow` | `block` | `modify`.
    action: Option<String>,
    /// CC top-level (PostToolUse / UserPromptSubmit): `block`.
    decision: Option<String>,
    reason: Option<String>,
    /// atomcode `modify` payload.
    args: Option<Value>,
    #[serde(rename = "hookSpecificOutput")]
    hook_specific: Option<HookSpecific>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct HookSpecific {
    /// CC PreToolUse: `allow` | `deny` | `ask`.
    #[serde(rename = "permissionDecision")]
    permission_decision: Option<String>,
    #[serde(rename = "permissionDecisionReason")]
    permission_decision_reason: Option<String>,
    /// CC PreToolUse: replaces the tool input.
    #[serde(rename = "updatedInput")]
    updated_input: Option<Value>,
    /// CC PostToolUse: replaces the tool result shown to the model.
    #[serde(rename = "updatedToolOutput")]
    updated_tool_output: Option<String>,
    /// CC context injection (SessionStart / UserPromptSubmit / PreToolUse).
    #[serde(rename = "additionalContext")]
    additional_context: Option<String>,
}

impl Decision {
    fn additional_context(&self) -> Option<&str> {
        self.hook_specific.as_ref()?.additional_context.as_deref()
    }
}

// ───────────────────────────── the producer ─────────────────────────────

/// CC external-hook runner. Holds the resolved config + the cwd / session id used to
/// build CC payloads. Clone-cheap is not needed (registered once behind `Arc`).
pub struct CCExternalHooks {
    hooks: Vec<HookConfig>,
    cwd: String,
    /// CC `session_id` stamped into every payload. Empty when the agent has no
    /// persistent session (the driver supplies it via [`with_session_id`]).
    session_id: String,
    /// Whether any PostToolUse hook is configured — gates the call→tool bookkeeping
    /// below so the (common) no-PostToolUse path allocates nothing.
    has_post_tool_hooks: bool,
    /// `tool_call_id → tool_name`, populated by `before` so PostToolUse `after` (which
    /// the kernel does not hand a tool name) can honor tool-name matchers. An entry is
    /// recorded only for a call that will RUN (gate ≠ Deny) and removed by `after`, so
    /// the map never outlives a turn's in-flight calls.
    call_tools: Mutex<HashMap<String, String>>,
}

impl CCExternalHooks {
    /// Build from an explicit hook list (used by tests + a future plugin source).
    pub fn new(hooks: Vec<HookConfig>, cwd: impl Into<String>) -> Self {
        let has_post_tool_hooks = hooks.iter().any(|h| h.event == HookEvent::PostToolUse);
        Self {
            hooks,
            cwd: cwd.into(),
            session_id: String::new(),
            has_post_tool_hooks,
            call_tools: Mutex::new(HashMap::new()),
        }
    }

    /// Load user + project `hooks.json` for `project_dir`.
    pub fn load(project_dir: &Path) -> Self {
        Self::new(load_hooks_config(project_dir), project_dir.to_string_lossy().into_owned())
    }

    /// Stamp the CC `session_id` sent in every payload (the agent's persistent session
    /// id). Builder-style so callers can chain off [`load`]; leave unset for one-shot
    /// runs (the payload then carries an empty `session_id`, as before).
    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = id.into();
        self
    }

    /// True when no hooks are configured — callers skip registration so the
    /// common no-hooks path adds zero overhead.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    fn matching(&self, event: HookEvent, tool: Option<&str>) -> Vec<&HookConfig> {
        self.hooks
            .iter()
            .filter(|h| h.event == event && tool.map(|t| matches_tool(&h.matcher, t)).unwrap_or(true))
            .collect()
    }
}

/// PostToolUse matcher resolution: when we know the tool name (the usual case — `before`
/// recorded it) honor it like any other matcher; when we DON'T (the call was denied or
/// never reached our `before`), only an all-tools matcher (`None`/`"*"`) may fire.
fn post_tool_matches(matcher: &Option<String>, tool_name: Option<&str>) -> bool {
    match tool_name {
        Some(name) => matches_tool(matcher, name),
        None => matches!(matcher.as_deref(), None | Some("*")),
    }
}

#[async_trait]
impl LifecycleHooks for CCExternalHooks {
    async fn session_start(&self, convo: &mut Conversation, _resumed: bool) {
        let payload = serde_json::json!({
            "session_id": self.session_id,
            "hook_event_name": HookEvent::SessionStart.cc_name(),
            "cwd": self.cwd,
        })
        .to_string();
        // Run matching hooks CONCURRENTLY (the payload is built once; SessionStart can't
        // block and only appends context), then push their output in config order so the
        // injected context stays deterministic. A single slow hook no longer serializes
        // the rest.
        let matched = self.matching(HookEvent::SessionStart, None);
        let outs =
            futures::future::join_all(matched.iter().map(|h| run_command_hook(h, &payload))).await;
        for (_code, stdout) in outs.into_iter().flatten() {
            // SessionStart: stdout (plain or hookSpecificOutput.additionalContext)
            // is injected as context. CC cannot block here.
            let ctx = last_json_line(&stdout)
                .as_ref()
                .and_then(|v| serde_json::from_value::<Decision>(v.clone()).ok())
                .and_then(|d| d.additional_context().map(str::to_owned))
                .unwrap_or_else(|| stdout.trim().to_string());
            if !ctx.is_empty() {
                convo.push(Message::user(crate::reminder::system_reminder(&ctx)));
            }
        }
    }

    async fn user_prompt_submit(&self, text: &mut String) -> Result<(), String> {
        let payload = serde_json::json!({
            "session_id": self.session_id,
            "hook_event_name": HookEvent::UserPromptSubmit.cc_name(),
            "prompt": &*text,
            "cwd": self.cwd,
        })
        .to_string();
        // Run matching hooks CONCURRENTLY — the payload is built once (every hook sees the
        // same original prompt), so there is no feed-forward between them. Fold the results
        // in config order: the first block (a `block` decision or a bare exit 2) wins; the
        // rest contribute injected context. (CC likewise runs UserPromptSubmit hooks in
        // parallel and aggregates.)
        let matched = self.matching(HookEvent::UserPromptSubmit, None);
        let outs =
            futures::future::join_all(matched.iter().map(|h| run_command_hook(h, &payload))).await;
        let mut injected: Vec<String> = Vec::new();
        for out in outs {
            let Some((exit_code, stdout)) = out else {
                continue; // timeout / spawn failure → silent continue.
            };
            let decided = last_json_line(&stdout)
                .and_then(|v| serde_json::from_value::<Decision>(v).ok());
            if let Some(d) = &decided {
                let blocks = d.decision.as_deref() == Some("block")
                    || d.action.as_deref() == Some("block");
                if blocks {
                    return Err(d
                        .reason
                        .clone()
                        .unwrap_or_else(|| "prompt blocked by hook".to_string()));
                }
                if let Some(c) = d.additional_context() {
                    if !c.is_empty() {
                        injected.push(c.to_string());
                    }
                    continue;
                }
            }
            // No parsed decision: CC's exit-code contract takes over. Exit 2 blocks the
            // prompt even without a JSON decision; any other code is non-blocking, so its
            // stdout is treated as injected context (a broken hook never wedges input).
            if decided.is_none() {
                if exit_requests_block(exit_code) {
                    let trimmed = stdout.trim();
                    return Err(if trimmed.is_empty() {
                        "prompt blocked by hook (exit 2)".to_string()
                    } else {
                        trimmed.to_string()
                    });
                }
                let trimmed = stdout.trim();
                if !trimmed.is_empty() {
                    injected.push(trimmed.to_string());
                }
            }
        }
        if !injected.is_empty() {
            text.push_str("\n\n");
            text.push_str(&injected.join("\n\n"));
        }
        Ok(())
    }

    async fn session_end(&self, _convo: &Conversation) {
        let payload = serde_json::json!({
            "session_id": self.session_id,
            "hook_event_name": HookEvent::SessionEnd.cc_name(),
            "cwd": self.cwd,
        })
        .to_string();
        // Observation only — fire all matching hooks concurrently and ignore output.
        let matched = self.matching(HookEvent::SessionEnd, None);
        futures::future::join_all(matched.iter().map(|h| run_command_hook(h, &payload))).await;
    }
}

#[async_trait]
impl ToolMiddleware for CCExternalHooks {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        // PreToolUse stdin: tool_input is the PARSED args object (CC sends an
        // object, not a string), falling back to the raw string if unparseable.
        let tool_input: Value =
            serde_json::from_str(&call.arguments).unwrap_or_else(|_| Value::String(call.arguments.clone()));
        let payload = serde_json::json!({
            "session_id": self.session_id,
            "hook_event_name": HookEvent::PreToolUse.cc_name(),
            "tool_name": call.name,
            "tool_input": tool_input,
            "cwd": self.cwd,
        })
        .to_string();

        // Fold across matching hooks: most-restrictive gate wins
        // (Deny > Ask > Allow > Proceed); the last `updatedInput` rewrite of `call.arguments`
        // wins. SEQUENTIAL (unlike the fire-and-forget session_* events) so the gate can
        // SHORT-CIRCUIT on the first Deny — a denied call must not keep spawning the
        // remaining hooks — and so the rewrite order is deterministic.
        let mut gate = BeforeOutcome::Proceed;
        for hook in self.matching(HookEvent::PreToolUse, Some(&call.name)) {
            let Some((exit_code, stdout)) = run_command_hook(hook, &payload).await else {
                continue;
            };
            let decided = last_json_line(&stdout)
                .and_then(|v| serde_json::from_value::<Decision>(v).ok());

            // Apply an arg rewrite (CC updatedInput / atomcode modify).
            if let Some(d) = &decided {
                let new_args = d
                    .hook_specific
                    .as_ref()
                    .and_then(|hs| hs.updated_input.clone())
                    .or_else(|| {
                        if d.action.as_deref() == Some("modify") {
                            d.args.clone()
                        } else {
                            None
                        }
                    });
                if let Some(v) = new_args {
                    call.arguments = v.to_string();
                }

                // Resolve this hook's gate decision.
                let cc_perm = d
                    .hook_specific
                    .as_ref()
                    .and_then(|hs| hs.permission_decision.as_deref());
                let reason = || {
                    d.hook_specific
                        .as_ref()
                        .and_then(|hs| hs.permission_decision_reason.clone())
                        .or_else(|| d.reason.clone())
                        .unwrap_or_else(|| format!("blocked by hook ({})", call.name))
                };
                let this = match (cc_perm, d.decision.as_deref(), d.action.as_deref()) {
                    (Some("deny"), _, _) | (_, Some("block"), _) | (_, _, Some("block")) => {
                        BeforeOutcome::Deny { reason: reason() }
                    }
                    (Some("ask"), _, _) => BeforeOutcome::Ask { reason: None },
                    (Some("allow"), _, _) | (_, _, Some("allow")) => {
                        BeforeOutcome::Allow { reason: None }
                    }
                    _ => BeforeOutcome::Proceed,
                };
                gate = stronger(gate, this);
            } else if exit_requests_block(exit_code) {
                // CC exit-code contract: exit 2 (and ONLY 2) blocks. Other non-zero
                // codes are non-blocking errors → the tool proceeds (a broken hook must
                // not wedge the turn — the spawn/timeout paths already continue).
                let trimmed = stdout.trim();
                let reason = if trimmed.is_empty() {
                    format!("hook blocked {} (exit 2)", call.name)
                } else {
                    trimmed.to_string()
                };
                gate = stronger(gate, BeforeOutcome::Deny { reason });
            }

            if matches!(gate, BeforeOutcome::Deny { .. }) {
                break; // deny is final.
            }
        }
        // Remember this call's tool name for PostToolUse `after` (kernel hands it no tool
        // name), but ONLY for a call that will actually run — a Deny here means the tool is
        // blocked, so its PostToolUse must not fire. `after` removes the entry.
        if self.has_post_tool_hooks && !matches!(gate, BeforeOutcome::Deny { .. }) {
            if let Ok(mut m) = self.call_tools.lock() {
                m.insert(call.id.clone(), call.name.clone());
            }
        }
        gate
    }

    async fn after(&self, result: &mut ToolResult) -> AfterOutcome {
        // Recover the tool name `before` stashed for this call_id (kernel doesn't thread
        // it into `after`), so PostToolUse tool-name matchers are honored. Absent ⇒ the
        // call never ran our `before` (e.g. denied earlier) ⇒ only all-tools hooks fire.
        let tool_name = self
            .call_tools
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&result.call_id));
        let payload = serde_json::json!({
            "session_id": self.session_id,
            "hook_event_name": HookEvent::PostToolUse.cc_name(),
            "tool_name": tool_name,
            "tool_response": result.content,
            "cwd": self.cwd,
        })
        .to_string();
        // SEQUENTIAL so the last `updatedToolOutput` rewrite of `result.content` wins
        // deterministically (the per-tool path typically has 0-1 PostToolUse hooks, so
        // there is little to gain from the session_*-style concurrency here).
        let mut outcome = AfterOutcome::Proceed;
        for hook in self.hooks.iter().filter(|h| {
            h.event == HookEvent::PostToolUse
                && post_tool_matches(&h.matcher, tool_name.as_deref())
        }) {
            let Some((_code, stdout)) = run_command_hook(hook, &payload).await else {
                continue;
            };
            if let Some(d) = last_json_line(&stdout).and_then(|v| serde_json::from_value::<Decision>(v).ok()) {
                if let Some(new_out) = d.hook_specific.as_ref().and_then(|hs| hs.updated_tool_output.clone()) {
                    result.content = new_out;
                }
                if d.decision.as_deref() == Some("block") {
                    outcome = AfterOutcome::Block {
                        reason: d.reason.clone().unwrap_or_else(|| "blocked by PostToolUse hook".into()),
                    };
                }
            }
        }
        outcome
    }
}

/// Most-restrictive-wins precedence for the PreToolUse gate fold:
/// Deny > Ask > Allow > Proceed.
fn stronger(a: BeforeOutcome, b: BeforeOutcome) -> BeforeOutcome {
    fn rank(o: &BeforeOutcome) -> u8 {
        match o {
            BeforeOutcome::Proceed => 0,
            BeforeOutcome::Allow { .. } => 1,
            BeforeOutcome::Ask { .. } => 2,
            BeforeOutcome::Deny { .. } => 3,
        }
    }
    if rank(&b) >= rank(&a) {
        b
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_event_both_spellings() {
        assert_eq!(HookEvent::parse("PreToolUse"), Some(HookEvent::PreToolUse));
        assert_eq!(HookEvent::parse("pre_tool_use"), Some(HookEvent::PreToolUse));
        assert_eq!(HookEvent::parse("UserPromptSubmit"), Some(HookEvent::UserPromptSubmit));
        assert_eq!(HookEvent::parse("nonsense"), None);
    }

    #[test]
    fn matcher_semantics() {
        assert!(matches_tool(&None, "bash"));
        assert!(matches_tool(&Some("*".into()), "bash"));
        assert!(matches_tool(&Some(String::new()), "bash")); // empty = all
        assert!(matches_tool(&Some("edit_*".into()), "edit_file")); // legacy trailing-* glob
        assert!(!matches_tool(&Some("edit_*".into()), "write_file"));
        assert!(matches_tool(&Some("bash".into()), "bash"));
        assert!(!matches_tool(&Some("bash".into()), "grep"));
    }

    #[test]
    fn matcher_alternation_and_glob() {
        // F5: `|`-separated alternatives (the common CC form).
        assert!(matches_tool(&Some("Edit|Write".into()), "Edit"));
        assert!(matches_tool(&Some("Edit|Write".into()), "Write"));
        assert!(!matches_tool(&Some("Edit|Write".into()), "Bash"));
        // Whitespace around alternatives is tolerated.
        assert!(matches_tool(&Some("Edit | Write".into()), "Write"));
        // `*` is a wildcard anywhere, not just trailing.
        assert!(matches_tool(&Some("mcp__github__*".into()), "mcp__github__get_issue"));
        assert!(!matches_tool(&Some("mcp__github__*".into()), "mcp__gitlab__get_issue"));
        assert!(matches_tool(&Some("*_file".into()), "edit_file"));
        // Exact stays exact (no accidental substring match).
        assert!(!matches_tool(&Some("Edit".into()), "MultiEdit"));
    }

    #[test]
    fn loads_and_filters_hooks_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".hooks.json"),
            r#"{"hooks":{
                "a":{"event":"PreToolUse","matcher":"bash","command":"echo a"},
                "b":{"event":"pre_tool_use","command":"echo b","disabled":true},
                "c":{"event":"bogus_event","command":"echo c"}
            }}"#,
        )
        .unwrap();
        let hooks = load_hooks_config(dir.path());
        // `b` disabled, `c` unknown event → only `a` survives.
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].event, HookEvent::PreToolUse);
        assert_eq!(hooks[0].matcher.as_deref(), Some("bash"));
        assert_eq!(hooks[0].timeout_ms, 10_000);
    }

    #[test]
    fn decision_precedence_deny_beats_allow() {
        let d = stronger(
            BeforeOutcome::Allow { reason: None },
            BeforeOutcome::Deny { reason: "no".into() },
        );
        assert!(d.is_deny());
        let a = stronger(BeforeOutcome::Proceed, BeforeOutcome::Allow { reason: None });
        assert!(matches!(a, BeforeOutcome::Allow { .. }));
    }

    #[test]
    fn parses_cc_pretooluse_decision() {
        let v: Value = serde_json::from_str(
            r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"nope"}}"#,
        )
        .unwrap();
        let d: Decision = serde_json::from_value(v).unwrap();
        assert_eq!(d.hook_specific.unwrap().permission_decision.as_deref(), Some("deny"));
    }

    #[tokio::test]
    async fn before_denies_on_cc_deny() {
        // A hook that prints a CC deny decision blocks the call.
        let hook = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"x"}}'"#.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let tool: Arc<dyn Tool> = Arc::new(atomcode_kernel::testkit::EchoTool);
        let mut call = ToolCall { id: "1".into(), name: "bash".into(), arguments: "{}".into() };
        let out = cc.before(&mut call, &tool, &rt).await;
        assert!(out.is_deny(), "CC deny must block: {out:?}");
    }

    #[tokio::test]
    async fn user_prompt_block_returns_err() {
        let hook = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: r#"echo '{"decision":"block","reason":"banned"}'"#.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp");
        let mut text = "hello".to_string();
        let r = cc.user_prompt_submit(&mut text).await;
        assert_eq!(r, Err("banned".to_string()));
    }

    #[tokio::test]
    async fn user_prompt_injects_additional_context() {
        let hook = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: r#"echo '{"hookSpecificOutput":{"additionalContext":"CTX"}}'"#.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp");
        let mut text = "hi".to_string();
        assert!(cc.user_prompt_submit(&mut text).await.is_ok());
        assert!(text.contains("hi"), "original prompt preserved");
        assert!(text.contains("CTX"), "context appended: {text}");
    }

    #[test]
    fn exit_code_block_contract_is_exit_2_only() {
        assert!(exit_requests_block(Some(2)));
        assert!(!exit_requests_block(Some(0)));
        assert!(!exit_requests_block(Some(1)));
        assert!(!exit_requests_block(Some(127)));
        assert!(!exit_requests_block(None)); // killed by signal → non-blocking
    }

    /// A PreToolUse hook that runs but exits NON-2 (a crash / generic error) with no
    /// JSON decision must NOT block — a merely-broken hook can't wedge every tool call.
    #[tokio::test]
    async fn before_non2_exit_does_not_block() {
        let hook = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "exit 1".into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let tool: Arc<dyn Tool> = Arc::new(atomcode_kernel::testkit::EchoTool);
        let mut call = ToolCall { id: "1".into(), name: "bash".into(), arguments: "{}".into() };
        let out = cc.before(&mut call, &tool, &rt).await;
        assert!(matches!(out, BeforeOutcome::Proceed), "exit 1 must NOT block: {out:?}");
    }

    /// Exit 2 with no JSON decision DOES block (CC's bare exit-2 contract).
    #[tokio::test]
    async fn before_exit_2_blocks() {
        let hook = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "exit 2".into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let tool: Arc<dyn Tool> = Arc::new(atomcode_kernel::testkit::EchoTool);
        let mut call = ToolCall { id: "1".into(), name: "bash".into(), arguments: "{}".into() };
        let out = cc.before(&mut call, &tool, &rt).await;
        assert!(out.is_deny(), "exit 2 must block: {out:?}");
    }

    /// UserPromptSubmit mirrors `before`: exit 2 blocks the prompt (Err), other non-zero
    /// does not (the prompt proceeds).
    #[tokio::test]
    async fn user_prompt_exit_code_contract() {
        let block = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "exit 2".into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![block], "/tmp");
        let mut text = "hi".to_string();
        assert!(cc.user_prompt_submit(&mut text).await.is_err(), "exit 2 blocks the prompt");

        let ok = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: "exit 1".into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![ok], "/tmp");
        let mut text = "hi".to_string();
        assert!(cc.user_prompt_submit(&mut text).await.is_ok(), "exit 1 must NOT block");
        assert_eq!(text, "hi", "no stdout ⇒ prompt unchanged");
    }

    #[test]
    fn post_tool_matcher_resolution() {
        // Known tool name ⇒ honored like any matcher.
        assert!(post_tool_matches(&Some("bash".into()), Some("bash")));
        assert!(!post_tool_matches(&Some("bash".into()), Some("grep")));
        assert!(post_tool_matches(&Some("edit_*".into()), Some("edit_file")));
        // Unknown tool name ⇒ only an all-tools matcher may fire.
        assert!(post_tool_matches(&None, None));
        assert!(post_tool_matches(&Some("*".into()), None));
        assert!(!post_tool_matches(&Some("bash".into()), None));
    }

    /// F3: a PostToolUse hook with a tool-name matcher now fires for the matching tool
    /// (the name `before` recorded for the call_id) and is skipped for a non-match.
    #[tokio::test]
    async fn post_tool_use_honors_matcher() {
        let hook = HookConfig {
            event: HookEvent::PostToolUse,
            matcher: Some("bash".into()),
            command: r#"echo '{"hookSpecificOutput":{"updatedToolOutput":"REWRITTEN"}}'"#.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let tool: Arc<dyn Tool> = Arc::new(atomcode_kernel::testkit::EchoTool);

        // before() records call_id "1" → "bash"; after() then matches matcher "bash".
        let mut call = ToolCall { id: "1".into(), name: "bash".into(), arguments: "{}".into() };
        let _ = cc.before(&mut call, &tool, &rt).await;
        let mut result = ToolResult { call_id: "1".into(), content: "orig".into(), is_error: false };
        cc.after(&mut result).await;
        assert_eq!(result.content, "REWRITTEN", "matcher 'bash' must fire for tool bash");

        // A different tool ("grep") does NOT match matcher "bash" → output untouched.
        let mut call = ToolCall { id: "2".into(), name: "grep".into(), arguments: "{}".into() };
        let _ = cc.before(&mut call, &tool, &rt).await;
        let mut result = ToolResult { call_id: "2".into(), content: "orig".into(), is_error: false };
        cc.after(&mut result).await;
        assert_eq!(result.content, "orig", "matcher 'bash' must NOT fire for tool grep");
    }

    /// F4: the configured session_id is threaded into the CC payload (the hook greps its
    /// own stdin for it and only emits context on a match).
    #[tokio::test]
    async fn session_id_is_threaded_into_payload() {
        let hook = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: r#"grep -q '"session_id":"sess-xyz"' && echo MATCHED"#.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp").with_session_id("sess-xyz");
        let mut text = "hi".to_string();
        assert!(cc.user_prompt_submit(&mut text).await.is_ok());
        assert!(text.contains("MATCHED"), "session_id must reach the payload: {text}");
    }

    fn prompt_hook(cmd: &str) -> HookConfig {
        HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command: cmd.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        }
    }

    /// F6: parallelizing UserPromptSubmit (via `join_all`) must keep the injected context
    /// in CONFIG order. A single `echo` is a shell builtin (no extra process), so this is
    /// robust under the parallel test harness; concurrency itself is guaranteed
    /// structurally by `join_all`, not asserted via flaky wall-clock timing.
    #[tokio::test]
    async fn user_prompt_aggregates_multiple_hooks_in_order() {
        let cc = CCExternalHooks::new(vec![prompt_hook("echo AAA"), prompt_hook("echo BBB")], "/tmp");
        let mut text = "hi".to_string();
        cc.user_prompt_submit(&mut text).await.unwrap();
        let (a, b) = (text.find("AAA"), text.find("BBB"));
        assert!(a.is_some() && b.is_some(), "both contexts injected: {text}");
        assert!(a < b, "context kept in config order: {text}");
        assert!(text.starts_with("hi"), "original prompt preserved: {text}");
    }
}
