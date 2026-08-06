//! `TranscriptHook` — appends ONE raw record per completed turn to `<id>.jsonl`, the
//! never-compacted ground truth for cross-session recall.
//!
//! It ACCUMULATES the turn across rounds (`on_model_response`) and FLUSHES exactly once
//! at the turn's terminal (`turn_complete`) — so it is robust to compaction (it never
//! diffs history post-hoc) AND it captures EVERY terminal (normal stop, error, cancel,
//! timeout, fuse), because `turn_complete` fires on all of them. Tool RESULTS are paired
//! to the turn's calls BY `tool_call_id` from the live conversation at flush time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message, Role};
use serde::{Deserialize, Serialize};

use super::{now_ms, snapshot::SnapshotPersistenceStatus, SessionManager, SessionResult};

pub const RECORD_VERSION: u32 = 1;

/// One completed turn, RAW (no redaction) — one JSON object per `<id>.jsonl` line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    /// `.jsonl` RECORD SCHEMA VERSION — same forward-compat seam as
    /// `SessionMeta.v`: new records write 1, pre-version lines read as 0
    /// (`serde(default)`); additive fields keep the `v`, breaking changes bump it.
    #[serde(default)]
    pub v: u32,
    /// epoch MILLISECONDS, UTC — stamped by L1 at flush.
    pub ts: i64,
    /// Human-readable RFC-3339 mirror of `ts` (display / debug).
    pub iso: String,
    pub session_id: String,
    /// Kernel `TurnCtx.turn_id` (monotonic within the session).
    pub turn_id: u64,
    /// RESERVED for `/undo`: a turn rewound past by a later `/undo` stays in the
    /// transcript (decision: kept, not deleted) flagged `undone`. v1 always writes
    /// `false` — the marking mechanism is DEFERRED with the rest of `/undo` wiring
    /// (the append-only jsonl needs a side index or a rewrite pass to set it after the
    /// fact); the field + the recall display are forward-compatible for when it lands.
    #[serde(default)]
    pub undone: bool,
    /// The original user prompt text (raw). A mid-turn synthetic continuation
    /// (`offer_continuation`) is NOT recorded as user input.
    pub user: String,
    /// Final assistant text across ALL rounds of the turn (raw).
    pub assistant: String,
    /// Concatenated reasoning across rounds, if any.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default)]
    pub tools: Vec<ToolRecord>,
    pub usage: UsageRecord,
}

/// One tool call + its paired result within a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolRecord {
    pub name: String,
    pub args: String,
    pub result: String,
    #[serde(default)]
    pub is_error: bool,
}

/// Turn-aggregated token usage.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub prompt: u32,
    pub completion: u32,
    #[serde(default)]
    pub cached: u32,
}

/// In-progress accumulation for the current turn (None between turns).
#[derive(Default)]
struct TurnBuffer {
    user: String,
    assistant: String,
    reasoning: String,
    turn_id: u64,
    /// Calls seen this turn (id used to pair the result at flush); results filled later.
    tools: Vec<PendingTool>,
    usage: UsageRecord,
}

struct PendingTool {
    id: String,
    name: String,
    args: String,
}

/// Appends a `TurnRecord` per completed turn to `<id>.jsonl`.
pub struct TranscriptHook {
    mgr: Arc<SessionManager>,
    session_id: String,
    buf: Mutex<Option<TurnBuffer>>,
    persistence_status: Option<SnapshotPersistenceStatus>,
}

impl TranscriptHook {
    pub fn new(mgr: Arc<SessionManager>, session_id: impl Into<String>) -> Self {
        Self {
            mgr,
            session_id: session_id.into(),
            buf: Mutex::new(None),
            persistence_status: None,
        }
    }

    pub fn with_persistence_status(mut self, status: SnapshotPersistenceStatus) -> Self {
        self.persistence_status = Some(status);
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<TurnBuffer>> {
        // Never panic on a poisoned lock (the PANIC CONTRACT): recover the inner guard.
        self.buf.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Build the record from the buffer + harvest tool results from `convo` by id, then
    /// append it. Returns the buffer to None. A turn with NO assistant text and NO tools
    /// (a rejected prompt, or a turn that failed before any model output) is dropped —
    /// nothing meaningful happened to record.
    fn flush(&self, convo: &Conversation) {
        let buffer = { self.lock().take() };
        let Some(b) = buffer else { return };
        if b.assistant.is_empty() && b.tools.is_empty() {
            return;
        }

        // Pair each call's result by tool_call_id from the live conversation.
        let mut results: HashMap<&str, (&str, bool)> = HashMap::new();
        for m in &convo.messages {
            if m.role == Role::Tool {
                if let Some(id) = &m.tool_call_id {
                    results.insert(id.as_str(), (m.text.as_str(), m.is_error));
                }
            }
        }
        let tools: Vec<ToolRecord> = b
            .tools
            .iter()
            .map(|t| {
                let (result, is_error) = results
                    .get(t.id.as_str())
                    .map(|(r, e)| (r.to_string(), *e))
                    .unwrap_or_default();
                ToolRecord {
                    name: t.name.clone(),
                    args: t.args.clone(),
                    result,
                    is_error,
                }
            })
            .collect();

        let ts = now_ms();
        let record = TurnRecord {
            v: RECORD_VERSION,
            ts,
            iso: chrono::DateTime::from_timestamp_millis(ts)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default(),
            session_id: self.session_id.clone(),
            turn_id: b.turn_id,
            undone: false,
            user: b.user,
            assistant: b.assistant,
            reasoning: b.reasoning,
            tools,
            usage: b.usage,
        };
        // The transcript is auxiliary to the committed native aggregate, so its
        // failure must not fail the turn. It must still be visible to the driver:
        // silently dropping ENOSPC here made missing history look successful.
        if let Err(error) = self.append(&record) {
            let guidance = if error.kind() == std::io::ErrorKind::PermissionDenied {
                "check file permissions, read-only attributes, security software, and whether another AtomCode process is using the file"
            } else {
                "check available disk space and file permissions"
            };
            let warning = format!("session transcript was not saved; {guidance}: {error}");
            eprintln!("[TranscriptHook] {warning}");
            if let Some(status) = &self.persistence_status {
                status.report_auxiliary_warning(warning);
            }
        }
    }

    fn append(&self, record: &TurnRecord) -> SessionResult<()> {
        let mut line =
            serde_json::to_vec(record).map_err(|e| super::SessionStoreError::Corrupt {
                kind: "transcript record",
                message: e.to_string(),
            })?;
        line.push(b'\n');
        self.mgr.append_jsonl_line(&self.session_id, &line)
    }
}

#[async_trait]
impl LifecycleHooks for TranscriptHook {
    /// Start a fresh turn buffer, recording the (post-hook) user text.
    async fn user_prompt_submit(&self, text: &mut String) -> Result<(), String> {
        let mut g = self.lock();
        *g = Some(TurnBuffer {
            user: text.clone(),
            ..TurnBuffer::default()
        });
        Ok(())
    }

    /// Accumulate this round's assistant text / reasoning / tool calls / usage, and
    /// capture the kernel-minted `turn_id` from the message meta.
    async fn on_model_response(&self, response: &mut Message) {
        let mut g = self.lock();
        let b = g.get_or_insert_with(TurnBuffer::default);
        if !response.text.is_empty() {
            b.assistant.push_str(&response.text);
        }
        if let Some(r) = &response.reasoning {
            b.reasoning.push_str(r);
        }
        for tc in &response.tool_calls {
            b.tools.push(PendingTool {
                id: tc.id.clone(),
                name: tc.name.clone(),
                args: tc.arguments.clone(),
            });
        }
        if let Some(meta) = &response.meta {
            b.turn_id = meta.turn_id;
            // Turn-aggregate usage: the last round's `prompt` is the cumulative context
            // the turn reached; `completion` sums the output produced across rounds.
            b.usage.prompt = meta.tokens.prompt;
            b.usage.completion = b.usage.completion.saturating_add(meta.tokens.completion);
            b.usage.cached = b.usage.cached.max(meta.tokens.cached);
        }
    }

    /// The turn TERMINATED (any reason): write its one transcript line. Fires on every
    /// terminal — normal stop AND error/cancel/timeout/fuse — so an errored turn that
    /// produced partial output is still recorded.
    async fn turn_complete(&self, convo: &Conversation, _reason: &StopReason, _ctx: &TurnCtx) {
        self.flush(convo);
    }

    /// Defensive flush at session end for any buffer a terminal somehow missed (with
    /// `turn_complete` firing on all paths this is normally a no-op).
    async fn session_end(&self, convo: &Conversation) {
        self.flush(convo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::{Message, MessageMeta};
    use atomcode_kernel::stream::TokenUsage;
    use atomcode_kernel::tool::ToolCall;

    fn hook(id: &str) -> (TranscriptHook, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(SessionManager::with_root(dir.path()));
        (TranscriptHook::new(mgr, id), dir)
    }

    fn assistant(text: &str, calls: Vec<ToolCall>, turn_id: u64) -> Message {
        let mut m = Message::assistant(text, calls);
        m.meta = Some(MessageMeta {
            turn_id,
            tokens: TokenUsage {
                prompt: 100,
                completion: 20,
                cached: 0,
            },
            ..Default::default()
        });
        m
    }

    fn read_lines(h: &TranscriptHook) -> Vec<TurnRecord> {
        let path = h.mgr.jsonl_path(&h.session_id).unwrap();
        let Ok(content) = std::fs::read_to_string(path) else {
            return vec![];
        };
        content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn writes_one_record_per_turn_with_paired_tool_results() {
        let (h, _d) = hook("s1");
        let mut text = "list the files".to_string();
        h.user_prompt_submit(&mut text).await.unwrap();

        let call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{\"cmd\":\"ls\"}".into(),
        };
        let mut resp = assistant("running ls", vec![call], 7);
        h.on_model_response(&mut resp).await;

        // The conversation at terminal carries the tool RESULT paired by id.
        let mut convo = Conversation::new();
        convo.push(Message::user("list the files"));
        convo.push(resp.clone());
        convo.push(Message::tool_result("c1", "a.txt\nb.txt", false));

        h.turn_complete(&convo, &StopReason::Stopped, &TurnCtx::default())
            .await;

        let recs = read_lines(&h);
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.session_id, "s1");
        assert_eq!(r.turn_id, 7);
        assert_eq!(r.user, "list the files");
        assert_eq!(r.assistant, "running ls");
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.tools[0].name, "bash");
        assert_eq!(r.tools[0].result, "a.txt\nb.txt");
        assert!(!r.tools[0].is_error);
        assert_eq!(r.usage.prompt, 100);
        assert!(!r.iso.is_empty());
    }

    #[tokio::test]
    async fn concatenates_assistant_text_across_rounds() {
        let (h, _d) = hook("s1");
        let mut text = "do it".to_string();
        h.user_prompt_submit(&mut text).await.unwrap();
        h.on_model_response(&mut assistant("first ", vec![], 1))
            .await;
        h.on_model_response(&mut assistant("second", vec![], 1))
            .await;
        let convo = Conversation::new();
        h.turn_complete(&convo, &StopReason::Stopped, &TurnCtx::default())
            .await;

        let recs = read_lines(&h);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].assistant, "first second");
        assert_eq!(
            recs[0].usage.completion, 40,
            "completion sums across rounds"
        );
    }

    #[tokio::test]
    async fn drops_a_rejected_or_empty_turn() {
        let (h, _d) = hook("s1");
        let mut text = "blocked prompt".to_string();
        h.user_prompt_submit(&mut text).await.unwrap();
        // No on_model_response (prompt rejected / instant fail) → terminal.
        let convo = Conversation::new();
        h.turn_complete(&convo, &StopReason::PromptRejected, &TurnCtx::default())
            .await;
        assert!(
            read_lines(&h).is_empty(),
            "an empty turn must not be recorded"
        );
    }

    #[tokio::test]
    async fn records_an_errored_turn_with_partial_assistant() {
        let (h, _d) = hook("s1");
        let mut text = "go".to_string();
        h.user_prompt_submit(&mut text).await.unwrap();
        h.on_model_response(&mut assistant("partial answer", vec![], 3))
            .await;
        let convo = Conversation::new();
        h.turn_complete(&convo, &StopReason::ProviderError, &TurnCtx::default())
            .await;

        let recs = read_lines(&h);
        assert_eq!(
            recs.len(),
            1,
            "an errored turn with output is still recorded"
        );
        assert_eq!(recs[0].assistant, "partial answer");
    }

    #[tokio::test]
    async fn transcript_write_failure_is_reported_without_failing_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SessionManager::with_root(dir.path()));
        let status = SnapshotPersistenceStatus::default();
        let hook =
            TranscriptHook::new(manager, "../invalid").with_persistence_status(status.clone());
        let mut text = "keep the turn alive".to_string();
        hook.user_prompt_submit(&mut text).await.unwrap();
        hook.on_model_response(&mut assistant("answer", vec![], 1))
            .await;

        hook.turn_complete(
            &Conversation::new(),
            &StopReason::Stopped,
            &TurnCtx::default(),
        )
        .await;

        let warning = status.take_auxiliary_warning().unwrap();
        assert!(warning.contains("transcript was not saved"));
        assert!(warning.contains("invalid session id"));
        assert!(status.take_uncertain_commit().is_none());
    }
}
