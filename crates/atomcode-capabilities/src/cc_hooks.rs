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
//! - [`LifecycleHooks`]: `session_start` / `session_end` / `user_prompt_submit`,
//!   plus `turn_complete` (mapped to the CC `Stop` / `StopFailure` terminal events)
//! - [`ToolMiddleware`]: `before` (PreToolUse) / `after` (PostToolUse)
//!
//! SCOPE (M2a): user (`$ATOMCODE_HOME/hooks.json`) + project (`<root>/.hooks.json`).
//! Plugin-contributed inline CC hooks need the plugin loader (still in core) and
//! land in a later slice; the [`HookConfig::plugin_root`] field is already wired so
//! that port is additive.
//!
//! GRACEFUL BY DESIGN: a hook that times out, fails to spawn, or emits garbage is a
//! silent continue — a broken hook never wedges the turn. Only an explicit `deny` /
//! `block` decision, or a bare exit `2` that carries a DELIBERATE reason (per CC's
//! contract), stops a tool / prompt. A bare exit `2` with no output, or whose stderr
//! is just a command launch failure (e.g. a plugin's `python3 "$CLAUDE_PLUGIN_ROOT/
//! x.py"` whose path doesn't resolve → python `can't open file` → exit 2), is treated
//! as a broken hook and does NOT block — see [`deliberate_block_reason`]. Any other
//! non-zero exit is a non-blocking error and the turn proceeds.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Conversation;
use atomcode_kernel::middleware::{AfterOutcome, BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall, ToolResult};

use crate::tools::{request_approval_decision, PermissionDecision, APPROVAL_KIND};

// ───────────────────────────── event + config ─────────────────────────────

/// The lifecycle events this port surfaces. A subset of CC's full surface — the
/// high-value events real plugins use. (PreCompact / subagent / batch etc. need
/// new kernel seams and are out of scope here.) `Stop` / `StopFailure` cover the
/// turn-terminal pair: every turn end fires EXACTLY ONE of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    /// Fired when a tool call FAILS (`ToolResult.is_error`), so a plugin can tell
    /// "tool succeeded" (`PostToolUse`) apart from "tool failed" (`PostToolUseFailure`).
    PostToolUseFailure,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    /// Turn ended (normal stop, safety fuses, user cancel, rate-limit pause).
    Stop,
    /// Turn ended because of an API/provider failure (`ProviderError` / `Timeout`).
    StopFailure,
}

impl HookEvent {
    /// Accept BOTH spellings: CC PascalCase (`PreToolUse`) and the legacy
    /// atomcode snake_case (`pre_tool_use`). Unknown events return `None` so the
    /// loader skips them (logged by the caller, not silently dropped).
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "PreToolUse" | "pre_tool_use" => HookEvent::PreToolUse,
            "PostToolUse" | "post_tool_use" => HookEvent::PostToolUse,
            "PostToolUseFailure" | "post_tool_use_failure" => HookEvent::PostToolUseFailure,
            "SessionStart" | "session_start" => HookEvent::SessionStart,
            "SessionEnd" | "session_end" => HookEvent::SessionEnd,
            "UserPromptSubmit" | "user_prompt_submit" => HookEvent::UserPromptSubmit,
            "Stop" | "stop" => HookEvent::Stop,
            "StopFailure" | "stop_failure" => HookEvent::StopFailure,
            _ => return None,
        })
    }

    /// The CC `hook_event_name` string (PascalCase) sent in the stdin payload.
    fn cc_name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
        }
    }
}

/// One resolved hook: an event + optional tool-name matcher + the shell command.
#[derive(Debug, Clone)]
pub struct HookConfig {
    pub event: HookEvent,
    /// Tool-name matcher (`PreToolUse`/`PostToolUse`/`PostToolUseFailure`). `None`/`"*"` = all;
    /// `"foo_*"` = prefix; otherwise exact.
    pub matcher: Option<String>,
    pub command: String,
    pub timeout_ms: u64,
    /// Set for plugin-contributed hooks; exported as `CLAUDE_PLUGIN_ROOT` /
    /// `ATOMCODE_PLUGIN_ROOT` so a plugin script can locate resources alongside
    /// its manifest. We never substitute it into the command (injection-safe).
    pub plugin_root: Option<PathBuf>,
}

impl HookConfig {
    /// Build a config from a Claude-Code plugin's INLINE manifest hook (a `plugin.json`
    /// `hooks` entry). The driver resolves these from installed plugins — which L1 cannot
    /// reach — and feeds them in via [`CCExternalHooks::load_with_extra`]. `event_name` is
    /// CC PascalCase (or the legacy snake_case); `timeout_secs` is CC's SECONDS (converted
    /// to ms, default 10s). Returns `None` for an unsupported event (the caller skips it).
    pub fn from_plugin_spec(
        event_name: &str,
        matcher: Option<String>,
        command: String,
        timeout_secs: Option<u64>,
        plugin_root: PathBuf,
    ) -> Option<HookConfig> {
        Some(HookConfig {
            event: HookEvent::parse(event_name)?,
            matcher,
            command,
            timeout_ms: timeout_secs
                .map(|s| s.saturating_mul(1000))
                .unwrap_or_else(default_timeout_ms),
            plugin_root: Some(plugin_root),
        })
    }
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

/// The GLOBAL hooks file `load_hooks_config` reads
/// (`$ATOMCODE_HOME`/`~/.atomcode` + `/hooks.json`), or `None` when no home resolves.
/// Exposed so diagnostics (`atomcode hooks paths`/`list`) show EXACTLY the file that is
/// loaded — which under `sudo` is NOT the sudo-aware `Config::config_dir()` this module
/// deliberately does not use.
pub fn global_hooks_path() -> Option<PathBuf> {
    atomcode_home().map(|h| h.join("hooks.json"))
}

/// The PROJECT hooks file `load_hooks_config` reads (`<root>/.hooks.json`).
pub fn project_hooks_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".hooks.json")
}

/// Load user (`$ATOMCODE_HOME/hooks.json`) + project (`<root>/.hooks.json`) hooks.
pub fn load_hooks_config(project_dir: &Path) -> Vec<HookConfig> {
    let mut out = Vec::new();
    if let Some(p) = global_hooks_path() {
        out.extend(load_hooks_file(&p));
    }
    out.extend(load_hooks_file(&project_hooks_path(project_dir)));
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
    let Some(pat) = matcher.as_deref() else {
        return true;
    };
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
/// honor the timeout, and return `(exit_code, stdout, stderr)`. `exit_code` is the
/// process exit status — `None` when the child was killed by a signal. stderr is
/// captured (not discarded) so callers can (a) surface a real block reason and
/// (b) tell a DELIBERATE exit-2 block from a hook that merely failed to launch (see
/// [`deliberate_block_reason`]). The OUTER `None` (timeout / spawn-failure) → the
/// caller treats it as a silent continue.
async fn run_command_hook(
    hook: &HookConfig,
    stdin_json: &str,
) -> Option<(Option<i32>, String, String)> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let mut cmd = shell_command(&hook.command);
    #[cfg(unix)]
    crate::process_utils::apply_utf8_locale_env(&mut cmd);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Drop the future on timeout → kill the subprocess instead of leaking it.
        .kill_on_drop(true);
    // Suppress the Windows console-window flash when the parent has no console
    // (daemon/clawbot); no-op off Windows.
    crate::process_utils::suppress_console_window(&mut cmd);
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
        Some((
            out.status.code(),
            crate::tools::bash::decode_output(&out.stdout),
            crate::tools::bash::decode_output(&out.stderr),
        ))
    };

    (tokio::time::timeout(Duration::from_millis(hook.timeout_ms), fut).await)
        .ok()
        .flatten()
}

/// Output of a diagnostic single-hook run (`atomcode hooks test`).
#[derive(Debug, Clone)]
pub struct HookRunOutput {
    /// Process exit status; `None` if the hook was killed by a signal.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run ONE hook for diagnostics, piping `payload` to its stdin (the CC
/// `json.load(sys.stdin)` contract) and honoring the hook's timeout. Reuses the
/// SAME executor the live middleware uses, so `atomcode hooks test` observes exactly
/// what a real turn would run. Returns `None` if the hook timed out or failed to spawn.
pub async fn run_hook_for_test(hook: &HookConfig, payload: &Value) -> Option<HookRunOutput> {
    run_command_hook(hook, &payload.to_string())
        .await
        .map(|(exit_code, stdout, stderr)| HookRunOutput {
            exit_code,
            stdout,
            stderr,
        })
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

/// Did the hook COMMAND fail to launch / run, rather than the hook program
/// deliberately deciding to block? CC maps a bare exit `2` to "block", but a
/// misconfigured command often *also* exits 2 incidentally — most notably
/// `python3 "$CLAUDE_PLUGIN_ROOT/script.py"` ("can't open file ... [Errno 2]" →
/// exit 2) when a plugin hook's path doesn't resolve. Treating that as a deliberate
/// block wedges EVERY prompt / tool for everyone who installed that plugin. So when
/// stderr carries an interpreter/shell launch-failure signature we classify it as a
/// broken hook (non-blocking), honoring the module's promise that a merely-broken
/// hook never wedges the turn. Signatures are specific diagnostics that a real,
/// human-written block reason is very unlikely to contain verbatim.
fn is_hook_launch_failure(stderr: &str) -> bool {
    const SIGNATURES: &[&str] = &[
        "can't open file",     // python3 missing/unreadable script → exit 2
        "command not found",   // sh: <cmd>: command not found
        "cannot execute",      // sh: cannot execute binary file
        "ModuleNotFoundError", // python missing dependency
        "No module named",     // python -m / import failure
        "ImportError",         // python broken import
    ];
    SIGNATURES.iter().any(|sig| stderr.contains(sig))
}

/// Resolve the human-facing reason for a BARE exit-2 (a hook that exited 2 with no
/// parsed JSON decision) and decide whether it is a DELIBERATE block. Returns
/// `Some(reason)` only when the hook genuinely meant to block; `None` when it should
/// be treated as a non-blocking broken / no-op hook. A deliberate CC block hook
/// communicates a reason (on stdout, or — per CC's UserPromptSubmit convention — on
/// stderr). A hook that exits 2 with NO output at all, or whose stderr is just an
/// interpreter/shell launch failure (see [`is_hook_launch_failure`]), is broken, not
/// deliberate — so the prompt / tool proceeds instead of being wedged.
fn deliberate_block_reason(stdout: &str, stderr: &str) -> Option<String> {
    let out = stdout.trim();
    let err = stderr.trim();
    let reason = if !out.is_empty() { out } else { err };
    if reason.is_empty() || is_hook_launch_failure(reason) {
        return None;
    }
    Some(reason.to_string())
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
    /// CC `transcript_path` — the session's append-only JSONL transcript file,
    /// when the driver persists one. `None` for one-shot runs (the Stop /
    /// StopFailure payload then carries `null`).
    transcript_path: Option<String>,
    /// Whether any PostToolUse / PostToolUseFailure hook is configured — gates the
    /// call→tool bookkeeping below so the (common) no-PostToolUse path allocates
    /// nothing.
    has_post_tool_hooks: bool,
    /// `tool_call_id → tool_name`, populated by `before` so PostToolUse /
    /// PostToolUseFailure `after` (which the kernel does not hand a tool name) can
    /// honor tool-name matchers. An entry is recorded only for a call that will RUN
    /// (gate ≠ Deny) and removed by `after`, so the map never outlives a turn's
    /// in-flight calls.
    call_tools: Mutex<HashMap<String, String>>,
}

impl CCExternalHooks {
    /// Build from an explicit hook list (used by tests + a future plugin source).
    pub fn new(hooks: Vec<HookConfig>, cwd: impl Into<String>) -> Self {
        let has_post_tool_hooks = hooks.iter().any(|h| {
            matches!(h.event, HookEvent::PostToolUse | HookEvent::PostToolUseFailure)
        });
        Self {
            hooks,
            cwd: cwd.into(),
            session_id: String::new(),
            transcript_path: None,
            has_post_tool_hooks,
            call_tools: Mutex::new(HashMap::new()),
        }
    }

    /// Load user + project `hooks.json` for `project_dir`.
    pub fn load(project_dir: &Path) -> Self {
        Self::new(
            load_hooks_config(project_dir),
            project_dir.to_string_lossy().into_owned(),
        )
    }

    /// Like [`load`](Self::load), but APPEND `extra` hooks the driver resolved out of band
    /// — e.g. plugin-contributed inline hooks ([`HookConfig::from_plugin_spec`]), which
    /// live behind the plugin loader that L1 cannot depend on. File hooks come first, then
    /// the extras (order only affects context-injection order; the gate fold is
    /// order-independent). An empty `extra` makes this identical to `load`.
    pub fn load_with_extra(project_dir: &Path, extra: Vec<HookConfig>) -> Self {
        let mut hooks = load_hooks_config(project_dir);
        hooks.extend(extra);
        Self::new(hooks, project_dir.to_string_lossy().into_owned())
    }

    /// Stamp the CC `session_id` sent in every payload (the agent's persistent session
    /// id). Builder-style so callers can chain off [`load`]; leave unset for one-shot
    /// runs (the payload then carries an empty `session_id`, as before).
    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = id.into();
        self
    }

    /// Stamp the CC `transcript_path` (the session's append-only JSONL transcript,
    /// resolved by the driver). Builder-style; leave unset for one-shot runs — the
    /// Stop / StopFailure payload then carries `null`.
    pub fn with_transcript_path(mut self, path: impl Into<String>) -> Self {
        self.transcript_path = Some(path.into());
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
            .filter(|h| {
                h.event == event && tool.map(|t| matches_tool(&h.matcher, t)).unwrap_or(true)
            })
            .collect()
    }
}

/// CC `StopFailure` fires when a turn ends BECAUSE of an API/provider failure;
/// every other terminal (normal stop, safety fuses, user cancel, rate-limit
/// pause) is a plain `Stop`. ProviderError + Timeout are the "API 出错" terminals
/// the issue asks to surface. Tool errors mid-turn NEVER map here — the kernel
/// routes those to `on_error` and the turn continues (it ends with a real
/// terminal reason instead), so a failed tool call must not light the failure
/// lamp for a turn that then completes normally.
fn stop_is_failure(reason: &StopReason) -> bool {
    matches!(reason, StopReason::ProviderError | StopReason::Timeout)
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
        for (_code, stdout, _stderr) in outs.into_iter().flatten() {
            // SessionStart: stdout (plain or hookSpecificOutput.additionalContext)
            // is injected as context. CC cannot block here.
            let ctx = last_json_line(&stdout)
                .as_ref()
                .and_then(|v| serde_json::from_value::<Decision>(v.clone()).ok())
                .and_then(|d| d.additional_context().map(str::to_owned))
                .unwrap_or_else(|| stdout.trim().to_string());
            if !ctx.is_empty() {
                // Hook stdout is runtime-owned ambient context, not text authored by the
                // human.  Keep it as a user-role message for provider/cache compatibility,
                // but preserve its provenance so presentation layers never replay it as a
                // visible user bubble when a live snapshot is synchronized at turn end.
                convo.push(crate::reminder::synthetic_system_reminder(&ctx));
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
            let Some((exit_code, stdout, stderr)) = out else {
                continue; // timeout / spawn failure → silent continue.
            };
            let decided =
                last_json_line(&stdout).and_then(|v| serde_json::from_value::<Decision>(v).ok());
            if let Some(d) = &decided {
                let blocks =
                    d.decision.as_deref() == Some("block") || d.action.as_deref() == Some("block");
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
            // prompt — but ONLY when the hook communicated a deliberate reason. A hook
            // that exits 2 with no output, or whose stderr is just an interpreter/shell
            // launch failure (e.g. a plugin's `python3 "$CLAUDE_PLUGIN_ROOT/x.py"` whose
            // path doesn't resolve → python "can't open file" → exit 2), is BROKEN, not a
            // deliberate block; it must not wedge every prompt. Any other exit code is
            // non-blocking, so its stdout is treated as injected context.
            if decided.is_none() {
                if exit_requests_block(exit_code) {
                    if let Some(reason) = deliberate_block_reason(&stdout, &stderr) {
                        return Err(reason);
                    }
                    // Broken / no-op exit-2 hook → fall through as non-blocking.
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

    /// CC's turn-terminal pair: EVERY turn end fires EXACTLY ONE of `Stop` /
    /// `StopFailure`. The kernel funnels every terminal (normal stop, safety
    /// fuses, provider error, stream timeout, cancel) through this single hook,
    /// so a plugin sees the turn's true outcome — never a stale or missing event.
    /// `StopFailure` (the "API 出错" signal) is reserved for a turn that ended
    /// BECAUSE of a provider/stream failure; everything else is a plain `Stop`.
    /// The payload also carries the kernel `stop_reason` so a plugin can refine
    /// (e.g. tell a user cancel from a fuse trip) without parsing anything else.
    async fn turn_complete(&self, _convo: &Conversation, reason: &StopReason, _ctx: &TurnCtx) {
        let event = if stop_is_failure(reason) {
            HookEvent::StopFailure
        } else {
            HookEvent::Stop
        };
        let matched = self.matching(event, None);
        // CC parity: `stop_hook_active` tells a terminal hook whether ANOTHER
        // Stop/StopFailure hook is configured and will fire alongside it — the
        // recursion-suppression flag CC hooks use (a Stop hook that triggers a
        // new prompt checks it to avoid re-entering itself).
        let stop_hook_active = matched.len() > 1;
        let payload = serde_json::json!({
            "session_id": self.session_id,
            "transcript_path": self.transcript_path,
            "hook_event_name": event.cc_name(),
            "stop_hook_active": stop_hook_active,
            "stop_reason": format!("{reason:?}"),
            "cwd": self.cwd,
        })
        .to_string();
        // Observation only — fire all matching hooks concurrently and ignore output.
        futures::future::join_all(matched.iter().map(|h| run_command_hook(h, &payload))).await;
    }
}

impl CCExternalHooks {
    /// Round-trip the driver for a hook-forced `ask` and map the decision, reusing the
    /// generic approval wire contract (`ApprovalMiddleware`'s `kind` + shapes) so the
    /// driver renders its normal approval prompt. A `Null` response (driver gone / timed
    /// out / cancelled) fails CLOSED as `Deny`, distinguishable in the reason from a real
    /// user denial (matching `ApprovalMiddleware`). Approval returns `Allow` so it
    /// short-circuits the downstream auto-approve gates — the point of an explicit "ask".
    /// A hook-forced ask is NOT remembered (no grant store here): "always" behaves like
    /// "allow once", so the ask keeps prompting — the intended semantics of a forced ask.
    async fn resolve_ask(&self, call: &ToolCall, rt: &RequestCtx) -> BeforeOutcome {
        match request_approval_decision(rt, APPROVAL_KIND, call, &call.name).await {
            Err(degraded) => degraded, // Null → fail closed (shared channel-failure deny).
            Ok(PermissionDecision::AllowOnce | PermissionDecision::AllowAlways) => {
                BeforeOutcome::Allow {
                    reason: Some("approved (hook ask)".into()),
                }
            }
            Ok(PermissionDecision::Deny) => BeforeOutcome::deny(format!(
                "denied by approval prompt (hook ask): {}",
                call.name
            )),
        }
    }
}

#[async_trait]
impl ToolMiddleware for CCExternalHooks {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> BeforeOutcome {
        // PreToolUse stdin: tool_input is the PARSED args object (CC sends an
        // object, not a string), falling back to the raw string if unparseable.
        let tool_input: Value = serde_json::from_str(&call.arguments)
            .unwrap_or_else(|_| Value::String(call.arguments.clone()));
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
            let Some((exit_code, stdout, stderr)) = run_command_hook(hook, &payload).await else {
                continue;
            };
            let decided =
                last_json_line(&stdout).and_then(|v| serde_json::from_value::<Decision>(v).ok());

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
                // CC exit-code contract: exit 2 (and ONLY 2) blocks. Other non-zero codes
                // are non-blocking errors → the tool proceeds (a broken hook must not wedge
                // the turn — the spawn/timeout paths already continue). A bare exit 2 only
                // denies when the hook gave a deliberate reason: an exit-2 with no output,
                // or one whose stderr is just a command launch failure (e.g. a plugin's
                // python script path didn't resolve), is broken, not a deny, so the tool
                // proceeds rather than being wedged for everyone with that plugin.
                if let Some(reason) = deliberate_block_reason(&stdout, &stderr) {
                    gate = stronger(gate, BeforeOutcome::Deny { reason });
                }
            }

            if gate.is_deny() {
                break; // deny is final.
            }
        }
        // Resolve a folded `ask` (CC `permissionDecision:"ask"`) into a REAL approval
        // prompt. The kernel has no L0 approval mechanism (it treats `Ask` as a no-op), so
        // — like `BashWorkspaceGate` / `WriteApprovalGate` — we round-trip the driver here
        // and map the decision. This middleware runs BEFORE the downstream auto-approve
        // gates, so returning `Allow` on approval short-circuits them: an explicit hook
        // "ask" forces a prompt even for an in-workspace edit or a Safe read, which would
        // otherwise auto-approve and drop the "ask" silently. Resolved AFTER the fold so a
        // later hook's `Deny` still outranks it (Deny > Ask).
        if matches!(gate, BeforeOutcome::Ask { .. }) {
            gate = self.resolve_ask(call, rt).await;
        }
        // Remember this call's tool name for PostToolUse / PostToolUseFailure `after`
        // (kernel hands it no tool name), but ONLY for a call that will actually run — a
        // Deny here means the tool is blocked, so its post-tool hook must not fire.
        // `after` removes the entry.
        if self.has_post_tool_hooks && !gate.is_deny() {
            if let Ok(mut m) = self.call_tools.lock() {
                m.insert(call.id.clone(), call.name.clone());
            }
        }
        gate
    }

    async fn after(
        &self,
        result: &mut ToolResult,
        _tool: Option<&std::sync::Arc<dyn atomcode_kernel::tool::Tool>>,
    ) -> AfterOutcome {
        // Recover the tool name `before` stashed for this call_id (kernel doesn't thread
        // it into `after`), so PostToolUse / PostToolUseFailure tool-name matchers are
        // honored. Absent ⇒ the call never ran our `before` (e.g. denied earlier) ⇒ only
        // all-tools hooks fire.
        let tool_name = self
            .call_tools
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&result.call_id));
        // A failed tool call (`is_error`) fires PostToolUseFailure instead of
        // PostToolUse, so a plugin can branch on success vs failure — but the payload
        // and the rewrite/block semantics stay identical for both.
        let event = if result.is_error {
            HookEvent::PostToolUseFailure
        } else {
            HookEvent::PostToolUse
        };
        let payload = serde_json::json!({
            "session_id": self.session_id,
            "hook_event_name": event.cc_name(),
            "tool_name": tool_name,
            "tool_response": result.content,
            "cwd": self.cwd,
        })
        .to_string();
        // SEQUENTIAL so the last `updatedToolOutput` rewrite of `result.content` wins
        // deterministically (the per-tool path typically has 0-1 hooks, so there is
        // little to gain from the session_*-style concurrency here).
        let mut outcome = AfterOutcome::Proceed;
        for hook in self.hooks.iter().filter(|h| {
            h.event == event && post_tool_matches(&h.matcher, tool_name.as_deref())
        }) {
            let Some((_code, stdout, _stderr)) = run_command_hook(hook, &payload).await else {
                continue;
            };
            if let Some(d) =
                last_json_line(&stdout).and_then(|v| serde_json::from_value::<Decision>(v).ok())
            {
                if let Some(new_out) = d
                    .hook_specific
                    .as_ref()
                    .and_then(|hs| hs.updated_tool_output.clone())
                {
                    result.content = new_out;
                }
                if d.decision.as_deref() == Some("block") {
                    outcome = AfterOutcome::Block {
                        reason: d
                            .reason
                            .clone()
                            .unwrap_or_else(|| "blocked by hook".into()),
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
            BeforeOutcome::DenyTurn { .. } | BeforeOutcome::DenyTurnWithIntervention { .. } => 4,
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
        assert_eq!(
            HookEvent::parse("pre_tool_use"),
            Some(HookEvent::PreToolUse)
        );
        assert_eq!(
            HookEvent::parse("UserPromptSubmit"),
            Some(HookEvent::UserPromptSubmit)
        );
        assert_eq!(HookEvent::parse("Stop"), Some(HookEvent::Stop));
        assert_eq!(HookEvent::parse("stop"), Some(HookEvent::Stop));
        assert_eq!(
            HookEvent::parse("StopFailure"),
            Some(HookEvent::StopFailure)
        );
        assert_eq!(
            HookEvent::parse("stop_failure"),
            Some(HookEvent::StopFailure)
        );
        assert_eq!(HookEvent::parse("nonsense"), None);
        assert_eq!(HookEvent::parse("SubagentStop"), None);
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
        assert!(matches_tool(
            &Some("mcp__github__*".into()),
            "mcp__github__get_issue"
        ));
        assert!(!matches_tool(
            &Some("mcp__github__*".into()),
            "mcp__gitlab__get_issue"
        ));
        assert!(matches_tool(&Some("*_file".into()), "edit_file"));
        // Exact stays exact (no accidental substring match).
        assert!(!matches_tool(&Some("Edit".into()), "MultiEdit"));
    }

    #[test]
    fn from_plugin_spec_maps_event_timeout_and_root() {
        let h = HookConfig::from_plugin_spec(
            "PreToolUse",
            Some("Bash".into()),
            "echo hi".into(),
            Some(3), // CC seconds → 3000 ms
            PathBuf::from("/plugins/foo"),
        )
        .unwrap();
        assert_eq!(h.event, HookEvent::PreToolUse);
        assert_eq!(h.matcher.as_deref(), Some("Bash"));
        assert_eq!(h.timeout_ms, 3_000);
        assert_eq!(h.plugin_root.as_deref(), Some(Path::new("/plugins/foo")));

        // Omitted timeout → default; legacy snake_case event still parses.
        let h2 = HookConfig::from_plugin_spec("post_tool_use", None, "x".into(), None, "/p".into())
            .unwrap();
        assert_eq!(h2.event, HookEvent::PostToolUse);
        assert_eq!(h2.timeout_ms, 10_000);

        // Stop / StopFailure are supported plugin hook events (both spellings).
        let h3 = HookConfig::from_plugin_spec("Stop", None, "x".into(), None, "/p".into()).unwrap();
        assert_eq!(h3.event, HookEvent::Stop);
        let h4 = HookConfig::from_plugin_spec("stop_failure", None, "x".into(), None, "/p".into())
            .unwrap();
        assert_eq!(h4.event, HookEvent::StopFailure);

        // Unsupported event → None (caller skips it).
        assert!(
            HookConfig::from_plugin_spec("PreCompact", None, "x".into(), None, "/p".into())
                .is_none()
        );
    }

    #[test]
    fn stop_vs_stop_failure_terminal_mapping() {
        // API/provider failure terminals → StopFailure ("API 出错" lamp).
        assert!(stop_is_failure(&StopReason::ProviderError));
        assert!(stop_is_failure(&StopReason::Timeout));
        // Every other terminal → plain Stop.
        for reason in [
            StopReason::Stopped,
            StopReason::MaxRounds,
            StopReason::MaxContinuations,
            StopReason::RepeatLoop,
            StopReason::ToolLoopDetected,
            StopReason::Cancelled,
            StopReason::PromptRejected,
            StopReason::PolicyDenied,
            StopReason::RateLimited,
        ] {
            assert!(!stop_is_failure(&reason), "{reason:?} must map to Stop");
        }
    }

    /// End-to-end: `turn_complete` dispatches EXACTLY ONE terminal event — `Stop`
    /// on a normal stop, `StopFailure` on an API/provider failure. Each hook greps
    /// its own stdin for its `hook_event_name` and touches a marker file when it
    /// fires, so we observe which subscriber ran.
    #[tokio::test]
    #[cfg(unix)]
    async fn turn_complete_dispatches_stop_vs_stop_failure() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().replace('\'', "'\\''");
        let stop_marker = dir.path().join("stop-fired");
        let failure_marker = dir.path().join("failure-fired");
        let stop_marker_s = stop_marker.to_string_lossy().replace('\'', "'\\''");
        let failure_marker_s = failure_marker.to_string_lossy().replace('\'', "'\\''");
        let hook = |event: HookEvent, marker: &str, name: &str| HookConfig {
            event,
            matcher: None,
            command: format!(
                "cd '{cwd}' && grep -q '\"hook_event_name\":\"{name}\"' && touch '{marker}'"
            ),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(
            vec![
                hook(HookEvent::Stop, &stop_marker_s, "Stop"),
                hook(HookEvent::StopFailure, &failure_marker_s, "StopFailure"),
            ],
            "/tmp",
        );
        let ctx = TurnCtx {
            session_id: None,
            turn_id: 1,
            request_id: 1,
            round: 1,
            max_rounds: None,
            cache_epoch: 0,
            context_window: 0,
            used_tokens: 0,
        };
        let convo = Conversation::new();

        // Normal terminal → ONLY the Stop hook fires.
        cc.turn_complete(&convo, &StopReason::Stopped, &ctx).await;
        assert!(stop_marker.exists(), "Stop hook must fire on a normal stop");
        assert!(
            !failure_marker.exists(),
            "StopFailure must NOT fire on a normal stop"
        );

        // Provider-error terminal → ONLY the StopFailure hook fires.
        let _ = std::fs::remove_file(&stop_marker);
        cc.turn_complete(&convo, &StopReason::ProviderError, &ctx)
            .await;
        assert!(
            failure_marker.exists(),
            "StopFailure hook must fire on a provider error"
        );
        assert!(
            !stop_marker.exists(),
            "Stop must NOT fire on a provider error"
        );
    }

    /// CC Stop/StopFailure payload parity: the payload carries the driver-resolved
    /// `transcript_path`, and `stop_hook_active` tells a terminal hook whether ANOTHER
    /// terminal hook is configured and fires alongside it (the recursion-suppression
    /// flag CC hooks use).
    #[tokio::test]
    #[cfg(unix)]
    async fn turn_complete_payload_alignment() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().replace('\'', "'\\''");
        let transcript = dir.path().join("session.jsonl");
        let transcript_s = transcript.to_string_lossy().replace('\'', "'\\''");
        let marker = |name: &str| {
            let p = dir.path().join(name);
            (p.clone(), p.to_string_lossy().replace('\'', "'\\''"))
        };
        let ctx = TurnCtx {
            session_id: None,
            turn_id: 1,
            request_id: 1,
            round: 1,
            max_rounds: None,
            cache_epoch: 0,
            context_window: 0,
            used_tokens: 0,
        };
        let convo = Conversation::new();

        // 1) A lone terminal hook sees the transcript path AND `stop_hook_active:false`.
        let (m1, m1_s) = marker("payload-ok");
        let single = HookConfig {
            event: HookEvent::Stop,
            matcher: None,
            command: format!(
                // Land the payload before grepping it: two chained `grep -q` share one
                // stdin, and the first drains the pipe when it exits on its match, so
                // the second always sees EOF and the `touch` never runs.
                "cd '{cwd}' && cat > payload.json \
                 && grep -q '\"transcript_path\":\"{transcript_s}\"' payload.json \
                 && grep -q '\"stop_hook_active\":false' payload.json && touch '{m1_s}'"
            ),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        CCExternalHooks::new(vec![single], "/tmp")
            .with_transcript_path(transcript_s.clone())
            .turn_complete(&convo, &StopReason::Stopped, &ctx)
            .await;
        assert!(
            m1.exists(),
            "payload must carry transcript_path + stop_hook_active:false"
        );

        // 2) TWO terminal hooks: BOTH see `stop_hook_active:true` (recursion flag).
        let (m_a, m_a_s) = marker("a-ok");
        let (m_b, m_b_s) = marker("b-ok");
        let hook = |marker_s: &str| HookConfig {
            event: HookEvent::Stop,
            matcher: None,
            command: format!(
                "cd '{cwd}' && grep -q '\"stop_hook_active\":true' && touch '{marker_s}'"
            ),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        CCExternalHooks::new(vec![hook(&m_a_s), hook(&m_b_s)], "/tmp")
            .turn_complete(&convo, &StopReason::Stopped, &ctx)
            .await;
        assert!(
            m_a.exists(),
            "first of two hooks must see stop_hook_active:true"
        );
        assert!(
            m_b.exists(),
            "second of two hooks must see stop_hook_active:true"
        );
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
            BeforeOutcome::Deny {
                reason: "no".into(),
            },
        );
        assert!(d.is_deny());
        let a = stronger(
            BeforeOutcome::Proceed,
            BeforeOutcome::Allow { reason: None },
        );
        assert!(matches!(a, BeforeOutcome::Allow { .. }));
    }

    #[test]
    fn parses_cc_pretooluse_decision() {
        let v: Value = serde_json::from_str(
            r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"nope"}}"#,
        )
        .unwrap();
        let d: Decision = serde_json::from_value(v).unwrap();
        assert_eq!(
            d.hook_specific.unwrap().permission_decision.as_deref(),
            Some("deny")
        );
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
        let mut call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let out = cc.before(&mut call, &tool, &rt).await;
        assert!(out.is_deny(), "CC deny must block: {out:?}");
    }

    #[tokio::test]
    async fn run_hook_for_test_captures_exit_and_stdout() {
        // Diagnostic single-hook runner (`atomcode hooks test`): pipes a payload,
        // captures exit code + stdout.
        let hook = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: "echo hooktest-ok".into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let out = run_hook_for_test(&hook, &serde_json::json!({"hook_event_name": "PreToolUse"}))
            .await
            .expect("hook ran");
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("hooktest-ok"), "stdout: {}", out.stdout);
    }

    #[tokio::test]
    async fn before_ask_forces_approval_round_trip_and_fails_closed() {
        // REGRESSION: a hook `permissionDecision:"ask"` must FORCE a real approval prompt,
        // not be silently dropped. Previously the kernel no-op'd `BeforeOutcome::Ask`, so an
        // "ask" hook let the call auto-approve (esp. an in-workspace edit or a Safe read).
        // Now the middleware round-trips the driver itself. With a SILENT driver the bounded
        // round-trip times out → Null → Deny (fail closed), marked as an internal channel
        // failure so it's distinguishable from a real user denial. Before the fix this
        // assertion would have been `Proceed`.
        let hook = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: r#"echo '{"hookSpecificOutput":{"permissionDecision":"ask"}}'"#.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(20)));
        let tool: Arc<dyn Tool> = Arc::new(atomcode_kernel::testkit::EchoTool);
        let mut call = ToolCall {
            id: "1".into(),
            name: "edit_file".into(),
            arguments: "{}".into(),
        };
        let out = cc.before(&mut call, &tool, &rt).await;
        assert!(
            out.is_deny(),
            "hook ask with a silent driver must fail closed, not proceed: {out:?}"
        );
        let reason = out.deny_reason().unwrap();
        assert!(
            reason.contains("internal channel failure") && reason.contains("not a user"),
            "a degraded (Null) forced-ask round-trip must read as a channel failure, not a user deny: {reason}"
        );
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

    #[tokio::test]
    async fn session_start_context_is_synthetic_not_user_authored() {
        let hook = HookConfig {
            event: HookEvent::SessionStart,
            matcher: None,
            command: r#"echo '{"hookSpecificOutput":{"additionalContext":"WSL setup hint"}}'"#
                .into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp");
        let mut convo = Conversation::new();

        cc.session_start(&mut convo, false).await;

        let injected = convo
            .messages
            .last()
            .expect("session hook should inject context");
        assert_eq!(injected.role, atomcode_kernel::message::Role::User);
        assert!(
            injected.synthetic,
            "hook context must not render as human input"
        );
        assert!(injected.text.contains("WSL setup hint"));
        assert!(injected.text.starts_with("<system-reminder>"));
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
        let mut call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let out = cc.before(&mut call, &tool, &rt).await;
        assert!(
            matches!(out, BeforeOutcome::Proceed),
            "exit 1 must NOT block: {out:?}"
        );
    }

    async fn run_before(cmd: &str) -> BeforeOutcome {
        let hook = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: None,
            command: cmd.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![hook], "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let tool: Arc<dyn Tool> = Arc::new(atomcode_kernel::testkit::EchoTool);
        let mut call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        cc.before(&mut call, &tool, &rt).await
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn command_hook_preserves_utf8_paths_when_parent_locale_is_c() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().replace('\'', "'\\''");
        let command = format!(
            r#"
            cd '{}'
            mkdir -p "产品需求/流水线/帮助文档"
            printf 'line\n' > "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
            wc -l "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
        "#,
            cwd
        );
        let hook = HookConfig {
            event: HookEvent::UserPromptSubmit,
            matcher: None,
            command,
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let _guard = crate::process_utils::EnvVarGuard::new(&["LC_ALL", "LANG", "LC_CTYPE"]);
        std::env::set_var("LC_ALL", "C");
        std::env::set_var("LANG", "C");
        std::env::set_var("LC_CTYPE", "C");

        let (_code, stdout, _stderr) = run_command_hook(&hook, "{}").await.unwrap();

        assert!(
            stdout.contains("产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"),
            "stdout was: {:?}",
            stdout
        );
    }

    /// Exit 2 WITH a deliberate reason DOES block (CC's bare exit-2 contract), and the
    /// reason is surfaced (stdout, then stderr) rather than an opaque "(exit 2)".
    #[tokio::test]
    async fn before_deliberate_exit_2_blocks() {
        let out = run_before("echo BLOCKED; exit 2").await;
        assert!(out.is_deny(), "deliberate exit 2 must block: {out:?}");
        if let BeforeOutcome::Deny { reason } = out {
            assert!(reason.contains("BLOCKED"), "reason surfaced: {reason}");
        }
        // Reason on stderr (CC convention) is surfaced too.
        let out = run_before("echo DENIED-ON-STDERR >&2; exit 2").await;
        assert!(
            out.is_deny(),
            "exit 2 with stderr reason must block: {out:?}"
        );
        if let BeforeOutcome::Deny { reason } = out {
            assert!(
                reason.contains("DENIED-ON-STDERR"),
                "stderr reason surfaced: {reason}"
            );
        }
    }

    /// A bare exit 2 with NO output is a broken / no-op hook, not a deliberate block —
    /// it must NOT wedge the tool. (Regression: v4.25.4 blocked every prompt/tool when a
    /// plugin's UserPromptSubmit hook exited 2 incidentally.)
    #[tokio::test]
    async fn before_bare_exit_2_does_not_block() {
        let out = run_before("exit 2").await;
        assert!(
            matches!(out, BeforeOutcome::Proceed),
            "bare exit 2 must NOT block: {out:?}"
        );
    }

    /// Exit 2 whose stderr is a command launch failure (the real incident: a plugin's
    /// `python3 "$CLAUDE_PLUGIN_ROOT/x.py"` whose path didn't resolve → python prints
    /// "can't open file ..." and exits 2) is a broken hook → must NOT block.
    #[tokio::test]
    async fn before_launch_failure_exit_2_does_not_block() {
        let out = run_before(
            "echo \"can't open file '/x.py': [Errno 2] No such file or directory\" >&2; exit 2",
        )
        .await;
        assert!(
            matches!(out, BeforeOutcome::Proceed),
            "launch-failure exit 2 must NOT block: {out:?}"
        );
    }

    /// UserPromptSubmit mirrors `before`: a DELIBERATE exit 2 (with a reason) blocks the
    /// prompt (Err), other non-zero does not (the prompt proceeds).
    #[tokio::test]
    async fn user_prompt_exit_code_contract() {
        let cc = CCExternalHooks::new(vec![prompt_hook("echo BLOCKED; exit 2")], "/tmp");
        let mut text = "hi".to_string();
        let err = cc.user_prompt_submit(&mut text).await;
        assert!(err.is_err(), "deliberate exit 2 blocks the prompt");
        assert!(
            err.unwrap_err().contains("BLOCKED"),
            "block reason is surfaced"
        );

        let cc = CCExternalHooks::new(vec![prompt_hook("exit 1")], "/tmp");
        let mut text = "hi".to_string();
        assert!(
            cc.user_prompt_submit(&mut text).await.is_ok(),
            "exit 1 must NOT block"
        );
        assert_eq!(text, "hi", "no stdout ⇒ prompt unchanged");
    }

    /// REGRESSION (v4.25.4): a UserPromptSubmit hook that exits 2 WITHOUT a deliberate
    /// reason — either silently, or with only a command launch failure on stderr (a
    /// plugin's `python3 "$CLAUDE_PLUGIN_ROOT/x.py"` whose path didn't resolve) — is a
    /// broken hook and must NOT wedge the user's prompt.
    #[tokio::test]
    async fn user_prompt_broken_exit_2_does_not_block() {
        // Bare exit 2, no output.
        let cc = CCExternalHooks::new(vec![prompt_hook("exit 2")], "/tmp");
        let mut text = "hi".to_string();
        assert!(
            cc.user_prompt_submit(&mut text).await.is_ok(),
            "bare exit 2 must NOT block"
        );
        assert_eq!(text, "hi", "prompt unchanged");

        // Exit 2 with a python "can't open file" launch failure on stderr (the incident).
        let cc = CCExternalHooks::new(
            vec![prompt_hook(
                "echo \"can't open file '/x.py': [Errno 2] No such file or directory\" >&2; exit 2",
            )],
            "/tmp",
        );
        let mut text = "hi".to_string();
        assert!(
            cc.user_prompt_submit(&mut text).await.is_ok(),
            "launch-failure exit 2 must NOT block the prompt"
        );
        assert_eq!(text, "hi", "prompt unchanged");
    }

    #[test]
    fn launch_failure_vs_deliberate_reason() {
        // Launch failures → not a deliberate block.
        assert!(deliberate_block_reason("", "can't open file '/x.py': [Errno 2]").is_none());
        assert!(deliberate_block_reason("", "python3: command not found").is_none());
        assert!(deliberate_block_reason("", "ModuleNotFoundError: No module named 'x'").is_none());
        // Empty output → not a deliberate block.
        assert!(deliberate_block_reason("", "").is_none());
        assert!(deliberate_block_reason("  ", " \n").is_none());
        // A real reason (stdout or stderr) → deliberate block, surfaced verbatim.
        assert_eq!(
            deliberate_block_reason("nope, blocked", "").as_deref(),
            Some("nope, blocked")
        );
        assert_eq!(
            deliberate_block_reason("", "policy: denied").as_deref(),
            Some("policy: denied")
        );
        // stdout wins over stderr when both present.
        assert_eq!(
            deliberate_block_reason("from-out", "from-err").as_deref(),
            Some("from-out")
        );
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
        let mut call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let _ = cc.before(&mut call, &tool, &rt).await;
        let mut result = ToolResult {
            call_id: "1".into(),
            content: "orig".into(),
            is_error: false,
            images: vec![],
        };
        cc.after(&mut result, None).await;
        assert_eq!(
            result.content, "REWRITTEN",
            "matcher 'bash' must fire for tool bash"
        );

        // A different tool ("grep") does NOT match matcher "bash" → output untouched.
        let mut call = ToolCall {
            id: "2".into(),
            name: "grep".into(),
            arguments: "{}".into(),
        };
        let _ = cc.before(&mut call, &tool, &rt).await;
        let mut result = ToolResult {
            call_id: "2".into(),
            content: "orig".into(),
            is_error: false,
            images: vec![],
        };
        cc.after(&mut result, None).await;
        assert_eq!(
            result.content, "orig",
            "matcher 'bash' must NOT fire for tool grep"
        );
    }

    /// PostToolUseFailure: a FAILED tool call (`is_error`) fires the failure event
    /// instead of PostToolUse, so a plugin can tell success from failure. Both
    /// events share the same payload/rewrite semantics.
    #[tokio::test]
    async fn failed_tool_fires_post_tool_use_failure() {
        let ok_hook = HookConfig {
            event: HookEvent::PostToolUse,
            matcher: None,
            command: r#"echo '{"hookSpecificOutput":{"updatedToolOutput":"OK-REWRITE"}}'"#.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let fail_hook = HookConfig {
            event: HookEvent::PostToolUseFailure,
            matcher: None,
            command: r#"echo '{"hookSpecificOutput":{"updatedToolOutput":"FAIL-REWRITE"}}'"#.into(),
            timeout_ms: 5_000,
            plugin_root: None,
        };
        let cc = CCExternalHooks::new(vec![ok_hook, fail_hook], "/tmp");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let tool: Arc<dyn Tool> = Arc::new(atomcode_kernel::testkit::EchoTool);

        // A SUCCESSFUL call: only the PostToolUse hook may rewrite.
        let mut call = ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let _ = cc.before(&mut call, &tool, &rt).await;
        let mut ok = ToolResult {
            call_id: "1".into(),
            content: "orig".into(),
            is_error: false,
            images: vec![],
        };
        cc.after(&mut ok, None).await;
        assert_eq!(ok.content, "OK-REWRITE", "success must fire PostToolUse, not PostToolUseFailure");

        // A FAILED call: only the PostToolUseFailure hook may rewrite.
        let mut call = ToolCall {
            id: "2".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let _ = cc.before(&mut call, &tool, &rt).await;
        let mut failed = ToolResult {
            call_id: "2".into(),
            content: "orig".into(),
            is_error: true,
            images: vec![],
        };
        cc.after(&mut failed, None).await;
        assert_eq!(
            failed.content, "FAIL-REWRITE",
            "failed call must fire PostToolUseFailure, not PostToolUse"
        );
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
        assert!(
            text.contains("MATCHED"),
            "session_id must reach the payload: {text}"
        );
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
        let cc = CCExternalHooks::new(
            vec![prompt_hook("echo AAA"), prompt_hook("echo BBB")],
            "/tmp",
        );
        let mut text = "hi".to_string();
        cc.user_prompt_submit(&mut text).await.unwrap();
        let (a, b) = (text.find("AAA"), text.find("BBB"));
        assert!(a.is_some() && b.is_some(), "both contexts injected: {text}");
        assert!(a < b, "context kept in config order: {text}");
        assert!(text.starts_with("hi"), "original prompt preserved: {text}");
    }
}
