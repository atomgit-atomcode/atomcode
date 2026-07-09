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

use async_trait::async_trait;
use atomcode_kernel::hook::{Continuation, LifecycleHooks};
use atomcode_kernel::message::{Conversation, Role};
use std::collections::HashMap;
use std::sync::Mutex;

const NUDGE: &str = "You made code edits but have not verified them. Run a fast check \
(`cargo check`, `tsc --noEmit`, or the equivalent for this project) to catch errors \
before finishing. Do NOT start a long-running process (dev server, watcher, full build).";

/// `offer_continuation` hook implementing the edit-then-verify cadence. Holds a small amount of
/// interior state so it nudges at most once per unverified edit.
#[derive(Default)]
pub struct VerifyCadenceHook {
    state: Mutex<State>,
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
    pub fn new() -> Self {
        Self::default()
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
fn unverified_edit(convo: &Conversation) -> Option<NudgedEdit> {
    let start = current_real_user_start(convo);
    // Tool-call ids are assigned by the assistant message that precedes the matching
    // tool-result message, so a single forward pass can resolve a result's tool name.
    let mut names: HashMap<&str, &str> = HashMap::new();
    // bash tool_call id → its command string (to tell a real check from an `ls` dodge).
    let mut bash_cmds: HashMap<&str, String> = HashMap::new();
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
                    Some("edit_file") | Some("write_file") => {
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
        let edit = unverified_edit(convo)?;
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

    async fn nudge_of(msgs: Vec<Message>) -> (VerifyCadenceHook, Option<String>) {
        let mut convo = Conversation::new();
        convo.messages = msgs;
        let hook = VerifyCadenceHook::new();
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
    async fn no_edits_does_not_nudge() {
        let msgs = vec![assistant_call("r1", "read_file"), tool_result("r1", false)];
        assert!(nudge_of(msgs).await.1.is_none());
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
        let hook = VerifyCadenceHook::new();
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
        let hook = VerifyCadenceHook::new();
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
