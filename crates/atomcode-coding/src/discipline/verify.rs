//! Edit-then-verify discipline — the coding self-correction loop.
//!
//! When the model stops (no more tool calls) having EDITED code but not run a successful
//! build/check afterward, we inject a one-shot nudge to verify before finishing. This is
//! the kernel `offer_continuation` seam: `Some(text)` continues the turn with a synthetic user
//! message; `None` lets it stop. The kernel's `max_continuations` fuse bounds
//! the loop, and our own state nudges ONCE per edit-batch so we never spin.
//!
//! Language-agnostic: detection keys on tool NAMES (edit_file / write_file / bash) and, for
//! bash, EXCLUDES a small denylist of read-only / navigation commands (`ls`, `echo`, `cat`,
//! …) so a throwaway `bash ls` after an edit no longer counts as "verified". It never
//! enumerates build commands (no cargo/npm allowlist) — a real check of ANY language still
//! counts. The nudge text lists `cargo check` / `tsc --noEmit` only as examples.

use crate::execution_policy::{execution_policy_for_messages, TurnExecutionPolicy};
use async_trait::async_trait;
use atomcode_kernel::hook::{Continuation, LifecycleHooks};
use atomcode_kernel::message::{Conversation, Role};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

const NUDGE: &str = "You made code edits but have not verified them. Run a fast check \
(`cargo check`, `tsc --noEmit`, or the equivalent for this project) to catch errors \
before finishing. Do NOT start a long-running process (dev server, watcher, full build).";

/// `offer_continuation` hook implementing the edit-then-verify cadence. Holds a small amount of
/// interior state so it nudges at most once per unverified edit.
#[derive(Default)]
pub struct VerifyCadenceHook {
    /// The pinned workspace root. An edit whose target resolves OUTSIDE this root (e.g. a
    /// throwaway `/tmp/notes.txt`) is not project code, so it does not arm the verify cadence
    /// — see [`path_in_workspace_lexical`]. An empty root (the `Default`) treats every edit as
    /// in-workspace, preserving the pre-gate behavior for tests / constructions without a cwd.
    workspace: PathBuf,
    execution_policy: Arc<TurnExecutionPolicy>,
    state: Mutex<State>,
    /// When `true`, the hook does NOT force a post-edit verify continuation — the run is
    /// ATTENDED (interactive TUI), so a present human sees the edit and can ask for a check,
    /// matching codex's "hold off on tests in interactive modes; run them proactively only when
    /// unattended". Default `false` (unattended / headless / scheduled) keeps the forcing
    /// cadence. Set via [`VerifyCadenceHook::attended`], which also honors the `ATOMCODE_VERIFY`
    /// override (`0`/`off` → always suppress, `1`/`on` → always force).
    suppress_verify_continuation: bool,
}

#[derive(Default)]
struct State {
    /// The current real-user turn and tool_call_id we ALREADY nudged for. Including the
    /// turn start keeps reused provider ids (`call_0`, `e1`, …) from suppressing a later
    /// user's fresh edit.
    nudged_for: Option<NudgedEdit>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NudgedEdit {
    turn_start: usize,
    edit_id: String,
}

impl VerifyCadenceHook {
    /// `workspace` is the agent's pinned working directory. Edits outside it don't arm the
    /// cadence (see [`path_in_workspace_lexical`]).
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            execution_policy: Arc::new(TurnExecutionPolicy::new()),
            state: Mutex::new(State::default()),
            suppress_verify_continuation: false,
        }
    }

    pub(crate) fn with_execution_policy(
        workspace: impl Into<PathBuf>,
        execution_policy: Arc<TurnExecutionPolicy>,
    ) -> Self {
        Self {
            workspace: workspace.into(),
            execution_policy,
            state: Mutex::new(State::default()),
            suppress_verify_continuation: false,
        }
    }

    /// Mark whether this run is ATTENDED (interactive). An attended run SUPPRESSES the forced
    /// post-edit verify continuation (a present human can request a check); an unattended run
    /// keeps forcing it. The `ATOMCODE_VERIFY` env overrides either way — see
    /// [`should_suppress_verify`]. Additive builder: existing call sites that don't call this
    /// keep the default (`false` → force), so behavior is unchanged unless opted in.
    pub(crate) fn attended(self, interactive: bool) -> Self {
        let suppress = should_suppress_verify(
            std::env::var("ATOMCODE_VERIFY").ok().as_deref(),
            interactive,
        );
        self.with_suppression(suppress)
    }

    fn with_suppression(mut self, suppress: bool) -> Self {
        self.suppress_verify_continuation = suppress;
        self
    }
}

/// Parse the `ATOMCODE_VERIFY` override into an explicit force decision: `0`/`false`/`off`/`no`
/// → force OFF, `1`/`true`/`on`/`yes` → force ON, anything else / empty / unset → `None` (fall
/// back to the attended default).
fn parse_verify_env(env: Option<&str>) -> Option<bool> {
    match env?.trim().to_ascii_lowercase().as_str() {
        "0" | "false" | "off" | "no" => Some(false),
        "1" | "true" | "on" | "yes" => Some(true),
        _ => None,
    }
}

/// Whether to SUPPRESS the forced post-edit verify continuation. The `ATOMCODE_VERIFY` override
/// wins; otherwise suppress iff the run is attended (interactive) — matching codex's "hold off on
/// tests in interactive modes, run them proactively only when unattended".
fn should_suppress_verify(env: Option<&str>, interactive: bool) -> bool {
    match parse_verify_env(env) {
        Some(force) => !force,
        None => interactive,
    }
}

/// Extract the `command` string from a bash tool-call's raw JSON `arguments`.
fn bash_command(arguments: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| {
            v.get("command")
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
}

/// Extract the `file_path` argument from an `edit_file` / `write_file` tool-call's raw JSON.
fn edit_path(arguments: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(arguments)
        .ok()
        .and_then(|v| {
            v.get("file_path")
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
}

/// Fold `.` and `..` components lexically (no filesystem access, so it never blocks the async
/// turn loop on a hung mount and it needs no path to actually exist). A leading `..` with
/// nothing to pop is kept, so a relative path that escapes its base stays escaped.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether an edit/write target resolves INSIDE the workspace root, decided purely lexically.
/// Relative targets resolve against `workspace` (the tools' cwd) so they are in-workspace by
/// construction; a leading `~` expands via `$HOME`; absolute targets are `..`/`.`-folded and
/// prefix-checked. Deliberately does NOT `canonicalize` (that would block on a stale network
/// mount — the WriteApprovalGate already owns the authoritative canonical decision at write
/// time), so a symlinked/differently-cased in-workspace path may read as outside; that only ever
/// SKIPS a nudge (benign), never produces a false /tmp nudge.
///
/// Returns `true` (conservative — keep the cadence, i.e. pre-gate behavior) when we can't
/// reliably classify: an empty/unparseable target, or a workspace root that isn't absolute
/// (a relative root can't anchor a prefix test — an absolute edit path would never match it,
/// which would silently disable the whole cadence).
fn path_in_workspace_lexical(raw: &str, workspace: &Path) -> bool {
    let raw = raw.trim();
    let root = lexical_normalize(workspace);
    if raw.is_empty() || !root.is_absolute() {
        return true;
    }
    let expanded: PathBuf = if raw == "~" {
        match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h),
            None => return true, // can't expand → don't skip the cadence
        }
    } else if let Some(rest) = raw.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(h) => Path::new(&h).join(rest),
            None => return true,
        }
    } else {
        PathBuf::from(raw)
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        workspace.join(expanded)
    };
    lexical_normalize(&joined).starts_with(&root)
}

/// Doc / prose / tabular-data file types for which a compile or type-check is meaningless.
/// Writing a README, a generated markdown report, a CSV, or a log is NOT "code that must be
/// verified", so such a write must not arm the "run cargo check" cadence — otherwise a
/// non-coding turn (e.g. "write my weekly report" → a `.md` file) triggers a bogus nudge and
/// the model runs an unrelated project's tests. Deliberately conservative: only clearly
/// non-source extensions are listed. Anything NOT here still arms — source code AND
/// build-affecting config (`Cargo.toml`, `package.json`, `tsconfig.json`, …), whose edits a
/// real check legitimately catches.
const NONCODE_DOC_EXTS: &[&str] = &[
    "md", "markdown", "mdx", "txt", "text", "rst", "adoc", "asciidoc", "org", "csv", "tsv", "log",
];

/// Whether an edit target is a doc/data file whose edit should not arm the verify cadence
/// (see [`NONCODE_DOC_EXTS`]). Keys purely on the file extension; a path with no extension,
/// or any extension not in the denylist, is treated as verifiable code (conservative).
fn path_is_noncode_doc(raw: &str) -> bool {
    Path::new(raw.trim())
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| NONCODE_DOC_EXTS.contains(&e.as_str()))
}

/// Whether a post-edit `bash` command plausibly VERIFIES the edit (runs a build / type-check
/// / test / lint), as opposed to read-only or review commands a model might run instead. A
/// command verifies iff at least one of its chained segments runs a NON-read-only command —
/// so `cd sub && cargo test` verifies, but `git diff`, `ls -la`, and `cat x | grep y` (all
/// read-only, even chained) do not. Language-agnostic: it excludes known no-ops/review
/// commands (incl. `git`, which has no build subcommand), never enumerates build commands, so
/// a real check of any language always counts.
fn bash_verifies(cmd: &str) -> bool {
    cmd.split(|c| c == '|' || c == ';' || c == '&')
        .map(str::trim)
        .filter(|seg| !seg.is_empty())
        .any(segment_is_work)
}

/// A single command segment does "work" (plausible verification) if its effective head —
/// after stripping leading `VAR=val` env assignments and known wrappers (`sudo`/`env`/`time`
/// /`nice`/…) and any path prefix — is NOT a read-only / review command.
fn segment_is_work(seg: &str) -> bool {
    // Read-only / review / aggregation commands. `git` included: `git diff|status|log|show`
    // are review, and git has no build/check subcommand.
    const READONLY: &[&str] = &[
        "ls", "cat", "pwd", "echo", "cd", "which", "whoami", "head", "tail", "find", "grep", "rg",
        "fd", "tree", "stat", "file", "printf", "true", "clear", "date", "sleep", "type", "git",
        "wc", "sort", "uniq", "awk", "sed", "cut", "diff", "less", "more", "tee", "basename",
        "dirname", "realpath", "readlink", ":",
    ];
    const WRAPPERS: &[&str] = &["sudo", "env", "time", "nice", "command", "exec"];
    let mut tokens = seg.split_whitespace();
    loop {
        let Some(tok) = tokens.next() else {
            return false; // only env-assignments / wrappers, no real command → not work
        };
        // Skip a leading `VAR=val` env assignment (`FOO=1 cargo test`).
        if tok.contains('=') && !tok.starts_with('-') {
            continue;
        }
        let head = tok.rsplit('/').next().unwrap_or(tok);
        if WRAPPERS.contains(&head) {
            continue; // `sudo`/`env`/`time` … → look at the wrapped command
        }
        return !READONLY.contains(&head);
    }
}

fn current_real_user_start(convo: &Conversation) -> usize {
    convo
        .messages
        .iter()
        .rposition(|m| m.role == Role::User && !m.synthetic)
        .unwrap_or(0)
}

/// Scan the conversation: returns the tool_call_id of the most recent successful edit
/// IF it has no VERIFYING `bash` after it (i.e. unverified), else `None`. A `bash` that is
/// merely read-only (`ls`/`echo`/…) does not count — see [`bash_verifies`].
fn unverified_edit(convo: &Conversation, workspace: &Path) -> Option<NudgedEdit> {
    let start = current_real_user_start(convo);
    // Tool-call ids are assigned by the assistant message that precedes the matching
    // tool-result message, so a single forward pass can resolve a result's tool name.
    let mut names: HashMap<&str, &str> = HashMap::new();
    // bash tool_call id → its command string (to tell a real check from an `ls` dodge).
    let mut bash_cmds: HashMap<&str, String> = HashMap::new();
    // edit/write tool_call id → its `file_path` (to gate out-of-workspace throwaway writes).
    let mut edit_paths: HashMap<&str, String> = HashMap::new();
    let mut last_edit_id: Option<String> = None;
    let mut bash_after_edit = false;

    for msg in &convo.messages[start..] {
        match msg.role {
            Role::Assistant => {
                for tc in &msg.tool_calls {
                    names.insert(tc.id.as_str(), tc.name.as_str());
                    if tc.name == "bash" {
                        if let Some(cmd) = bash_command(&tc.arguments) {
                            bash_cmds.insert(tc.id.as_str(), cmd);
                        }
                    } else if tc.name == "edit_file" || tc.name == "write_file" {
                        if let Some(p) = edit_path(&tc.arguments) {
                            edit_paths.insert(tc.id.as_str(), p);
                        }
                    }
                }
            }
            Role::Tool => {
                if msg.is_error {
                    continue;
                }
                let Some(id) = msg.tool_call_id.as_deref() else {
                    continue;
                };
                match names.get(id).copied() {
                    // Only edits WITHIN the workspace arm the cadence — a throwaway write
                    // outside the project (e.g. /tmp) is not code to compile-check. A missing
                    // /unparseable path is treated as in-workspace (conservative — keep the nudge).
                    // A doc/data write (a `.md` report, `.csv`, `.log`) is also skipped: it is
                    // not compilable code, so it must not arm "run cargo check" on a non-coding
                    // turn (see [`path_is_noncode_doc`]).
                    Some("edit_file") | Some("write_file")
                        if edit_paths
                            .get(id)
                            .is_none_or(|p| path_in_workspace_lexical(p, workspace))
                            && !edit_paths.get(id).is_some_and(|p| path_is_noncode_doc(p)) =>
                    {
                        last_edit_id = Some(id.to_string());
                        bash_after_edit = false;
                    }
                    // Only a real check counts — a read-only/navigation command does NOT verify.
                    Some("bash") => {
                        if bash_cmds.get(id).is_some_and(|c| bash_verifies(c)) {
                            bash_after_edit = true;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    match last_edit_id {
        Some(id) if !bash_after_edit => Some(NudgedEdit {
            turn_start: start,
            edit_id: id,
        }),
        _ => None,
    }
}

fn verify_reminder_already_present(convo: &Conversation, edit: &NudgedEdit) -> bool {
    let mut after_edit = false;
    for msg in &convo.messages[edit.turn_start..] {
        if msg.role == Role::Tool
            && msg.tool_call_id.as_deref() == Some(edit.edit_id.as_str())
            && !msg.is_error
        {
            after_edit = true;
            continue;
        }
        if after_edit
            && msg.role == Role::User
            && msg.synthetic
            && msg.text.trim_start().starts_with(NUDGE)
        {
            return true;
        }
    }
    false
}

#[async_trait]
impl LifecycleHooks for VerifyCadenceHook {
    /// The hook instance is REUSED across respawns (it lives in `CodingParts`), but
    /// `nudged_for` is per-CONVERSATION state keyed by tool_call_id — and providers
    /// with sequential per-conversation ids (`call_0`, `call_1`, …) would collide a
    /// FRESH conversation's first edit with the old one's last nudge, wrongly
    /// suppressing it once. A fresh session start resets; a resume keeps the state
    /// (same conversation → an already-nudged edit must stay nudged).
    async fn session_start(&self, _convo: &mut Conversation, resumed: bool) {
        if !resumed {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .nudged_for = None;
        }
    }

    async fn offer_continuation(&self, convo: &Conversation) -> Option<String> {
        self.offer_typed_continuation(convo).await.map(|c| c.text)
    }

    async fn offer_typed_continuation(&self, convo: &Conversation) -> Option<Continuation> {
        // Attended (interactive) runs — or an explicit `ATOMCODE_VERIFY=0` — do not FORCE a
        // post-edit verify continuation: a present human can ask for the check. Unattended /
        // headless / scheduled runs keep the cadence. (See `should_suppress_verify`.)
        if self.suppress_verify_continuation {
            return None;
        }
        if self.execution_policy.current().skips_verification()
            || execution_policy_for_messages(&convo.messages).skips_verification()
        {
            return None;
        }
        let edit = unverified_edit(convo, &self.workspace)?;
        if verify_reminder_already_present(convo, &edit) {
            return None;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.nudged_for.as_ref() == Some(&edit) {
            return None; // already nudged for this exact edit — let the turn stop.
        }
        state.nudged_for = Some(edit);
        Some(Continuation::verify_cadence(NUDGE.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::tool::ToolCall;

    fn assistant_call(id: &str, name: &str) -> Message {
        Message::assistant(
            "",
            vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: "{}".into(),
            }],
        )
    }
    fn user(text: &str) -> Message {
        Message::user(text)
    }
    fn synthetic_user(text: &str) -> Message {
        Message::synthetic_user(text)
    }
    /// A `bash` tool call carrying a real command (so `bash_verifies` can classify it).
    fn bash_call(id: &str, cmd: &str) -> Message {
        let args = serde_json::json!({ "command": cmd }).to_string();
        Message::assistant(
            "",
            vec![ToolCall {
                id: id.into(),
                name: "bash".into(),
                arguments: args,
            }],
        )
    }
    fn tool_result(id: &str, is_error: bool) -> Message {
        Message::tool_result(id, "ok", is_error)
    }

    fn assistant_call_path(id: &str, name: &str, path: &str) -> Message {
        let args = serde_json::json!({ "file_path": path }).to_string();
        Message::assistant(
            "",
            vec![ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args,
            }],
        )
    }

    /// Test hook whose workspace is `/` — every absolute path is in-workspace, so path gating
    /// never suppresses these path-agnostic cases (they use relative / empty targets).
    fn hook_any_ws() -> VerifyCadenceHook {
        VerifyCadenceHook::new("/")
    }

    #[test]
    fn should_suppress_verify_resolves_env_and_attendedness() {
        // No env → follow attendedness: interactive suppresses the forced check, headless forces it.
        assert!(should_suppress_verify(None, true), "interactive → suppress");
        assert!(!should_suppress_verify(None, false), "headless → force");
        // `ATOMCODE_VERIFY` wins in BOTH directions, regardless of attendedness.
        assert!(
            should_suppress_verify(Some("0"), false),
            "=0 suppresses even in an unattended run"
        );
        assert!(should_suppress_verify(Some("off"), false));
        assert!(
            !should_suppress_verify(Some("1"), true),
            "=1 forces even in an interactive run"
        );
        assert!(!should_suppress_verify(Some("on"), true));
        // Unrecognized / empty → fall back to attendedness (not an override).
        assert!(should_suppress_verify(Some(""), true));
        assert!(!should_suppress_verify(Some("maybe"), false));
    }

    #[tokio::test]
    async fn attended_run_suppresses_verify_continuation() {
        // An unverified edit that WOULD nudge unattended must NOT force a continuation when
        // attended (interactive) — the human present sees the edit and can request a check.
        let hook = hook_any_ws().with_suppression(true);
        let mut convo = Conversation::new();
        convo.messages = vec![assistant_call("e1", "edit_file"), tool_result("e1", false)];
        assert!(
            hook.offer_continuation(&convo).await.is_none(),
            "attended run must not force a post-edit verify continuation"
        );
    }

    #[tokio::test]
    async fn unattended_run_still_nudges() {
        // The default (unattended / headless) keeps the forcing cadence.
        let hook = hook_any_ws().with_suppression(false);
        let mut convo = Conversation::new();
        convo.messages = vec![assistant_call("e1", "edit_file"), tool_result("e1", false)];
        assert!(
            hook.offer_continuation(&convo).await.is_some(),
            "unattended run keeps the verify cadence"
        );
    }

    async fn nudge_of(msgs: Vec<Message>) -> (VerifyCadenceHook, Option<String>) {
        let mut convo = Conversation::new();
        convo.messages = msgs;
        let hook = hook_any_ws();
        let r = hook.offer_continuation(&convo).await;
        (hook, r)
    }

    #[tokio::test]
    async fn edit_without_build_nudges_once() {
        let msgs = vec![assistant_call("e1", "edit_file"), tool_result("e1", false)];
        let (hook, first) = nudge_of(msgs.clone()).await;
        assert!(first.is_some(), "unverified edit must nudge");
        // Calling again on the SAME conversation must NOT nudge twice.
        let mut convo = Conversation::new();
        convo.messages = msgs;
        assert!(
            hook.offer_continuation(&convo).await.is_none(),
            "must not nudge twice for the same edit"
        );
    }

    #[tokio::test]
    async fn explicit_user_execution_limit_suppresses_verify_nudge() {
        for instruction in [
            "修改代码，但禁止编译和禁止执行脚本",
            "Make the edit, but do not run tests.",
            "不要运行任何命令，只修改文件",
            "直接写完代码 不验证",
        ] {
            let msgs = vec![
                user(instruction),
                assistant_call("e1", "edit_file"),
                tool_result("e1", false),
            ];
            assert!(
                nudge_of(msgs).await.1.is_none(),
                "must suppress verify cadence for {instruction:?}"
            );
        }
    }

    #[tokio::test]
    async fn live_steer_policy_suppresses_nudge_before_conversation_catches_up() {
        let policy = Arc::new(TurnExecutionPolicy::new());
        let hook = VerifyCadenceHook::with_execution_policy("/", policy.clone());
        let mut convo = Conversation::new();
        convo.messages = vec![assistant_call("e1", "edit_file"), tool_result("e1", false)];
        policy.update_from_user_text("现在停止验证，不要运行测试");
        assert!(hook.offer_continuation(&convo).await.is_none());
    }

    #[tokio::test]
    async fn edit_then_successful_check_does_not_nudge() {
        let msgs = vec![
            assistant_call("e1", "edit_file"),
            tool_result("e1", false),
            bash_call("b1", "cargo check"),
            tool_result("b1", false),
        ];
        assert!(
            nudge_of(msgs).await.1.is_none(),
            "a real check after the edit verifies it"
        );
    }

    #[tokio::test]
    async fn edit_then_readonly_bash_still_nudges() {
        // The dodge this change closes: a throwaway `ls`/`echo` after an edit is NOT a check,
        // so the edit is still unverified and must nudge.
        for dodge in ["ls -la", "echo done", "cat src/main.rs", "pwd"] {
            let msgs = vec![
                assistant_call("e1", "edit_file"),
                tool_result("e1", false),
                bash_call("b1", dodge),
                tool_result("b1", false),
            ];
            assert!(
                nudge_of(msgs).await.1.is_some(),
                "read-only `{dodge}` must not count as verification"
            );
        }
    }

    #[tokio::test]
    async fn chained_or_unknown_bash_after_edit_verifies() {
        // A build behind a `cd`, or any non-denylisted command, counts (conservative).
        for cmd in [
            "cd sub && cargo test",
            "npm run build",
            "./gradlew build",
            "pytest -q",
        ] {
            let msgs = vec![
                assistant_call("e1", "edit_file"),
                tool_result("e1", false),
                bash_call("b1", cmd),
                tool_result("b1", false),
            ];
            assert!(
                nudge_of(msgs).await.1.is_none(),
                "real check `{cmd}` must verify the edit"
            );
        }
    }

    #[test]
    fn bash_verifies_excludes_readonly_but_counts_real_checks() {
        // Read-only / review — do NOT verify, even chained or piped.
        assert!(!bash_verifies("ls -la"));
        assert!(!bash_verifies("echo done"));
        assert!(!bash_verifies("/usr/bin/cat foo")); // path-stripped head
        assert!(!bash_verifies("   ")); // empty
        assert!(!bash_verifies("git diff")); // review, not verification
        assert!(!bash_verifies("git status && git log")); // all read-only
        assert!(!bash_verifies("cat x | grep y")); // read-only pipe
        assert!(
            !bash_verifies("cd x && ls"),
            "read-only chained commands do not verify"
        );
        // Real checks — verify, including behind env prefixes / wrappers / chains.
        assert!(bash_verifies("cargo check"));
        assert!(bash_verifies("tsc --noEmit"));
        assert!(bash_verifies("make test"));
        assert!(bash_verifies("cd x && cargo check")); // a work segment in the chain
        assert!(bash_verifies("cargo test | grep -i pass")); // the test DID run
        assert!(bash_verifies("FOO=1 cargo test")); // env-assign prefix stripped
        assert!(bash_verifies("env RUST_LOG=info cargo check")); // wrapper + assign
    }

    #[tokio::test]
    async fn failed_check_after_edit_still_nudges() {
        // A bash that ERRORED does not count as verification.
        let msgs = vec![
            assistant_call("e1", "edit_file"),
            tool_result("e1", false),
            bash_call("b1", "cargo check"),
            tool_result("b1", true),
        ];
        assert!(
            nudge_of(msgs).await.1.is_some(),
            "errored check is not verification"
        );
    }

    #[tokio::test]
    async fn write_file_counts_as_edit() {
        let msgs = vec![assistant_call("w1", "write_file"), tool_result("w1", false)];
        assert!(nudge_of(msgs).await.1.is_some());
    }

    #[tokio::test]
    async fn write_outside_workspace_does_not_nudge() {
        // The reported misfire: `write_file` to /tmp is a throwaway file, not project code —
        // it must NOT arm the "run cargo check" cadence.
        let hook = VerifyCadenceHook::new("/home/proj");
        let mut convo = Conversation::new();
        convo.messages = vec![
            assistant_call_path("w1", "write_file", "/tmp/test_permission.txt"),
            tool_result("w1", false),
        ];
        assert!(
            hook.offer_continuation(&convo).await.is_none(),
            "writing outside the workspace (/tmp) must not trigger the verify cadence"
        );
    }

    #[tokio::test]
    async fn write_inside_workspace_still_nudges() {
        let hook = VerifyCadenceHook::new("/home/proj");
        let mut convo = Conversation::new();
        convo.messages = vec![
            assistant_call_path("w1", "write_file", "/home/proj/src/main.rs"),
            tool_result("w1", false),
        ];
        assert!(
            hook.offer_continuation(&convo).await.is_some(),
            "an absolute in-workspace edit must still nudge"
        );
    }

    #[tokio::test]
    async fn relative_edit_is_in_workspace_and_nudges() {
        // Relative targets resolve against the workspace cwd → in-workspace by construction.
        let hook = VerifyCadenceHook::new("/home/proj");
        let mut convo = Conversation::new();
        convo.messages = vec![
            assistant_call_path("e1", "edit_file", "src/main.rs"),
            tool_result("e1", false),
        ];
        assert!(
            hook.offer_continuation(&convo).await.is_some(),
            "a relative edit resolves inside the workspace and must nudge"
        );
    }

    #[tokio::test]
    async fn relative_parent_escape_out_of_workspace_does_not_nudge() {
        // `../../tmp/x` lexically escapes the workspace root → outside → no cadence.
        let hook = VerifyCadenceHook::new("/home/proj");
        let mut convo = Conversation::new();
        convo.messages = vec![
            assistant_call_path("w1", "write_file", "../../tmp/x.txt"),
            tool_result("w1", false),
        ];
        assert!(
            hook.offer_continuation(&convo).await.is_none(),
            "a relative path escaping the workspace via `..` must not nudge"
        );
    }

    #[tokio::test]
    async fn unparseable_edit_path_is_conservatively_in_workspace() {
        // No file_path in the args (can't classify) → keep the cadence rather than skip it.
        let hook = VerifyCadenceHook::new("/home/proj");
        let mut convo = Conversation::new();
        convo.messages = vec![assistant_call("e1", "edit_file"), tool_result("e1", false)];
        assert!(
            hook.offer_continuation(&convo).await.is_some(),
            "an edit with no parseable path stays in-workspace (conservative) and nudges"
        );
    }

    #[test]
    fn path_in_workspace_lexical_classifies_paths() {
        let ws = Path::new("/home/proj");
        // Inside.
        assert!(path_in_workspace_lexical("/home/proj/src/main.rs", ws));
        assert!(path_in_workspace_lexical("src/main.rs", ws)); // relative → joined to ws
        assert!(path_in_workspace_lexical("./a/b.rs", ws));
        assert!(path_in_workspace_lexical("/home/proj/./sub/../x.rs", ws)); // normalizes inside
                                                                            // Outside.
        assert!(!path_in_workspace_lexical("/tmp/test.txt", ws));
        assert!(!path_in_workspace_lexical("/home/other/x.rs", ws));
        assert!(!path_in_workspace_lexical("../sibling/x.rs", ws)); // escapes via ..
        assert!(!path_in_workspace_lexical("/home/proj/../evil.rs", ws)); // climbs out
                                                                          // Sibling-prefix must not false-match (/home/proj2 is NOT under /home/proj).
        assert!(!path_in_workspace_lexical("/home/proj2/x.rs", ws));
        // Empty / unparseable → conservative in-workspace.
        assert!(path_in_workspace_lexical("", ws));
    }

    #[test]
    fn relative_or_empty_workspace_root_disables_gate_conservatively() {
        // A non-absolute root can't anchor a prefix test — an absolute edit path would never
        // match it and the cadence would silently vanish. Bail to "in-workspace" (old behavior)
        // instead, so an absolute /tmp write still nudges rather than being wrongly skipped.
        for root in ["proj", ".", "", "../proj"] {
            let ws = Path::new(root);
            assert!(
                path_in_workspace_lexical("/tmp/x.txt", ws),
                "relative/empty workspace root {root:?} must not gate (stay conservative)"
            );
            assert!(
                path_in_workspace_lexical("/home/proj/src/main.rs", ws),
                "relative/empty workspace root {root:?} must not suppress a real edit"
            );
        }
    }

    #[tokio::test]
    async fn no_edits_does_not_nudge() {
        let msgs = vec![assistant_call("r1", "read_file"), tool_result("r1", false)];
        assert!(nudge_of(msgs).await.1.is_none());
    }

    #[tokio::test]
    async fn writing_a_markdown_report_does_not_nudge() {
        // Reported misfire: a non-coding turn that writes a markdown report (a weekly report)
        // must NOT arm "run cargo check" — a `.md` is prose, not compilable code.
        let hook = VerifyCadenceHook::new("/home/proj");
        let mut convo = Conversation::new();
        convo.messages = vec![
            assistant_call_path("w1", "write_file", "/home/proj/report_2026W26.md"),
            tool_result("w1", false),
        ];
        assert!(
            hook.offer_continuation(&convo).await.is_none(),
            "writing a markdown report must not trigger the verify cadence"
        );
    }

    #[tokio::test]
    async fn weekly_report_turn_with_verified_scripts_then_md_does_not_nudge() {
        // The exact reported sequence: throwaway analysis scripts (RUN via node → verified),
        // then the final markdown report (doc → does not arm). The turn must not nudge, so the
        // model never reaches for a previous coding task's test suite.
        let hook = VerifyCadenceHook::new("/home/proj");
        let mut convo = Conversation::new();
        convo.messages = vec![
            user("analyze my week then write my weekly report"),
            assistant_call_path("w1", "write_file", "/home/proj/_activity.js"),
            tool_result("w1", false),
            bash_call("b1", "node _activity.js"),
            tool_result("b1", false),
            assistant_call_path("w2", "write_file", "/home/proj/_week.js"),
            tool_result("w2", false),
            bash_call("b2", "node _week.js"),
            tool_result("b2", false),
            assistant_call_path("w3", "write_file", "/home/proj/report_2026W26.md"),
            tool_result("w3", false),
        ];
        assert!(
            hook.offer_continuation(&convo).await.is_none(),
            "a report turn whose only unverified write is a .md doc must not nudge"
        );
    }

    #[tokio::test]
    async fn unverified_markdown_before_a_real_source_edit_still_nudges() {
        // The doc skip must not mask a genuine unverified SOURCE edit later in the turn.
        let hook = VerifyCadenceHook::new("/home/proj");
        let mut convo = Conversation::new();
        convo.messages = vec![
            assistant_call_path("w1", "write_file", "/home/proj/notes.md"),
            tool_result("w1", false),
            assistant_call_path("e1", "edit_file", "/home/proj/src/main.rs"),
            tool_result("e1", false),
        ];
        assert!(
            hook.offer_continuation(&convo).await.is_some(),
            "an unverified source edit after a doc write must still nudge"
        );
    }

    #[test]
    fn path_is_noncode_doc_classifies_extensions() {
        for doc in [
            "report.md",
            "a.markdown",
            "notes.txt",
            "data.csv",
            "run.log",
            "/x/y.MD",
        ] {
            assert!(path_is_noncode_doc(doc), "{doc} should be a non-code doc");
        }
        for code in [
            "main.rs",
            "app.ts",
            "x.py",
            "index.html",
            "Cargo.toml",
            "package.json",
            "noext",
        ] {
            assert!(
                !path_is_noncode_doc(code),
                "{code} must stay verifiable (arms cadence)"
            );
        }
    }

    #[tokio::test]
    async fn prior_turn_unverified_edit_does_not_nudge_later_real_user_turn() {
        let msgs = vec![
            user("create a file"),
            assistant_call("e1", "write_file"),
            tool_result("e1", false),
            Message::assistant("created", vec![]),
            user("what model are you"),
            Message::assistant("I am AtomCode.", vec![]),
        ];

        assert!(
            nudge_of(msgs).await.1.is_none(),
            "verify cadence must not carry a previous real user's edit into a later real user turn"
        );
    }

    #[tokio::test]
    async fn synthetic_user_message_does_not_reset_current_turn_scope() {
        let msgs = vec![
            user("create a file"),
            assistant_call("e1", "write_file"),
            tool_result("e1", false),
            synthetic_user("[Additional context from user]: keep going"),
        ];

        assert!(
            nudge_of(msgs).await.1.is_some(),
            "synthetic context messages attach to the current real user turn and must not hide the edit"
        );
    }

    #[tokio::test]
    async fn existing_verify_reminder_marker_suppresses_repeat_after_resume() {
        let msgs = vec![
            user("create a file"),
            assistant_call("e1", "write_file"),
            tool_result("e1", false),
            synthetic_user(NUDGE),
        ];

        let hook = VerifyCadenceHook::default();
        let mut convo = Conversation::new();
        convo.messages = msgs;
        assert!(
            hook.offer_continuation(&convo).await.is_none(),
            "a persisted synthetic verify reminder means this edit already got one internal verify chance"
        );
    }

    #[tokio::test]
    async fn build_then_edit_is_unverified() {
        // bash BEFORE the edit does not verify the later edit.
        let msgs = vec![
            assistant_call("b1", "bash"),
            tool_result("b1", false),
            assistant_call("e1", "edit_file"),
            tool_result("e1", false),
        ];
        assert!(
            nudge_of(msgs).await.1.is_some(),
            "build must come AFTER the edit"
        );
    }

    #[tokio::test]
    async fn fresh_edit_after_nudge_retriggers() {
        let hook = hook_any_ws();
        let mut convo = Conversation::new();
        convo.messages = vec![assistant_call("e1", "edit_file"), tool_result("e1", false)];
        assert!(
            hook.offer_continuation(&convo).await.is_some(),
            "first edit nudges"
        );
        assert!(
            hook.offer_continuation(&convo).await.is_none(),
            "same edit, no second nudge"
        );
        // A NEW edit (different id) appears → nudge again.
        convo.messages.push(assistant_call("e2", "edit_file"));
        convo.messages.push(tool_result("e2", false));
        assert!(
            hook.offer_continuation(&convo).await.is_some(),
            "a fresh unverified edit re-triggers"
        );
    }

    #[tokio::test]
    async fn repeated_tool_call_id_in_new_real_user_turn_still_nudges() {
        let hook = hook_any_ws();
        let mut convo = Conversation::new();
        convo.messages = vec![
            user("first edit"),
            assistant_call("e1", "edit_file"),
            tool_result("e1", false),
        ];
        assert!(
            hook.offer_continuation(&convo).await.is_some(),
            "first turn should nudge"
        );

        convo.messages.extend([
            synthetic_user(NUDGE),
            Message::assistant("No verification is needed.", vec![]),
            user("second edit"),
            assistant_call("e1", "edit_file"),
            tool_result("e1", false),
        ]);

        assert!(
            hook.offer_continuation(&convo).await.is_some(),
            "same tool_call_id in a new real user turn is a new edit and must nudge"
        );
    }
}
