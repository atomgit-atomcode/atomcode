// crates/atomcode-tuix/src/state.rs

/// Plan vs Build execution mode. Plan is read-only exploration (no file
/// writes, no shell commands); Build is full execution with all tools.
/// Toggled by the Tab key (when the input buffer is empty) or the
/// `/plan` and `/build` slash commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    #[default]
    Build,
    Plan,
}

impl AgentMode {
    /// Human-readable label for status bar display.
    pub fn label(self) -> &'static str {
        match self {
            Self::Build => "Build",
            Self::Plan => "Plan",
        }
    }

    /// Return the opposite mode.
    pub fn toggle(self) -> Self {
        match self {
            Self::Build => Self::Plan,
            Self::Plan => Self::Build,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiPhase {
    Idle,
    Streaming,
    Approval,
    Suspended,
}

/// Rotating pool of "thinking" labels — CC-style playful verbs.
/// Advances once per turn so consecutive turns vary.
pub const THINKING_LABELS: &[&str] = &[
    "Pondering",
    "Noodling",
    "Percolating",
    "Brewing",
    "Cogitating",
    "Churning",
    "Hatching",
    "Marinating",
    "Simmering",
    "Tinkering",
    "Mulling",
    "Musing",
    "Ruminating",
    "Puttering",
    "Fermenting",
    "Divining",
    "Concocting",
    "Germinating",
    "Whittling",
    "Scheming",
];

/// Rotating pool of turn-completion phrases — CC-vibe playful verbs.
pub const DONE_LABELS: &[&str] = &[
    "Done",
    "Nailed it",
    "Wrapped",
    "Shipped",
    "Baked",
    "Plated",
    "Served",
    "Bagged",
    "Handled",
    "Dialed in",
    "Locked in",
    "Sealed",
    "Stuck the landing",
    "Buttoned up",
    "Squared away",
    "Cooked",
    "Dusted",
    "Called it",
    "Delivered",
    "Tied off",
];

/// Snapshot of the agent's context budget, cached from `AgentEvent::ContextStats`
/// and surfaced by the `/context` command.
///
/// Merged across two emission paths: the narrow TurnEvent-forwarded one
/// (system/sent/total_messages) and the rich one from `handle_send_message`
/// (tool_defs / cold_zone / ctx_window / ctx_name). Each path leaves the
/// fields it doesn't know at 0 / empty, so we merge by keeping non-zero
/// updates. See `UiState::on_context_stats`.
#[derive(Debug, Clone, Default)]
pub struct ContextSnapshot {
    pub system_tokens: usize,
    pub sent_tokens: usize,
    pub tool_defs_tokens: usize,
    pub cold_zone_tokens: usize,
    pub total_messages: usize,
    pub ctx_window: usize,
    pub ctx_name: String,
    /// Full assembled system prompt from the most recent turn.
    /// Surfaced by `/context prompt`. Empty until the first rich
    /// emission lands.
    pub system_prompt: String,
}

pub struct UiState {
    pub phase: UiPhase,
    pub agent_mode: AgentMode,
    pub spinner_label: String,
    pub spinner_frame: usize,
    /// Mirrors `TerminalCaps::unicode_symbols` — frozen at construction.
    /// When false, `tick_spinner` and the spinner-label ellipsis fall
    /// back to ASCII so terminals whose font lacks `◐` / `…` (notably
    /// Windows legacy conhost) don't show `□` tofu.
    pub unicode_symbols: bool,
    pub total_tokens: usize,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub cached_tokens: usize,
    /// When Suspended, holds the phase to restore on resume.
    pub prior_phase: Option<UiPhase>,
    /// Round-robin index into THINKING_LABELS; bumped on each on_submit.
    pub thinking_idx: usize,
    /// When the current turn started. Set by on_submit, cleared on
    /// turn-complete / turn-cancelled / error. Used to surface the
    /// total wall-clock duration in the TurnComplete event payload.
    pub turn_started_at: Option<std::time::Instant>,
    /// When the current phase began. Reset on every phase transition
    /// (on_submit, on_thinking, on_tool_call_streaming,
    /// on_tool_call_started) so the spinner shows time spent on the
    /// CURRENT operation — `Pondering… 12s`, `Running ReadFile… 4s`
    /// — instead of accumulating over the whole turn. Cleared on
    /// turn-complete / turn-cancelled / error so the idle spinner
    /// (rare) doesn't tick a stale duration.
    pub phase_started_at: Option<std::time::Instant>,
    /// Last observed context breakdown. Populated from
    /// `AgentEvent::ContextStats` — `/context` renders this. `None`
    /// before the first turn completes.
    pub last_context: Option<ContextSnapshot>,
    /// Verbatim text of the message that is currently running. Set
    /// on every submit, cleared on turn-complete. When the user hits
    /// Ctrl+C / Esc mid-stream the streaming-key handler takes this
    /// and restores it to the input buffer so the cancelled message
    /// can be edited + resent without re-typing. `None` between
    /// turns and after any successful completion.
    pub last_submitted_message: Option<String>,
    /// `/context` dispatched a `RefreshContextStats` command and is
    /// waiting for the resulting rich ContextStats event to render the
    /// report. `Some(show_prompt)` until the next rich emission lands;
    /// cleared after the render fires. Prevents stale-cache renders
    /// without forcing /context to block synchronously on the agent
    /// loop. The bool is the `prompt` sub-arg (include full system
    /// prompt body).
    pub pending_context_render: Option<bool>,
    /// Images pasted from clipboard (Ctrl+V) waiting to be sent with
    /// the next user message. Drained on submit.
    pub pending_images: Vec<atomcode_core::conversation::message::ImagePart>,
    /// Parallel to `pending_images` — content fingerprint of each pasted
    /// image's raw RGBA bytes. Used by the right-aligned status hint to
    /// suppress `Image in clipboard · ctrl+v to paste` once the clipboard
    /// content matches an already-attached image (avoids dup paste prompts),
    /// while still surfacing the hint when the user copies a new image
    /// after pasting an earlier one. Cleared together with `pending_images`
    /// on submit.
    pub pending_image_hashes: Vec<u64>,
    /// Whether to show real-time tool output (e.g., bash stdout/stderr).
    /// Toggled by Ctrl+O. When false (default), tool output is hidden
    /// during execution and only shown in the final result.
    pub show_tool_output: bool,
    /// Whether to show LLM reasoning/thinking content (e.g., DeepSeek-R1,
    /// MiniMax-M2.7). Toggled by Ctrl+O together with `show_tool_output`.
    /// When false (default), reasoning content is hidden during streaming.
    pub show_reasoning: bool,
    /// Number of fork sub-agents currently dispatched. While > 0, the
    /// foreground turn is blocked awaiting `pool.execute_all` — there's
    /// no fresh tool / think event to update the spinner, so without an
    /// override the label stays frozen on the last tool name (e.g.
    /// "Running ReadFile… 82s") for the entire pool duration. Cleared
    /// on `SubAgentDispatchEnd`.
    pub sub_agent_total: usize,
    /// How many sub-agents in the current dispatch have reported a
    /// terminal status (done / failed / timeout). Updated by
    /// `on_sub_agent_settled`. Reset to 0 on each new dispatch.
    pub sub_agent_done: usize,
    /// Per-task descriptors (path + dedup suffix) for the active
    /// dispatch. Indexed identically to the `tasks` field on
    /// `AgentEvent::SubAgentDispatchStart` so the UI can look up a
    /// child's display path from the `index` field on Started/Done/
    /// Failed events. Cleared on `on_sub_agent_dispatch_end`.
    pub sub_agent_tasks: Vec<atomcode_core::agent::SubAgentTaskInfo>,
    /// Number of failed sub-agents in the current dispatch — tracked
    /// separately from `sub_agent_done` so the aggregate summary can
    /// distinguish "6/7 ok · 1 fail" from "7/7 ok". Reset on each new
    /// dispatch.
    pub sub_agent_failed: usize,
    /// Wall-clock start of the current dispatch. Used to render the
    /// elapsed-time figure on the `SubAgentDispatchEnd` aggregate
    /// summary line. Cleared with the rest of the dispatch state.
    pub sub_agent_started_at: Option<std::time::Instant>,

    /// Cumulative byte count of `ReasoningDelta` text emitted in the
    /// CURRENT turn. Lets the spinner show a quantitative thinking
    /// estimate (`~4.2K tok`) so a 108s "Percolating" doesn't look
    /// like the model is stuck — the chars keep climbing as long as
    /// the upstream is producing reasoning chunks. Reset on submit /
    /// turn complete / turn cancelled / error.
    ///
    /// We track bytes not tokens because we don't have a tokenizer
    /// here; the displayed token figure is `bytes / 3`, a rough
    /// chars-per-token estimate for mixed CJK + English + code that
    /// matches what real tokenizers return within ~30%. The user
    /// sees a `~` prefix to flag the approximation.
    pub reasoning_chars: usize,

    /// Active tool batches, keyed by `batch_id`. Populated on
    /// `ToolBatchStarted` and cleared on `ToolBatchCompleted`. Used by
    /// per-call event handlers to detect "this ToolCallStarted/Result
    /// belongs to an active batch" and skip the standalone row render
    /// (the batch header already represents it).
    pub active_tool_batches: std::collections::HashMap<String, ActiveToolBatch>,
    /// Reverse map call_id → batch_id for O(1) lookup when a per-call
    /// event arrives. Mirrors `active_tool_batches` membership; cleared
    /// together.
    pub call_id_to_batch: std::collections::HashMap<String, String>,
}

/// Per-batch state for an active `ToolBatchStarted`. Tracks how many
/// children have completed so the UI can emit the final `· N/M ok`
/// summary on `ToolBatchCompleted`.
#[derive(Debug, Clone)]
pub struct ActiveToolBatch {
    pub call_ids: Vec<String>,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        Self::with_unicode(true)
    }

    /// Construct a `UiState` with an explicit Unicode capability.
    /// Production code calls this from `App::new` with the value the
    /// terminal-capability probe produced; tests stick with `new()`.
    pub fn with_unicode(unicode_symbols: bool) -> Self {
        Self {
            phase: UiPhase::Idle,
            agent_mode: AgentMode::default(),
            spinner_label: String::new(),
            spinner_frame: 0,
            unicode_symbols,
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            prior_phase: None,
            thinking_idx: 0,
            turn_started_at: None,
            phase_started_at: None,
            last_context: None,
            last_submitted_message: None,
            pending_context_render: None,
            pending_images: Vec::new(),
            pending_image_hashes: Vec::new(),
            show_tool_output: false,
            show_reasoning: false,
            sub_agent_total: 0,
            sub_agent_done: 0,
            sub_agent_tasks: Vec::new(),
            sub_agent_failed: 0,
            sub_agent_started_at: None,
            reasoning_chars: 0,
            active_tool_batches: std::collections::HashMap::new(),
            call_id_to_batch: std::collections::HashMap::new(),
        }
    }

    /// Render the cumulative thinking-progress estimate suitable for
    /// appending to the spinner label. Returns None below the visibility
    /// threshold so a brief sub-second think doesn't flicker `~12 tok`
    /// onto the screen for a moment.
    ///
    /// Threshold: 600 bytes (~200 estimated tokens) — about 1-2 sentences
    /// of mixed CJK/English. Below that the model probably hasn't even
    /// committed to a direction yet, so showing a count would be
    /// premature noise. Above it we know the model is meaningfully
    /// engaged and the count keeps the user from wondering "is it stuck".
    pub fn thinking_progress_label(&self) -> Option<String> {
        if self.reasoning_chars < 600 {
            return None;
        }
        let tokens = self.reasoning_chars / 3;
        let body = if tokens < 1000 {
            format!("~{} tok", tokens)
        } else {
            format!("~{:.1}K tok", tokens as f64 / 1000.0)
        };
        Some(body)
    }

    /// Single-character horizontal ellipsis (`…`, U+2026) when Unicode
    /// is available, three ASCII dots (`...`) otherwise. Used by the
    /// spinner label and any other "still working…" suffix.
    pub fn ellipsis(&self) -> &'static str {
        if self.unicode_symbols {
            "…"
        } else {
            "..."
        }
    }

    /// Merge one `AgentEvent::ContextStats` emission into the cached
    /// snapshot. The agent side fires two emissions per turn: one narrow
    /// (from `TurnRunner`) and one rich (from `handle_send_message`).
    /// Each leaves the fields it doesn't know at 0 / empty — we keep the
    /// most-recent non-zero value per field so either order works.
    pub fn on_context_stats(
        &mut self,
        system_tokens: usize,
        sent_tokens: usize,
        tool_defs_tokens: usize,
        cold_zone_tokens: usize,
        total_messages: usize,
        ctx_window: usize,
        ctx_name: &str,
        system_prompt: &str,
    ) {
        let is_rich = ctx_window > 0;
        let snap = self
            .last_context
            .get_or_insert_with(ContextSnapshot::default);
        if is_rich {
            snap.system_tokens = system_tokens;
            snap.sent_tokens = sent_tokens;
            snap.tool_defs_tokens = tool_defs_tokens;
            snap.cold_zone_tokens = cold_zone_tokens;
            snap.total_messages = total_messages;
            snap.ctx_window = ctx_window;
            if !ctx_name.is_empty() {
                snap.ctx_name = ctx_name.to_string();
            }
            snap.system_prompt = system_prompt.to_string();
            return;
        }
        if system_tokens > 0 {
            snap.system_tokens = system_tokens;
        }
        if sent_tokens > 0 {
            snap.sent_tokens = sent_tokens;
        }
        if tool_defs_tokens > 0 {
            snap.tool_defs_tokens = tool_defs_tokens;
        }
        // cold_zone can be 0 legitimately (no compression yet) — the rich
        // emission always sends an accurate value, so only overwrite when
        // the emission carries the ctx_window signal (rich path).
        if total_messages > 0 {
            snap.total_messages = total_messages;
        }
        if !ctx_name.is_empty() {
            snap.ctx_name = ctx_name.to_string();
        }
        // system_prompt — only the rich path sends non-empty bytes;
        // narrow path passes "" and we keep whatever was cached last.
        if !system_prompt.is_empty() {
            snap.system_prompt = system_prompt.to_string();
        }
    }

    /// Elapsed wall time since the current turn began, if a turn is
    /// active. Returns None when idle.
    pub fn turn_elapsed(&self) -> Option<std::time::Duration> {
        self.turn_started_at.map(|t| t.elapsed())
    }

    /// Elapsed wall time since the current phase began. The spinner
    /// uses this so its `· 12s` suffix shows time on the current
    /// operation (LLM round-trip / tool execution), not cumulative
    /// turn time. Falls back to `turn_elapsed()` when no phase
    /// transition has fired yet — defensive, should not normally
    /// happen since `on_submit` seeds both.
    pub fn phase_elapsed(&self) -> Option<std::time::Duration> {
        self.phase_started_at
            .map(|t| t.elapsed())
            .or_else(|| self.turn_elapsed())
    }

    fn current_thinking(&self) -> &'static str {
        THINKING_LABELS[self.thinking_idx % THINKING_LABELS.len()]
    }

    pub fn on_submit(&mut self) {
        self.phase = UiPhase::Streaming;
        self.spinner_label = self.current_thinking().to_string();
        self.spinner_frame = 0;
        self.thinking_idx = self.thinking_idx.wrapping_add(1);
        let now = std::time::Instant::now();
        self.turn_started_at = Some(now);
        self.phase_started_at = Some(now);
        self.reasoning_chars = 0;
    }

    pub fn on_turn_complete(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
        self.turn_started_at = None;
        self.phase_started_at = None;
        self.reasoning_chars = 0;
        // Turn finished normally — no need to offer resubmit of the
        // message any more. (On cancel, the streaming-key handler
        // already took() the Option before the TurnCancelled event
        // reaches here, so the cancelled path naturally leaves this
        // None too.)
        self.last_submitted_message = None;
    }

    pub fn on_turn_cancelled(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
        self.turn_started_at = None;
        self.phase_started_at = None;
        self.reasoning_chars = 0;
    }

    pub fn on_error(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
        self.turn_started_at = None;
        self.phase_started_at = None;
        self.reasoning_chars = 0;
    }

    /// Set the spinner label to `"Running {name}"` (no trailing ellipsis —
    /// the renderer appends `...` uniformly so it looks right even when
    /// the elapsed-time suffix is appended). Resets the phase clock so
    /// the spinner timer starts fresh on this tool execution.
    pub fn on_tool_call_started(&mut self, name: &str) {
        self.spinner_label = format!("Running {}", name);
        self.phase_started_at = Some(std::time::Instant::now());
    }

    pub fn on_tool_call_streaming(&mut self, name: &str) {
        self.spinner_label = format!("Preparing {}", name);
        self.phase_started_at = Some(std::time::Instant::now());
    }

    pub fn on_thinking(&mut self) {
        // Reuse the current pool label (don't bump the index — that's done
        // on submit, one rotation per turn not per state transition).
        let idx = self.thinking_idx.saturating_sub(1) % THINKING_LABELS.len();
        self.spinner_label = THINKING_LABELS[idx].to_string();
        // New LLM round-trip → new phase clock. Without this reset the
        // displayed time keeps growing across consecutive thinks/tools
        // and ends up showing "Noodling… 1301s" mid-turn.
        self.phase_started_at = Some(std::time::Instant::now());
    }

    /// Begin a fork dispatch. Stores the per-task descriptors so the UI
    /// can look up each child's display path + dedup-suffix by index
    /// when a `SubAgentTaskStarted/Done/Failed` event arrives — the
    /// previous flow flattened identical basenames (`tunnel.rs` × 3)
    /// into indistinguishable rows. Also overrides the foreground
    /// spinner label since `pool.execute_all` blocks the loop.
    pub fn on_sub_agent_dispatch_start(
        &mut self,
        tasks: Vec<atomcode_core::agent::SubAgentTaskInfo>,
    ) {
        self.sub_agent_total = tasks.len();
        self.sub_agent_done = 0;
        self.sub_agent_failed = 0;
        self.spinner_label = format!("Sub-agents 0/{}", tasks.len());
        self.phase_started_at = Some(std::time::Instant::now());
        self.sub_agent_started_at = Some(std::time::Instant::now());
        self.sub_agent_tasks = tasks;
    }

    /// Mark one sub-agent as completed (success). Late events after
    /// `on_sub_agent_dispatch_end` are no-ops — `sub_agent_total == 0`
    /// is the gate.
    pub fn on_sub_agent_task_done(&mut self) {
        if self.sub_agent_total == 0 {
            return;
        }
        self.sub_agent_done = self.sub_agent_done.saturating_add(1);
        self.refresh_sub_agent_label();
    }

    /// Mark one sub-agent as failed (error / timeout / no-edit).
    /// Increments BOTH the done and failed counters — for the spinner
    /// label `done` is "settled" regardless of outcome, but the
    /// aggregate emitted on dispatch_end needs the success/fail split.
    pub fn on_sub_agent_task_failed(&mut self) {
        if self.sub_agent_total == 0 {
            return;
        }
        self.sub_agent_done = self.sub_agent_done.saturating_add(1);
        self.sub_agent_failed = self.sub_agent_failed.saturating_add(1);
        self.refresh_sub_agent_label();
    }

    fn refresh_sub_agent_label(&mut self) {
        self.spinner_label = format!("Sub-agents {}/{}", self.sub_agent_done, self.sub_agent_total);
    }

    /// End the dispatch — clears descriptors so subsequent thinks/tools
    /// resume normal label behaviour. The next `on_thinking` /
    /// `on_tool_call_started` will overwrite `spinner_label`; we leave it
    /// alone here so the final "N/N" stays visible until the next phase.
    /// The success/fail split is preserved long enough for the event
    /// loop to render the aggregate summary line, then cleared.
    pub fn on_sub_agent_dispatch_end(&mut self) {
        self.sub_agent_total = 0;
        self.sub_agent_done = 0;
        self.sub_agent_failed = 0;
        self.sub_agent_tasks.clear();
        self.sub_agent_started_at = None;
    }

    pub fn on_approval_needed(&mut self, _tool: &str) {
        self.phase = UiPhase::Approval;
    }

    pub fn on_approval_resolved(&mut self) {
        self.phase = UiPhase::Streaming;
    }

    pub fn on_suspend(&mut self) {
        self.prior_phase = Some(self.phase);
        self.phase = UiPhase::Suspended;
    }

    pub fn on_resume(&mut self) {
        if let Some(p) = self.prior_phase.take() {
            self.phase = p;
        } else {
            self.phase = UiPhase::Idle;
        }
    }

    /// Pick (and advance) a playful "done" phrase for the turn separator.
    pub fn next_done_label(&mut self) -> &'static str {
        // Reuse thinking_idx rotation so done/think move together.
        let idx = self.thinking_idx.wrapping_sub(1) % DONE_LABELS.len();
        DONE_LABELS[idx]
    }

    /// Toggle real-time tool output and reasoning visibility.
    /// Both are controlled by Ctrl+O (verbose mode).
    pub fn toggle_tool_output(&mut self) {
        self.show_tool_output = !self.show_tool_output;
        self.show_reasoning = !self.show_reasoning;
    }

    /// Toggle verbose mode (alias for toggle_tool_output).
    /// Shows/hides both tool output and reasoning content.
    pub fn toggle_verbose(&mut self) {
        self.toggle_tool_output();
    }

    pub fn tick_spinner(&mut self) -> &'static str {
        // Two frame sets, picked once at construction:
        //   Unicode → half-moon rotation; Braille was prettier but
        //   Windows fonts often lack that block and fall back to ":".
        //   ASCII   → classic `|/-\` for terminals whose font also
        //   lacks the Geometric Shapes block (notably Windows legacy
        //   conhost with NSimSun / Consolas variants).
        const UNICODE_FRAMES: &[&str] = &["◐", "◓", "◑", "◒"];
        const ASCII_FRAMES: &[&str] = &["|", "/", "-", "\\"];
        let frames = if self.unicode_symbols {
            UNICODE_FRAMES
        } else {
            ASCII_FRAMES
        };
        self.spinner_frame = (self.spinner_frame + 1) % frames.len();
        frames[self.spinner_frame]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: terminals whose font lacks `◐` / `…` (Windows legacy
    // conhost with default Consolas) used to show `□` tofu for both.
    // ASCII fallback gives them readable `|/-\` and `...` instead.
    #[test]
    fn ascii_spinner_uses_pipe_slash_dash_backslash() {
        let mut s = UiState::with_unicode(false);
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(s.tick_spinner());
        }
        // Order is implementation detail; the SET must match.
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["-", "/", "\\", "|"]);
    }

    #[test]
    fn unicode_spinner_uses_half_moons() {
        let mut s = UiState::with_unicode(true);
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(s.tick_spinner());
        }
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["◐", "◑", "◒", "◓"]);
    }

    #[test]
    fn ellipsis_falls_back_to_three_ascii_dots() {
        assert_eq!(UiState::with_unicode(false).ellipsis(), "...");
        assert_eq!(UiState::with_unicode(true).ellipsis(), "…");
    }

    #[test]
    fn new_state_is_idle() {
        let s = UiState::new();
        assert_eq!(s.phase, UiPhase::Idle);
    }

    #[test]
    fn submit_transitions_to_streaming() {
        let mut s = UiState::new();
        s.on_submit();
        assert_eq!(s.phase, UiPhase::Streaming);
        // Label is one of the rotating pool entries.
        assert!(THINKING_LABELS.contains(&s.spinner_label.as_str()));
    }

    #[test]
    fn consecutive_submits_rotate_labels() {
        let mut s = UiState::new();
        s.on_submit();
        let first = s.spinner_label.clone();
        s.on_turn_complete();
        s.on_submit();
        let second = s.spinner_label.clone();
        assert_ne!(first, second);
    }

    #[test]
    fn turn_complete_returns_to_idle() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_turn_complete();
        assert_eq!(s.phase, UiPhase::Idle);
    }

    #[test]
    fn approval_needed_transitions_to_approval() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_approval_needed("bash");
        assert_eq!(s.phase, UiPhase::Approval);
    }

    #[test]
    fn approval_resolved_back_to_streaming() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_approval_needed("bash");
        s.on_approval_resolved();
        assert_eq!(s.phase, UiPhase::Streaming);
    }

    #[test]
    fn suspend_preserves_prior_phase() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_suspend();
        assert_eq!(s.phase, UiPhase::Suspended);
        s.on_resume();
        assert_eq!(s.phase, UiPhase::Streaming);
    }

    #[test]
    fn tool_call_updates_spinner_label() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_tool_call_started("read_file");
        assert!(s.spinner_label.contains("read_file"));
    }

    #[test]
    fn error_returns_to_idle() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_error();
        assert_eq!(s.phase, UiPhase::Idle);
    }

    #[test]
    fn agent_mode_default_is_build() {
        assert_eq!(AgentMode::default(), AgentMode::Build);
    }

    fn task_info(path: &str, dedup: &str) -> atomcode_core::agent::SubAgentTaskInfo {
        atomcode_core::agent::SubAgentTaskInfo {
            path: path.to_string(),
            dedup_suffix: dedup.to_string(),
        }
    }

    #[test]
    fn sub_agent_dispatch_overrides_stale_tool_label() {
        // Reproduces the user's "Running ReadFile… 82s" stale-spinner
        // problem: the foreground turn was waiting on pool.execute_all and
        // the last tool name (read_file) stayed pinned. After dispatch_start
        // the label must reflect the sub-agent counter, not the dead tool.
        let mut s = UiState::new();
        s.on_submit();
        s.on_tool_call_started("read_file");
        assert!(s.spinner_label.contains("read_file"));
        let tasks = (0..6).map(|i| task_info(&format!("a{}.rs", i), "")).collect();
        s.on_sub_agent_dispatch_start(tasks);
        assert_eq!(s.spinner_label, "Sub-agents 0/6");
    }

    #[test]
    fn sub_agent_task_done_increments_counter() {
        let mut s = UiState::new();
        let tasks = vec![
            task_info("a.rs", ""),
            task_info("b.rs", ""),
            task_info("c.rs", ""),
        ];
        s.on_sub_agent_dispatch_start(tasks);
        s.on_sub_agent_task_done();
        assert_eq!(s.spinner_label, "Sub-agents 1/3");
        s.on_sub_agent_task_done();
        s.on_sub_agent_task_done();
        assert_eq!(s.spinner_label, "Sub-agents 3/3");
        assert_eq!(s.sub_agent_failed, 0);
    }

    #[test]
    fn sub_agent_task_failed_counts_toward_done_and_failed() {
        // `sub_agent_done` is "settled" — done OR failed. The aggregate
        // line emitted on dispatch_end uses `sub_agent_failed` to split
        // them apart for "6 ok · 1 fail" rendering.
        let mut s = UiState::new();
        s.on_sub_agent_dispatch_start(vec![task_info("x.rs", ""), task_info("y.rs", "")]);
        s.on_sub_agent_task_done();
        s.on_sub_agent_task_failed();
        assert_eq!(s.sub_agent_done, 2);
        assert_eq!(s.sub_agent_failed, 1);
    }

    #[test]
    fn sub_agent_task_event_outside_dispatch_is_noop() {
        // A late event after DispatchEnd must not bring the counter
        // label back from the dead.
        let mut s = UiState::new();
        s.on_thinking();
        let pre = s.spinner_label.clone();
        s.on_sub_agent_task_done();
        s.on_sub_agent_task_failed();
        assert_eq!(s.spinner_label, pre);
    }

    #[test]
    fn sub_agent_dispatch_end_clears_descriptors() {
        let mut s = UiState::new();
        s.on_sub_agent_dispatch_start(vec![task_info("a.rs", ""), task_info("b.rs", "")]);
        assert_eq!(s.sub_agent_tasks.len(), 2);
        s.on_sub_agent_task_done();
        s.on_sub_agent_task_done();
        s.on_sub_agent_dispatch_end();
        assert_eq!(s.sub_agent_total, 0);
        assert_eq!(s.sub_agent_done, 0);
        assert_eq!(s.sub_agent_failed, 0);
        assert!(s.sub_agent_tasks.is_empty());
        assert!(s.sub_agent_started_at.is_none());
        s.on_thinking();
        assert!(!s.spinner_label.starts_with("Sub-agents"));
    }

    #[test]
    fn sub_agent_dispatch_preserves_dedup_suffix() {
        // Three tasks against tunnel.rs must come through as #1/#2/#3
        // suffixes from the dispatcher, and `state.sub_agent_tasks`
        // must carry that data so a `SubAgentTaskStarted { index: 2 }`
        // event renders the right disambiguator.
        let mut s = UiState::new();
        s.on_sub_agent_dispatch_start(vec![
            task_info("src/server/tunnel.rs", " (#1)"),
            task_info("src/client/tunnel.rs", ""),
            task_info("src/server/tunnel.rs", " (#2)"),
        ]);
        assert_eq!(s.sub_agent_tasks[0].dedup_suffix, " (#1)");
        assert_eq!(s.sub_agent_tasks[1].dedup_suffix, "");
        assert_eq!(s.sub_agent_tasks[2].dedup_suffix, " (#2)");
    }

    #[test]
    fn agent_mode_build_label() {
        assert_eq!(AgentMode::Build.label(), "Build");
    }

    #[test]
    fn agent_mode_plan_label() {
        assert_eq!(AgentMode::Plan.label(), "Plan");
    }

    #[test]
    fn agent_mode_build_toggles_to_plan() {
        assert_eq!(AgentMode::Build.toggle(), AgentMode::Plan);
    }

    #[test]
    fn agent_mode_plan_toggles_to_build() {
        assert_eq!(AgentMode::Plan.toggle(), AgentMode::Build);
    }

    #[test]
    fn agent_mode_double_toggle_returns_to_original() {
        assert_eq!(AgentMode::Build.toggle().toggle(), AgentMode::Build);
    }

    // ── thinking-progress estimate ──

    #[test]
    fn thinking_progress_silent_below_threshold() {
        // Sub-second think → no flicker. 599 bytes is one byte under
        // the visibility cutoff; nothing should render.
        let mut s = UiState::new();
        s.reasoning_chars = 599;
        assert!(s.thinking_progress_label().is_none());
    }

    #[test]
    fn thinking_progress_renders_under_1k_tokens_as_integer() {
        // 600 bytes / 3 = 200 tok exactly at the threshold.
        let mut s = UiState::new();
        s.reasoning_chars = 600;
        assert_eq!(s.thinking_progress_label().as_deref(), Some("~200 tok"));

        s.reasoning_chars = 2_999;
        assert_eq!(s.thinking_progress_label().as_deref(), Some("~999 tok"));
    }

    #[test]
    fn thinking_progress_renders_over_1k_tokens_as_kilo() {
        // 12_600 bytes / 3 = 4200 tok → `~4.2K tok`.
        let mut s = UiState::new();
        s.reasoning_chars = 12_600;
        assert_eq!(s.thinking_progress_label().as_deref(), Some("~4.2K tok"));
    }

    #[test]
    fn thinking_progress_clears_on_lifecycle_transitions() {
        let mut s = UiState::new();
        s.reasoning_chars = 5_000;

        s.on_submit();
        assert_eq!(s.reasoning_chars, 0, "on_submit must reset for fresh turn");

        s.reasoning_chars = 5_000;
        s.on_turn_complete();
        assert_eq!(s.reasoning_chars, 0, "on_turn_complete must reset");

        s.reasoning_chars = 5_000;
        s.on_turn_cancelled();
        assert_eq!(s.reasoning_chars, 0, "on_turn_cancelled must reset");

        s.reasoning_chars = 5_000;
        s.on_error();
        assert_eq!(s.reasoning_chars, 0, "on_error must reset");
    }
}
