//! Sub-agent parallel execution for multi-file tasks.
//!
//! Each SubAgent handles one file with its own Conversation + TurnRunner,
//! running in parallel via tokio::JoinSet. This keeps each sub-agent's
//! context small (~3-4K tokens) so weak models perform well.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::conversation::Conversation;
use crate::provider::LlmProvider;
use crate::tool::{ToolContext, ToolRegistry};
use crate::turn::event::{TurnEvent, TurnResult};
use crate::turn::permission::{AutoPermissionDecider, AutoPermissionMode};
use crate::turn::runner::TurnRunner;

/// Tunable knobs for the resilience layer of `SubAgentTask::execute`.
/// Wired from `Config::subagent` at the call site; defaults match
/// `SubAgentConfig::default()`.
#[derive(Debug, Clone)]
pub struct ResilienceConfig {
    /// Starting per-task turn budget.
    pub initial_turns: usize,
    /// Hard cap regardless of progress signals.
    pub max_turns: usize,
    /// Minimum turns to run before honoring budget exhaustion (so a
    /// single bad turn can't end the sub-agent prematurely).
    pub min_turns: usize,
    /// Budget bonus when a turn produced a successful edit.
    pub edit_bonus: usize,
    /// Budget penalty when no_edit_runs ≥ idle_threshold.
    pub idle_penalty: usize,
    /// Number of consecutive no-edit turns before idle penalty applies.
    pub idle_threshold: usize,
    /// Number of consecutive no-edit turns that triggers early kill
    /// (NoProgress failure).
    pub idle_kill_threshold: usize,
    /// Max in-loop retries for stream-timeout class failures (network).
    pub max_call_retries: usize,
    /// Reads of the assigned file (with zero successful edits) that
    /// triggers the hallucination nudge.
    pub hallucination_read_threshold: usize,
}

impl Default for ResilienceConfig {
    fn default() -> Self {
        Self {
            initial_turns: 4,
            max_turns: 12,
            min_turns: 2,
            edit_bonus: 2,
            idle_penalty: 1,
            idle_threshold: 2,
            idle_kill_threshold: 4,
            max_call_retries: 1,
            hallucination_read_threshold: 3,
        }
    }
}

/// Per-execute progress signals. Updated each turn by `scan_turn_signals`.
/// Drives the adaptive budget calculation and hallucination nudge.
#[derive(Debug, Default, Clone)]
struct ProgressTracker {
    /// All files that received at least one successful edit.
    edited_files: std::collections::HashSet<String>,
    /// Turn index of most-recent successful edit.
    last_edit_turn: Option<usize>,
    /// Consecutive turns with zero successful edits.
    no_edit_runs: usize,
    /// Per-file read-call counts (regardless of success/failure).
    read_count: std::collections::HashMap<String, usize>,
    /// How many stream-timeout retries have fired so far.
    timeouts: usize,
    /// How many hallucination nudges have been injected.
    hallucination_nudges_sent: usize,
}

impl ProgressTracker {
    fn observe_turn(
        &mut self,
        turn_idx: usize,
        edited: &[String],
        reads: &[String],
    ) {
        if !edited.is_empty() {
            for f in edited {
                self.edited_files.insert(f.clone());
            }
            self.last_edit_turn = Some(turn_idx);
            self.no_edit_runs = 0;
        } else {
            self.no_edit_runs += 1;
        }
        for f in reads {
            *self.read_count.entry(f.clone()).or_default() += 1;
        }
    }

    fn budget_adjustment(&self, cfg: &ResilienceConfig) -> i32 {
        let mut delta = 0_i32;
        if self.last_edit_turn.is_some() {
            delta += cfg.edit_bonus as i32;
        }
        if self.no_edit_runs >= cfg.idle_threshold {
            delta -= cfg.idle_penalty as i32;
        }
        delta
    }

    fn hallucination_detected(
        &self,
        assigned_file: &str,
        cfg: &ResilienceConfig,
    ) -> Option<String> {
        let count = self.read_count.get(assigned_file).copied().unwrap_or(0);
        if count >= cfg.hallucination_read_threshold && self.edited_files.is_empty() {
            Some(format!(
                "You have read `{}` {} times without editing. \
                 Stop reading; the file content is already in your prompt above. \
                 Call edit_file NOW with old_string + new_string.",
                assigned_file, count
            ))
        } else {
            None
        }
    }
}

/// Walk new messages added during one turn (slice `&messages[prev_len..]`)
/// and extract:
///  - `edited`: files whose `edit_file` / `search_replace` call returned
///    `success=true`. `search_replace` has no `file_path` so we record an
///    empty string to mark "an edit occurred" (still counts toward
///    last_edit_turn). Failed edits are excluded.
///  - `reads`: every `read_file` call's `file_path`, regardless of result.
fn scan_turn_signals(
    messages: &[crate::conversation::message::Message],
    prev_len: usize,
) -> (Vec<String> /* edited */, Vec<String> /* reads */) {
    use crate::conversation::message::MessageContent;

    // First pass: collect (call_id → tool_name + file_path)
    let mut call_meta: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    for msg in &messages[prev_len..] {
        if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
            for tc in tool_calls {
                let path = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                    .ok()
                    .and_then(|v| {
                        v.get("file_path")
                            .and_then(|x| x.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                call_meta.insert(tc.id.clone(), (tc.name.clone(), path));
            }
        }
    }

    // Second pass: pair tool_results with calls
    let mut edited = Vec::new();
    let mut reads = Vec::new();
    for msg in &messages[prev_len..] {
        if let MessageContent::ToolResult(r) = &msg.content {
            if let Some((name, path)) = call_meta.get(&r.call_id) {
                match name.as_str() {
                    "edit_file" if r.success => edited.push(path.clone()),
                    "search_replace" if r.success => edited.push(path.clone()),
                    "read_file" => reads.push(path.clone()),
                    _ => {}
                }
            }
        }
    }

    (edited, reads)
}

/// Conservative classifier: was this runner-level error a network /
/// transport hiccup (worth one retry) or a logic error (no point
/// retrying)? Substring-matches a small known set.
fn is_stream_timeout(err: &str) -> bool {
    let lo = err.to_lowercase();
    lo.contains("stream timeout")
        || lo.contains("first token timeout")
        || lo.contains("connection reset")
        || lo.contains("eof")
}

/// Wrap a single `runner.run` call with up-to-N retries for stream-timeout
/// class failures. Returns the final `TurnResult` plus a counter of how
/// many retries fired (so the caller can bump `tracker.timeouts`). On
/// retry, partial conversation state from the failed attempt is rolled
/// back so the second attempt sends a clean prompt.
async fn run_turn_with_retry(
    runner: &mut TurnRunner,
    conversation: &mut Conversation,
    system_prompt: &str,
    event_tx: &mpsc::UnboundedSender<TurnEvent>,
    cancel: CancellationToken,
    max_retries: usize,
) -> (TurnResult, usize /* timeouts_fired */) {
    let mut timeouts_fired = 0usize;
    for attempt in 0..=max_retries {
        let pre_msg_count = conversation.messages.len();
        let result = runner
            .run(conversation, system_prompt, event_tx, cancel.clone())
            .await;
        match &result {
            TurnResult::Failed(err) if is_stream_timeout(err) && attempt < max_retries => {
                timeouts_fired += 1;
                // Roll conversation back to pre-attempt state so retry
                // sends a clean prompt instead of a half-filled assistant
                // message.
                conversation.messages.truncate(pre_msg_count);
                conversation.clear_stream_buffer();
                continue;
            }
            _ => return (result, timeouts_fired),
        }
    }
    unreachable!("run_turn_with_retry loop must exit via the inner return")
}

/// Construct a human-readable summary of what the sub-agent did.
/// Replaces the previous "first 200 chars of last_text" approach with
/// a compact, signal-aware multi-part line.
fn build_summary(
    assigned: &str,
    tracker: &ProgressTracker,
    last_text: &str,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if tracker.edited_files.is_empty() {
        parts.push(format!("Did not edit `{}`", assigned));
    } else {
        let edited: Vec<&str> = tracker.edited_files.iter().map(|s| s.as_str()).collect();
        parts.push(format!(
            "Edited {} file(s): {}",
            edited.len(),
            edited.join(", ")
        ));
    }
    if tracker.timeouts > 0 {
        parts.push(format!("{} timeout(s) recovered", tracker.timeouts));
    }
    if tracker.hallucination_nudges_sent > 0 {
        parts.push(format!(
            "{} hallucination nudge(s) sent",
            tracker.hallucination_nudges_sent
        ));
    }
    if !last_text.is_empty() {
        let snippet: String = last_text.chars().take(120).collect();
        parts.push(format!("model said: {}", snippet));
    }
    parts.join(" · ")
}

/// A single sub-agent task: one file to modify.
pub struct SubAgentTask {
    pub file_path: String,
    pub file_content: String,
    pub task_instruction: String,
    pub contract: String,
    pub sibling_skeletons: String,
}

/// Structured reason a sub-agent ended in failure. Replaces the old
/// `errors: Vec<String>` so callers can match on discriminant instead
/// of substring-matching on free text.
#[derive(Debug, Clone)]
pub enum SubAgentFailure {
    /// Stream-timeout-class network failure that survived one in-loop retry.
    StreamTimeoutAfterRetry,
    /// Model read the same file ≥ `hallucination_read_threshold` times
    /// without producing a successful edit, AND the recovery turn after
    /// the nudge also failed to edit.
    HallucinationLoop { reads: usize, file: String },
    /// `no_edit_runs` reached `idle_kill_threshold`. May or may not be
    /// preceded by a hallucination nudge.
    NoProgress { idle_turns: usize },
    /// Loop exited because turn budget was exhausted and zero edits had
    /// landed. (If edits landed, exit is treated as success.)
    BudgetExhaustedNoEdits,
    /// Pool wall-time wrapper (default 5min) tripped.
    SubAgentTimeout5min,
    /// Provider returned a non-timeout-class error (network, 4xx/5xx).
    ProviderError(String),
    /// `tokio::JoinSet` reported the task panicked or was cancelled at
    /// the runtime level.
    JoinError(String),
    /// User pressed Ctrl+C / cancellation token tripped.
    Cancelled,
}

/// Per-task instrumentation snapshot. Populated as `ProgressTracker`
/// observes turns; surfaced on `SubAgentResult` so the parent agent
/// (and operators reading datalog) can diagnose without re-deriving
/// from raw conversation history.
#[derive(Debug, Clone, Default)]
pub struct Diagnostic {
    pub edited_files: Vec<String>,
    pub read_counts: std::collections::HashMap<String, usize>,
    pub timeouts: usize,
    pub hallucination_nudges_sent: usize,
    pub final_budget: usize,
    pub turns_used: usize,
}

/// Result of a sub-agent execution.
#[derive(Debug, Clone)]
pub struct SubAgentResult {
    pub file_path: String,
    pub success: bool,
    pub turns_used: usize,
    pub summary: String,
    pub failures: Vec<SubAgentFailure>,
    pub diagnostic: Diagnostic,
}

/// Tool wrapper that delegates to an inner `ReadFileTool` but rejects any
/// `read_file` whose `file_path` arg differs from `assigned_file`. Used by
/// `filter_tools_for_subagent` to keep sub-agents from drifting into
/// sibling exploration.
struct ScopedReadFile {
    inner: Arc<dyn crate::tool::Tool>,
    assigned_file: String,
}

#[async_trait::async_trait]
impl crate::tool::Tool for ScopedReadFile {
    fn definition(&self) -> crate::tool::ToolDef {
        // Delegate; the LLM sees the same schema as a normal read_file.
        self.inner.definition()
    }

    fn approval(&self, args: &str) -> crate::tool::ApprovalRequirement {
        self.inner.approval(args)
    }

    fn approval_with_context(
        &self,
        args: &str,
        ctx: &crate::tool::ToolContext,
    ) -> crate::tool::ApprovalRequirement {
        self.inner.approval_with_context(args, ctx)
    }

    fn validate_args(&self, args: &str) -> std::result::Result<(), String> {
        // First: inner schema check.
        self.inner.validate_args(args)?;
        // Second: scope check. Parse args to peek at file_path.
        let parsed: serde_json::Value = serde_json::from_str(args)
            .map_err(|e| format!("scope check parse: {e}"))?;
        let path = parsed
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if path != self.assigned_file {
            return Err(format!(
                "Sub-agent only reads its assigned file `{}`. \
                 Sibling content is in your prompt's skeleton section; \
                 do not call read_file for path `{}`.",
                self.assigned_file, path
            ));
        }
        Ok(())
    }

    async fn execute(
        &self,
        args: &str,
        ctx: &crate::tool::ToolContext,
    ) -> anyhow::Result<crate::tool::ToolResult> {
        self.inner.execute(args, ctx).await
    }
}

/// Build a sub-agent ToolRegistry by selecting only whitelisted tools from
/// the parent. `read_file` is wrapped in `ScopedReadFile` so it can only
/// read `assigned_file`. All other tools (bash, web_*, change_dir, glob,
/// list_directory, write_file, grep) are absent from the result —
/// the runner's "tool not registered" path returns a structured error to
/// the model, which routes back through the LLM as a re-think signal.
///
/// Async because the parent registry's lock is `tokio::sync::RwLock`. Called
/// from `SubAgentTask::execute` (Task 8 wiring) which is itself async.
async fn filter_tools_for_subagent(
    parent: &ToolRegistry,
    assigned_file: &str,
) -> ToolRegistry {
    let filtered = ToolRegistry::new();
    for (name, tool) in parent.iter().await {
        match name.as_str() {
            "read_file" => {
                let scoped = ScopedReadFile {
                    inner: tool,
                    assigned_file: assigned_file.to_string(),
                };
                filtered
                    .register_arc("read_file".to_string(), Arc::new(scoped))
                    .await;
            }
            "edit_file" | "search_replace" => {
                filtered.register_arc(name, tool).await;
            }
            _ => {} // blacklist by omission
        }
    }
    filtered
}

impl SubAgentTask {
    /// Execute this sub-agent task with its own Conversation + TurnRunner.
    /// Runs up to `max_turns` LLM round-trips. Auto-approves all tools.
    pub async fn execute(
        &self,
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        config: &Config,
        working_dir: &std::path::Path,
        max_turns: usize,
    ) -> SubAgentResult {
        // 1. Build minimal system prompt
        let rules = crate::config::prompt_sections::build_rules();
        let vue_warning = if self.file_path.ends_with(".vue") || self.file_path.ends_with(".svelte")
        {
            "\nCRITICAL: This is a Vue SFC. Edit <script> and <template> in SEPARATE edit_file calls. \
             Use old_string/new_string for each edit. Keep each edit focused on one region."
        } else {
            ""
        };

        let system_prompt = format!(
            "{}\n\n## SUB-AGENT RULES\n\
             You are a sub-agent. Your ONLY job: edit `{}`.\n\
             The file content is provided below — do NOT read_file, you already have it.\n\
             Call edit_file IMMEDIATELY on your first turn. Do NOT analyze, summarize, or plan.\n\
             Use old_string/new_string to find and replace text. One edit per call.\n\
             You are responsible for ONE file only. Ignore other files.{}",
            rules, self.file_path, vue_warning,
        );

        // 2. Create fresh Conversation with injected context
        let mut conversation = Conversation::new();
        let user_message = format!(
            "## Task\n{}\n\n## Contract\n{}\n\n## File: {}\n```\n{}\n```\n\n## Sibling files (skeleton)\n{}",
            self.task_instruction,
            self.contract,
            self.file_path,
            self.file_content,
            self.sibling_skeletons,
        );
        conversation.add_user_message(&user_message);

        // 3. Create isolated ToolContext + TurnRunner
        let tool_ctx = ToolContext::new(working_dir.to_path_buf());
        let permission = Box::new(AutoPermissionDecider::new(AutoPermissionMode::BypassAll));

        // Pick the same ctx strategy the parent AgentLoop would. Sub-agents
        // run on the same provider, so `for_provider` returns the matching
        // builder (DefaultCtx / OllamaCtx / future per-model strategies).
        // Falls back to a synthetic 128K-window config if the provider name
        // isn't in the config — matches AgentLoop::new's fallback.
        let build_ctx = match config.providers.get(&config.default_provider) {
            Some(pc) => crate::ctx::for_provider(pc),
            None => crate::ctx::for_provider(&crate::config::provider::ProviderConfig {
                provider_type: String::new(),
                api_key: None,
                model: String::new(),
                base_url: None,
                system_prompt: None,
                user_agent: None,
                context_window: 128_000,
                max_tokens: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                thinking_enabled: None,
                thinking_budget: None,
                skip_tls_verify: false,
                ephemeral: true,

}),
        };

        // Sandbox: filter parent tools to the sub-agent whitelist
        // (edit_file, search_replace, scoped read_file). Hands the
        // filtered registry to the runner so blacklisted tools (bash,
        // web_*, glob, list_directory, change_dir, write_file, grep)
        // are absent — the runner returns "tool not registered" to the
        // model, which routes it back via re-think.
        let sandboxed_tools =
            Arc::new(filter_tools_for_subagent(&tools, &self.file_path).await);

        let mut runner = TurnRunner {
            provider,
            tools: sandboxed_tools,
            context: tool_ctx,
            config: config.clone(),
            ctx: build_ctx,
            permission,
            hook_engine: std::sync::Arc::new(crate::hook::HookEngine::new()), // Sub-agents don't use hooks for now
            recently_edited_files: Vec::new(),
            loop_guard: Default::default(),
            current_turn_number: 0,
        };

        // 4. Event channel (we drain but don't forward — sub-agent is silent)
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<TurnEvent>();
        let cancel = CancellationToken::new();

        // 5. Run loop with resilience layer
        let res_cfg = ResilienceConfig::default();
        let mut tracker = ProgressTracker::default();
        let cap = max_turns.min(res_cfg.max_turns);
        let mut dynamic_budget = res_cfg.initial_turns as i32;
        let mut last_text = String::new();
        let mut failures: Vec<SubAgentFailure> = Vec::new();
        let mut turns_used = 0usize;

        for turn in 0..cap {
            // 1. Idle kill check (hard exit, no recovery)
            if tracker.no_edit_runs >= res_cfg.idle_kill_threshold {
                failures.push(SubAgentFailure::NoProgress {
                    idle_turns: tracker.no_edit_runs,
                });
                break;
            }

            // 2. Pre-turn hallucination check
            if let Some(nudge) = tracker.hallucination_detected(&self.file_path, &res_cfg) {
                conversation.add_user_message(&nudge);
                tracker.hallucination_nudges_sent += 1;
                // Grace turn so nudge has recovery room before budget check
                dynamic_budget += 1;
            }

            // 3. Budget exhaustion check
            if turn as i32 >= dynamic_budget && turn >= res_cfg.min_turns {
                if tracker.edited_files.is_empty() {
                    failures.push(SubAgentFailure::BudgetExhaustedNoEdits);
                }
                break;
            }

            // 4. Run turn with retry (1× for stream-timeout class)
            let pre_msg_count = conversation.messages.len();
            let (result, timeouts_fired) = run_turn_with_retry(
                &mut runner,
                &mut conversation,
                &system_prompt,
                &event_tx,
                cancel.clone(),
                res_cfg.max_call_retries,
            )
            .await;
            tracker.timeouts += timeouts_fired;
            turns_used = turn + 1;

            // Drain any UI events the runner emitted (we don't forward them).
            while event_rx.try_recv().is_ok() {}

            // 5. Process result
            match result {
                TurnResult::Responded { text, .. } => {
                    last_text = text;
                    break;
                }
                TurnResult::UsedTools { text, .. } => {
                    if let Some(t) = text {
                        last_text = t;
                    }
                    let (edited, reads) =
                        scan_turn_signals(&conversation.messages, pre_msg_count);
                    tracker.observe_turn(turn, &edited, &reads);
                    let delta = tracker.budget_adjustment(&res_cfg);
                    dynamic_budget = (dynamic_budget + delta)
                        .max(res_cfg.min_turns as i32)
                        .min(res_cfg.max_turns as i32);
                }
                TurnResult::Failed(err) if is_stream_timeout(&err) => {
                    failures.push(SubAgentFailure::StreamTimeoutAfterRetry);
                    break;
                }
                TurnResult::Failed(err) => {
                    failures.push(SubAgentFailure::ProviderError(err));
                    break;
                }
                TurnResult::Cancelled => {
                    failures.push(SubAgentFailure::Cancelled);
                    break;
                }
            }
        }

        // 6. Build diagnostic + summary
        let diagnostic = Diagnostic {
            edited_files: tracker.edited_files.iter().cloned().collect(),
            read_counts: tracker.read_count.clone(),
            timeouts: tracker.timeouts,
            hallucination_nudges_sent: tracker.hallucination_nudges_sent,
            final_budget: dynamic_budget.max(0) as usize,
            turns_used,
        };
        let success = !tracker.edited_files.is_empty() && failures.is_empty();
        let summary = build_summary(&self.file_path, &tracker, &last_text);

        SubAgentResult {
            file_path: self.file_path.clone(),
            success,
            turns_used,
            summary,
            failures,
            diagnostic,
        }
    }
}

/// Pool that runs multiple SubAgentTasks in parallel with concurrency limits.
pub struct SubAgentPool {
    pub tasks: Vec<SubAgentTask>,
    pub max_concurrent: usize,
    pub timeout_secs: u64,
}

impl SubAgentPool {
    pub fn new(tasks: Vec<SubAgentTask>) -> Self {
        Self {
            tasks,
            max_concurrent: 3,
            timeout_secs: 300,
        }
    }

    /// Execute all tasks in parallel, streaming progress events.
    pub async fn execute_all(
        self,
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        config: &Config,
        working_dir: &std::path::Path,
        event_tx: &tokio::sync::mpsc::UnboundedSender<super::AgentEvent>,
    ) -> Vec<SubAgentResult> {
        use tokio::task::JoinSet;

        let timeout = Duration::from_secs(self.timeout_secs);
        let total = self.tasks.len();
        let mut results: Vec<SubAgentResult> = Vec::with_capacity(total);

        // Process in batches of max_concurrent. Index is preserved across
        // batches via the outer `task_idx` counter — UI uses it to find
        // each task's display slot, so it must match the original
        // dispatch order regardless of which batch the task lands in.
        let mut chunks = self.tasks.into_iter().enumerate().peekable();
        while chunks.peek().is_some() {
            let batch: Vec<(usize, SubAgentTask)> =
                (&mut chunks).take(self.max_concurrent).collect();
            let mut set = JoinSet::new();

            for (task_idx, task) in batch {
                let provider = provider.clone();
                let tools = tools.clone();
                let config = config.clone();
                let working_dir = working_dir.to_path_buf();
                let tx = event_tx.clone();
                let file_path_for_err = task.file_path.clone();

                set.spawn(async move {
                    let _ = tx.send(super::AgentEvent::SubAgentTaskStarted { index: task_idx });
                    let start = std::time::Instant::now();

                    let result = tokio::time::timeout(
                        timeout,
                        task.execute(provider, tools, &config, &working_dir, 5),
                    )
                    .await;

                    let elapsed_ms = start.elapsed().as_millis() as u64;
                    match &result {
                        Ok(r) => {
                            if r.success {
                                let _ = tx.send(super::AgentEvent::SubAgentTaskDone {
                                    index: task_idx,
                                    elapsed_ms,
                                    turns: r.turns_used,
                                    summary: r.summary.clone(),
                                });
                            } else {
                                let _ = tx.send(super::AgentEvent::SubAgentTaskFailed {
                                    index: task_idx,
                                    elapsed_ms,
                                    turns: r.turns_used,
                                    reason: r.summary.clone(),
                                });
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(super::AgentEvent::SubAgentTaskFailed {
                                index: task_idx,
                                elapsed_ms,
                                turns: 0,
                                reason: "timeout".to_string(),
                            });
                        }
                    }
                    (file_path_for_err, result)
                });
            }

            while let Some(join_result) = set.join_next().await {
                match join_result {
                    Ok((_, Ok(result))) => results.push(result),
                    Ok((name, Err(_timeout))) => {
                        results.push(SubAgentResult {
                            file_path: name,
                            success: false,
                            turns_used: 0,
                            summary: "Timed out".to_string(),
                            failures: vec![SubAgentFailure::SubAgentTimeout5min],
                            diagnostic: Diagnostic::default(),
                        });
                    }
                    Err(join_err) => {
                        results.push(SubAgentResult {
                            file_path: "unknown".to_string(),
                            success: false,
                            turns_used: 0,
                            summary: "Task panicked".to_string(),
                            failures: vec![SubAgentFailure::JoinError(format!("{}", join_err))],
                            diagnostic: Diagnostic::default(),
                        });
                    }
                }
            }
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_agent_pool_creation() {
        let pool = SubAgentPool::new(vec![
            SubAgentTask {
                file_path: "TopBar.vue".to_string(),
                file_content: "<template>...</template>".to_string(),
                task_instruction: "美化样式".to_string(),
                contract: "emit('toggleSidebar')".to_string(),
                sibling_skeletons: "App.vue: ...".to_string(),
            },
            SubAgentTask {
                file_path: "Sidebar.vue".to_string(),
                file_content: "<template>...</template>".to_string(),
                task_instruction: "美化样式".to_string(),
                contract: "props: { collapsed: Boolean }".to_string(),
                sibling_skeletons: "App.vue: ...".to_string(),
            },
        ]);
        assert_eq!(pool.tasks.len(), 2);
        assert_eq!(pool.max_concurrent, 3);
        assert_eq!(pool.timeout_secs, 300);
    }

    #[test]
    fn scoped_read_file_rejects_sibling_path() {
        use crate::tool::Tool;
        let inner = Arc::new(crate::tool::read::ReadFileTool) as Arc<dyn Tool>;
        let scoped = ScopedReadFile {
            inner,
            assigned_file: "/work/a.rs".to_string(),
        };
        let err = scoped
            .validate_args(r#"{"file_path":"/work/b.rs"}"#)
            .unwrap_err();
        assert!(
            err.contains("only reads its assigned file"),
            "expected scope rejection, got: {err}"
        );
    }

    #[test]
    fn scoped_read_file_allows_assigned_path() {
        use crate::tool::Tool;
        let inner = Arc::new(crate::tool::read::ReadFileTool) as Arc<dyn Tool>;
        let scoped = ScopedReadFile {
            inner,
            assigned_file: "/work/a.rs".to_string(),
        };
        assert!(
            scoped
                .validate_args(r#"{"file_path":"/work/a.rs"}"#)
                .is_ok(),
            "assigned file must pass"
        );
    }

    #[tokio::test]
    async fn filter_tools_for_subagent_keeps_only_whitelisted() {
        let parent = make_full_tool_registry();
        let filtered = filter_tools_for_subagent(&parent, "/work/a.rs").await;
        let names = collect_tool_names(&filtered).await;
        // Allowed:
        assert!(names.contains(&"edit_file".to_string()));
        assert!(names.contains(&"search_replace".to_string()));
        assert!(names.contains(&"read_file".to_string()));
        // Blocked:
        assert!(!names.contains(&"bash".to_string()));
        assert!(!names.contains(&"web_fetch".to_string()));
        assert!(!names.contains(&"glob".to_string()));
        assert!(!names.contains(&"list_directory".to_string()));
        assert!(!names.contains(&"change_dir".to_string()));
    }

    /// Test helper: build a registry with all common tools.
    fn make_full_tool_registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register_sync(Box::new(crate::tool::read::ReadFileTool));
        r.register_sync(Box::new(crate::tool::write::WriteFileTool));
        r.register_sync(Box::new(crate::tool::edit::EditFileTool));
        r.register_sync(Box::new(crate::tool::bash::BashTool));
        r.register_sync(Box::new(crate::tool::cd::CdTool));
        r.register_sync(Box::new(crate::tool::grep::GrepTool));
        r.register_sync(Box::new(crate::tool::glob::GlobTool));
        r.register_sync(Box::new(crate::tool::list_dir::ListDirTool));
        r.register_sync(Box::new(crate::tool::web_fetch::WebFetchTool));
        r.register_sync(Box::new(crate::tool::search_replace::SearchReplaceTool));
        r
    }

    /// Test helper: collect tool names from a registry via the public iter API.
    async fn collect_tool_names(r: &ToolRegistry) -> Vec<String> {
        r.iter().await.map(|(name, _)| name).collect()
    }

    #[test]
    fn is_stream_timeout_matches_known_phrases() {
        assert!(is_stream_timeout("stream timeout after 60s"));
        assert!(is_stream_timeout("First token timeout"));
        assert!(is_stream_timeout("connection reset by peer"));
        assert!(is_stream_timeout("Unexpected EOF"));
        // Case insensitive
        assert!(is_stream_timeout("STREAM TIMEOUT"));
    }

    #[test]
    fn is_stream_timeout_rejects_other_errors() {
        assert!(!is_stream_timeout("401 Unauthorized"));
        assert!(!is_stream_timeout("missing field `content`"));
        assert!(!is_stream_timeout("Tool 'foo' was denied by the user"));
        assert!(!is_stream_timeout(""));
    }

    use crate::conversation::message::{Message, MessageContent, Role};
    use crate::tool::{ToolCall, ToolResult};

    fn make_assistant_with_tool_call(call_id: &str, name: &str, args: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: None,
                tool_calls: vec![ToolCall {
                    id: call_id.into(),
                    name: name.into(),
                    arguments: args.into(),
                }],
                reasoning_content: None,
                thinking_blocks: Vec::new(),
            },
                    synthetic: false,
        }
    }

    fn make_tool_result(call_id: &str, success: bool, output: &str) -> Message {
        Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolResult {
                call_id: call_id.into(),
                output: output.into(),
                success,
            }),
                    synthetic: false,
        }
    }

    #[test]
    fn scan_signals_counts_successful_edit_only() {
        let msgs = vec![
            make_assistant_with_tool_call("c1", "edit_file", r#"{"file_path":"/a.rs"}"#),
            make_tool_result("c1", true, "Edited /a.rs"),
        ];
        let (edited, reads) = scan_turn_signals(&msgs, 0);
        assert_eq!(edited, vec!["/a.rs".to_string()]);
        assert!(reads.is_empty());
    }

    #[test]
    fn scan_signals_failed_edit_not_counted() {
        let msgs = vec![
            make_assistant_with_tool_call("c1", "edit_file", r#"{"file_path":"/a.rs"}"#),
            make_tool_result("c1", false, "old_string not found"),
        ];
        let (edited, _reads) = scan_turn_signals(&msgs, 0);
        assert!(edited.is_empty(), "failed edits must not count");
    }

    #[test]
    fn scan_signals_counts_read_regardless_of_success() {
        let msgs = vec![
            make_assistant_with_tool_call("c1", "read_file", r#"{"file_path":"/a.rs"}"#),
            make_tool_result("c1", true, "..."),
        ];
        let (edited, reads) = scan_turn_signals(&msgs, 0);
        assert!(edited.is_empty());
        assert_eq!(reads, vec!["/a.rs".to_string()]);
    }

    #[test]
    fn scan_signals_counts_search_replace_as_edit() {
        let msgs = vec![
            make_assistant_with_tool_call(
                "c1",
                "search_replace",
                r#"{"search":"a","replace":"b"}"#,
            ),
            make_tool_result("c1", true, "modified 3 files"),
        ];
        let (edited, _reads) = scan_turn_signals(&msgs, 0);
        // search_replace has no file_path; we record empty string to mark
        // "an edit occurred" (still counts toward last_edit_turn).
        assert_eq!(edited.len(), 1);
    }

    #[test]
    fn scan_signals_respects_prev_len_offset() {
        let msgs = vec![
            make_assistant_with_tool_call("c0", "read_file", r#"{"file_path":"/a.rs"}"#),
            make_tool_result("c0", true, "..."),
            make_assistant_with_tool_call("c1", "edit_file", r#"{"file_path":"/a.rs"}"#),
            make_tool_result("c1", true, "Edited"),
        ];
        // Look at only the second pair (prev_len=2)
        let (edited, reads) = scan_turn_signals(&msgs, 2);
        assert_eq!(edited, vec!["/a.rs".to_string()]);
        assert!(reads.is_empty());
    }

    #[test]
    fn progress_tracker_increments_on_successful_edit() {
        let mut t = ProgressTracker::default();
        t.observe_turn(0, &["a.rs".into()], &[]);
        assert_eq!(t.last_edit_turn, Some(0));
        assert_eq!(t.no_edit_runs, 0);
        assert!(t.edited_files.contains("a.rs"));
    }

    #[test]
    fn progress_tracker_failed_edit_doesnt_count() {
        // Failed edit means scan_turn_signals returns it as nothing.
        // The contract is "edited slice = SUCCESSFUL edits only".
        let mut t = ProgressTracker::default();
        t.observe_turn(0, &[], &[]);
        assert_eq!(t.last_edit_turn, None);
        assert_eq!(t.no_edit_runs, 1);
    }

    #[test]
    fn progress_tracker_idle_runs_reset_on_edit() {
        let mut t = ProgressTracker::default();
        t.observe_turn(0, &[], &[]);
        t.observe_turn(1, &[], &[]);
        assert_eq!(t.no_edit_runs, 2);
        t.observe_turn(2, &["a.rs".into()], &[]);
        assert_eq!(t.no_edit_runs, 0);
    }

    #[test]
    fn hallucination_detected_at_3_reads_no_edit() {
        let cfg = ResilienceConfig::default();
        let mut t = ProgressTracker::default();
        t.observe_turn(0, &[], &["a.rs".into()]);
        t.observe_turn(1, &[], &["a.rs".into()]);
        assert!(t.hallucination_detected("a.rs", &cfg).is_none());
        t.observe_turn(2, &[], &["a.rs".into()]);
        let nudge = t.hallucination_detected("a.rs", &cfg);
        assert!(nudge.is_some());
        assert!(nudge.unwrap().contains("Stop reading"));
    }

    #[test]
    fn hallucination_not_detected_when_already_edited() {
        let cfg = ResilienceConfig::default();
        let mut t = ProgressTracker::default();
        t.observe_turn(0, &["a.rs".into()], &["a.rs".into()]);
        t.observe_turn(1, &[], &["a.rs".into()]);
        t.observe_turn(2, &[], &["a.rs".into()]);
        // 3 reads but already 1 edit — no nudge
        assert!(t.hallucination_detected("a.rs", &cfg).is_none());
    }

    #[test]
    fn budget_adjustment_combines_signals() {
        let cfg = ResilienceConfig::default();
        let mut t = ProgressTracker::default();

        // No edits, no idle → 0
        assert_eq!(t.budget_adjustment(&cfg), 0);

        // Edit happened, no idle → +edit_bonus
        t.observe_turn(0, &["a.rs".into()], &[]);
        assert_eq!(t.budget_adjustment(&cfg), cfg.edit_bonus as i32);

        // After 2 idle turns → -idle_penalty (last_edit still set, so still +bonus)
        t.observe_turn(1, &[], &[]);
        t.observe_turn(2, &[], &[]);
        let delta = t.budget_adjustment(&cfg);
        assert_eq!(
            delta,
            cfg.edit_bonus as i32 - cfg.idle_penalty as i32,
            "edit happened earlier (+bonus) AND idle threshold hit (-penalty)"
        );
    }

    #[test]
    fn resilience_config_default_values_sensible() {
        let cfg = ResilienceConfig::default();
        assert_eq!(cfg.initial_turns, 4);
        assert_eq!(cfg.max_turns, 12);
        assert_eq!(cfg.min_turns, 2);
        assert_eq!(cfg.edit_bonus, 2);
        assert_eq!(cfg.idle_penalty, 1);
        assert_eq!(cfg.idle_threshold, 2);
        assert_eq!(cfg.idle_kill_threshold, 4);
        assert_eq!(cfg.max_call_retries, 1);
        assert_eq!(cfg.hallucination_read_threshold, 3);
    }
}
