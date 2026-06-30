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

/// How long the model may go silent before the spinner surfaces the "slow
/// response · esc to cancel" hint. A mid-stream drop fails cleanly at the
/// provider's idle watchdog (~120s), but that is silent dead-air; this reassures
/// the user well before then that it isn't frozen and that esc cancels. Set to
/// 20s (was 12s): long enough that a merely-slow model / gateway first-byte
/// doesn't trip it — only a genuine stall does — so the hint isn't a false alarm.
/// Reasoning models still stream reasoning deltas, which reset the clock.
pub const STREAM_STALL_HINT: std::time::Duration = std::time::Duration::from_secs(20);

/// Whether the streaming spinner should warn the network may be down: we're
/// awaiting the MODEL stream AND it's been silent for at least
/// [`STREAM_STALL_HINT`]. `awaiting_model` MUST exclude tool execution — a slow
/// local tool (a 30s build) emits no events but is not a network stall, so warning
/// there would be a false positive. `None` elapsed (no activity stamped yet) never
/// warns. Pure (no clock) so the threshold logic is unit-testable.
pub fn stream_stalled_for(awaiting_model: bool, since_activity: Option<std::time::Duration>) -> bool {
    awaiting_model && since_activity.is_some_and(|d| d >= STREAM_STALL_HINT)
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

/// One entry in `message_queue`. Replaces the prior `String`-only
/// representation so queued messages can carry their pasted images +
/// markers — otherwise queueing a message during streaming silently
/// drops attachments.
#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub text: String,
    pub images: Vec<atomcode_core::conversation::message::ImagePart>,
    pub image_markers: Vec<usize>,
}

pub struct UiState {
    pub phase: UiPhase,
    pub agent_mode: AgentMode,
    pub spinner_label: String,
    pub spinner_frame: usize,
    /// A compaction's slow LLM summary is currently running — the footer
    /// spinner shows "Compacting…" instead of a thinking label and the stall
    /// hint is compaction-specific. Set by `AgentEvent::CompactionUi(Begin)`,
    /// cleared on `End`/`Mark`.
    pub compacting: bool,
    /// Whether `CompactionUi(Begin)` forced `phase = Streaming` (a standalone
    /// manual `/compact` started from Idle, which otherwise animates no
    /// spinner). If so, completion restores Idle; an auto-compaction runs
    /// mid-turn and leaves the phase to the turn's own lifecycle.
    pub compaction_forced_streaming: bool,
    /// Mirrors `TerminalCaps::unicode_symbols` — frozen at construction.
    /// When false, `tick_spinner` and the spinner-label ellipsis fall
    /// back to ASCII so terminals whose font lacks `◐` / `…` (notably
    /// Windows legacy conhost) don't show `□` tofu.
    pub unicode_symbols: bool,
    pub total_tokens: usize,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub cached_tokens: usize,
    /// Per-turn token tallies (reset at turn end). Feed the footer's billable
    /// token count + cache-hit annotation via [`turn_token_summary`]; kept
    /// separate from the session-cumulative `*_tokens` above.
    pub turn_prompt_tokens: usize,
    pub turn_completion_tokens: usize,
    pub turn_cached_tokens: usize,
    /// Whether the CURRENT turn has rendered any visible assistant text (a
    /// non-empty post-think-strip `TextDelta`). Reset at turn start/end. Read on
    /// `TurnComplete` to detect a turn that finished with NO visible answer —
    /// e.g. a reasoning-only / `<think>`-only completion the TUI would otherwise
    /// show as a blank bubble — so it can surface a notice instead.
    pub turn_rendered_visible_text: bool,
    /// Whether the CURRENT turn produced any reasoning (a `ReasoningDelta`),
    /// regardless of `show_reasoning`. Reset at turn start/end. Lets the blank-
    /// turn notice say "only reasoning, press Ctrl+O" vs "no output at all".
    pub turn_saw_reasoning: bool,
    /// Verbatim accumulation of the CURRENT/most-recent assistant reply's
    /// visible markdown (post think-strip), reassembled from `TextDelta`s.
    /// `/copy` extracts fenced code blocks from this — it must read the
    /// ORIGINAL unwrapped text, not the body cells, which are already
    /// hard-wrapped + PAD-indented and would corrupt a copied command.
    /// Cleared lazily: the first delta after [`UiState::response_finalized`]
    /// wipes it, so between turns it still holds the last reply for `/copy`.
    pub last_assistant_response: String,
    /// Set on TurnComplete / TurnCancelled / Error to seal
    /// `last_assistant_response`; the next turn's first delta clears it.
    pub response_finalized: bool,
    /// When Suspended, holds the phase to restore on resume.
    pub prior_phase: Option<UiPhase>,
    /// While waiting on a tool approval, holds the `"Running {Tool}"`
    /// label that was active before the prompt opened. `on_approval_needed`
    /// stashes it here and swaps `spinner_label` to "Waiting approval";
    /// `on_approval_resolved` restores it. Without this, the spinner kept
    /// saying "Running Bash… · 273s" while the agent was actually blocked
    /// on `permission.decide().await` — looked identical to a real tool
    /// hang.
    pub prior_spinner_label: Option<String>,
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
    /// When the last stream activity (any foreground agent event) was observed.
    /// Set on submit and refreshed on every received event; the spinner reads its
    /// elapsed to warn the user when the stream has gone silent (e.g. network drop)
    /// past [`STREAM_STALL_HINT`]. Distinct from `phase_started_at`, which does NOT
    /// reset per chunk and so can't tell "alive but slow" from "connection dead".
    pub last_stream_activity: Option<std::time::Instant>,
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
    /// Parallel to `pending_images` — the marker number `N` originally
    /// printed for each image at paste time. Submit-time matching does
    /// `line.contains("[Image #N]")` against this number to decide
    /// whether the image survived editing. Must NOT be `i + 1` from the
    /// vec position — once `session_image_count` became monotonic across
    /// turns, paste-time numbers diverge from positional indices, and
    /// using the index dropped images on every retry that wasn't the
    /// first paste of the session.
    pub pending_image_markers: Vec<usize>,
    /// Image attachments to re-attach when the user submits a recalled
    /// history entry. Populated by the up-arrow handler from the
    /// recalled `HistoryEntry::images`; drained on submit by the
    /// hydrate prelude in `event_loop/mod.rs`. Lazy by design — disk
    /// reads happen at submit time, not on every navigation.
    pub pending_recalled_attachments: Vec<crate::input::history::HistoryImageRef>,
    /// Monotonic counter for the `[Image #N]` marker shown in the input
    /// buffer + scrollback. Incremented on every paste and NEVER reset
    /// across turns — so two images pasted in different turns get
    /// distinct labels (e.g. `[Image #1]` in turn 1, `[Image #2]` in
    /// turn 2). Without this, both turns' first paste would both render
    /// as `[Image #1]`, making it ambiguous which image a later
    /// reference points at when scrolling back.
    pub session_image_count: usize,
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
    /// Session map of todo task id → content, learned by parsing the full
    /// list every `todo` result returns. `todo update` calls only carry
    /// `id`+`status` in their arguments, so this is the only place the
    /// task NAME is available at render time — used to upgrade an update
    /// row from `#4 → completed` to `#4 Write tests → completed`. Never
    /// cleared: ids are monotonic within a session, so a stale title is
    /// harmless and a known title outlives the batch that revealed it.
    pub todo_titles: std::collections::HashMap<u64, String>,
    /// Current reasoning_effort level for the active provider.
    pub reasoning_effort: Option<String>,
    /// Active goal condition string, if a `/goal` is running.
    pub goal_condition: Option<String>,
    /// Current round number of the running goal loop.
    pub goal_round: u32,
    /// When the goal was started, for elapsed-time display.
    pub goal_started_at: Option<std::time::Instant>,
    /// Per-turn stats buffered from the most recent `TurnComplete`. The
    /// separator line is NOT rendered immediately so that — if the next
    /// event happens to be `GoalUpdate(active=false)` (the goal just
    /// ended) — we can render the goal verdict banner ABOVE the line.
    /// Any other event flushes this buffer with the usual `↻ goal round N`
    /// or `✓ done` label.
    pub pending_separator: Option<PendingSeparator>,
}

/// Per-batch state for an active `ToolBatchStarted`. Tracks how many
/// children have completed so the UI can emit the final `· N/M ok`
/// summary on `ToolBatchCompleted`.
#[derive(Debug, Clone)]
pub struct ActiveToolBatch {
    pub call_ids: Vec<String>,
}

/// Stats captured at `TurnComplete` and held until the next event decides
/// how to render the turn-boundary separator.
#[derive(Debug, Clone)]
pub struct PendingSeparator {
    pub duration: std::time::Duration,
    pub turn_count: usize,
    pub tool_call_count: usize,
    pub total_tokens: usize,
    /// Whether the turn ran inside an active `/goal` (decides `↻` vs `✓`
    /// when flushed without a goal-end event).
    pub was_goal_round: bool,
    /// Whether the turn ended with `TurnStopReason::Error`. Lets the deferred
    /// flush render the ✗ "stopped" summary instead of a celebratory ✓ under
    /// the red Error line (preserves the pre-/goal-merge behaviour).
    pub errored: bool,
    /// Cache-hit ratio over the turn's input, if the provider reported cached
    /// tokens. `None` ⇒ no annotation. Rendered as `· N% cached`.
    pub cached_pct: Option<u8>,
}

/// Turn-end token summary for the footer separator.
///
/// Returns `(billable_tokens, cached_pct)` from a turn's summed
/// `prompt` / `completion` / `cached` token counts:
///   - `billable` = output + UNCACHED input = `completion + (prompt - cached)`.
///     This is what the turn actually cost — re-reading the cached prefix every
///     round is near-free, so summing gross `prompt + completion` (the old v2
///     footer) overstated usage by ~10-100× on long, multi-round turns.
///   - `cached_pct` = `cached / prompt` rounded, or `None` when the provider
///     reported no cached tokens (so we never show a misleading `0% cached`).
pub fn turn_token_summary(prompt: usize, completion: usize, cached: usize) -> (usize, Option<u8>) {
    let billable = completion + prompt.saturating_sub(cached);
    let pct = if cached > 0 && prompt > 0 {
        Some(((cached as u128 * 100 / prompt as u128).min(100)) as u8)
    } else {
        None
    };
    (billable, pct)
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
            compacting: false,
            compaction_forced_streaming: false,
            unicode_symbols,
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            turn_prompt_tokens: 0,
            turn_completion_tokens: 0,
            turn_cached_tokens: 0,
            turn_rendered_visible_text: false,
            turn_saw_reasoning: false,
            last_assistant_response: String::new(),
            response_finalized: false,
            prior_phase: None,
            prior_spinner_label: None,
            thinking_idx: 0,
            turn_started_at: None,
            phase_started_at: None,
            last_stream_activity: None,
            last_context: None,
            last_submitted_message: None,
            pending_context_render: None,
            pending_images: Vec::new(),
            pending_image_hashes: Vec::new(),
            pending_image_markers: Vec::new(),
            pending_recalled_attachments: Vec::new(),
            session_image_count: 0,
            show_tool_output: false,
            show_reasoning: false,
            sub_agent_total: 0,
            sub_agent_done: 0,
            sub_agent_tasks: Vec::new(),
            sub_agent_failed: 0,
            sub_agent_started_at: None,
            active_tool_batches: std::collections::HashMap::new(),
            call_id_to_batch: std::collections::HashMap::new(),
            todo_titles: std::collections::HashMap::new(),
            reasoning_effort: None,
            goal_condition: None,
            goal_round: 0,
            goal_started_at: None,
            pending_separator: None,
        }
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

    /// Refresh the cached context window after a model switch.
    ///
    /// The footer renders `used/window` from `last_context`, but that
    /// snapshot is only refreshed by `on_context_stats` during a turn.
    /// Switching models via the picker fires no turn, so without this the
    /// denominator stays pinned to the PREVIOUS model's window (e.g.
    /// `10.1k/200k` after switching to a 128k model). Update the window in
    /// place — keeping the used count — so the footer follows immediately;
    /// the next turn's real `ContextStats` then re-confirms it. Seeds a
    /// fresh snapshot when none exists yet so a pre-turn switch still shows
    /// the new model's window (used = 0).
    pub fn on_model_window_changed(&mut self, ctx_window: usize) {
        let snap = self
            .last_context
            .get_or_insert_with(ContextSnapshot::default);
        snap.ctx_window = ctx_window;
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

    /// Stamp "the stream is alive" — called on submit and on every received
    /// foreground agent event. Resets the stall clock read by [`Self::stream_stalled`].
    pub fn note_stream_activity(&mut self) {
        self.last_stream_activity = Some(std::time::Instant::now());
    }

    /// Whether the spinner should warn the network may be down (see
    /// [`stream_stalled_for`]). "Awaiting the model" = Streaming with a thinking
    /// label showing — NOT a tool label (`Running …`/`Preparing …`), approval, or
    /// sub-agent label, so a slow local tool never trips the network warning.
    pub fn stream_stalled(&self) -> bool {
        let awaiting_model = matches!(self.phase, UiPhase::Streaming)
            && THINKING_LABELS.contains(&self.spinner_label.as_str());
        stream_stalled_for(awaiting_model, self.last_stream_activity.map(|t| t.elapsed()))
    }

    /// Whether a running compaction's summary has stalled past
    /// [`STREAM_STALL_HINT`]. Mirrors [`Self::stream_stalled`] but gates on
    /// `compacting` instead of a THINKING_LABEL (the spinner shows
    /// "Compacting…", not a thinking label, during a compaction).
    pub fn compaction_stalled(&self) -> bool {
        stream_stalled_for(self.compacting, self.last_stream_activity.map(|t| t.elapsed()))
    }

    fn current_thinking(&self) -> &'static str {
        THINKING_LABELS[self.thinking_idx % THINKING_LABELS.len()]
    }

    /// The thinking word for the CURRENT turn — the one `on_thinking` re-displays.
    /// `on_submit` bumps `thinking_idx` AFTER showing the word, so the active word
    /// sits at `thinking_idx - 1` (mirrors the index `on_thinking` computes).
    fn active_thinking_word(&self) -> &'static str {
        let idx = self.thinking_idx.saturating_sub(1) % THINKING_LABELS.len();
        THINKING_LABELS[idx]
    }

    /// The spinner word to DISPLAY. Tool-execution labels (`Running X`,
    /// `Preparing X`) are mapped back to the turn's thinking word so the footer
    /// spinner never flashes tool names — tool progress is shown by the body
    /// `▸ Tool(detail)` rows instead. Every other label (`Sub-agents N/M`,
    /// `Waiting approval`, …) passes through unchanged. Display-only: the stored
    /// `spinner_label` and all phase-clock timing logic are untouched.
    pub(crate) fn display_spinner_label(&self) -> &str {
        if self.spinner_label.starts_with("Running ")
            || self.spinner_label.starts_with("Preparing ")
        {
            self.active_thinking_word()
        } else {
            &self.spinner_label
        }
    }

    pub fn on_submit(&mut self) {
        self.phase = UiPhase::Streaming;
        self.spinner_label = self.current_thinking().to_string();
        self.spinner_frame = 0;
        self.thinking_idx = self.thinking_idx.wrapping_add(1);
        let now = std::time::Instant::now();
        self.turn_started_at = Some(now);
        self.phase_started_at = Some(now);
        // Fresh turn: no visible text or reasoning seen yet (drives the
        // blank-turn notice on TurnComplete).
        self.turn_rendered_visible_text = false;
        self.turn_saw_reasoning = false;
        // Seed the stall clock so the first silent stretch is measured from submit,
        // not a stale stamp from the previous turn (which would flash the warning).
        self.last_stream_activity = Some(now);
    }

    pub fn on_turn_complete(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
        // Defensive: never carry a compaction spinner state into the next turn.
        self.compacting = false;
        self.compaction_forced_streaming = false;
        self.turn_started_at = None;
        self.phase_started_at = None;
        // Per-turn token tallies are consumed by the separator that renders just
        // before this; clear them so the next turn starts fresh.
        self.turn_prompt_tokens = 0;
        self.turn_completion_tokens = 0;
        self.turn_cached_tokens = 0;
        self.turn_rendered_visible_text = false;
        self.turn_saw_reasoning = false;
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
        self.compacting = false;
        self.compaction_forced_streaming = false;
        self.turn_started_at = None;
        self.phase_started_at = None;
        self.turn_prompt_tokens = 0;
        self.turn_completion_tokens = 0;
        self.turn_cached_tokens = 0;
        self.turn_rendered_visible_text = false;
        self.turn_saw_reasoning = false;
    }

    pub fn on_error(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
        self.compacting = false;
        self.compaction_forced_streaming = false;
        self.turn_started_at = None;
        self.phase_started_at = None;
        // Parity with on_turn_complete/on_turn_cancelled: clear the blank-turn
        // flags so an errored turn can't leak a stale notice into a reused turn.
        self.turn_rendered_visible_text = false;
        self.turn_saw_reasoning = false;
    }

    /// Set the spinner label to `"Running {name}"` (no trailing ellipsis —
    /// the renderer appends `...` uniformly so it looks right even when
    /// the elapsed-time suffix is appended). Resets the phase clock so
    /// the spinner timer starts fresh on this tool execution.
    pub fn on_tool_call_started(&mut self, name: &str) {
        self.spinner_label = format!("Running {}", name);
        // During a parallel batch many tool events interleave; resetting the
        // phase clock on each made the spinner's elapsed-ms flicker 0→N→0. The
        // batch anchors the clock once (`on_tool_batch_started`); only a
        // standalone tool call resets it here.
        if self.active_tool_batches.is_empty() {
            self.phase_started_at = Some(std::time::Instant::now());
        }
    }

    pub fn on_tool_call_streaming(&mut self, name: &str) {
        // The kernel emits a ToolCallStreaming per streamed argument delta, so this is
        // called repeatedly while a single tool call's args stream in (and again as the
        // name fills in). Anchor the spinner clock ONLY on the transition INTO the
        // preparing phase — resetting it on every delta made the elapsed ping-pong
        // 0ms↔Nms (reported "Preparing … · 7ms" snapping back to 0, reading as a stuck
        // loop). Mirrors the parallel-batch anchor (`on_tool_batch_started`).
        let entering = !self.spinner_label.starts_with("Preparing");
        self.spinner_label = format!("Preparing {}", name);
        if entering && self.active_tool_batches.is_empty() {
            self.phase_started_at = Some(std::time::Instant::now());
        }
    }

    /// A parallel tool batch is starting. Anchor the spinner clock once here so
    /// the elapsed-ms ticks steadily for the whole batch, instead of being reset
    /// by every interleaved per-tool event (the "Preparing … · 0ms" flicker).
    pub fn on_tool_batch_started(&mut self) {
        self.phase_started_at = Some(std::time::Instant::now());
    }

    pub fn on_thinking(&mut self) {
        // A model round is starting. Restore Streaming if we'd fallen back to
        // Idle: a `/goal` continuation runs SERVER-SIDE (the bridge injects the
        // next round, it never flows through the local `on_submit` that normally
        // sets Streaming), so without this the TUI can sit in Idle while the
        // agent keeps looping — and Esc / Ctrl+C, which only reach the cancel
        // path from `handle_streaming_key`, become silent no-ops (the reported
        // "goal can't be interrupted"). Only Idle→Streaming: never clobber an
        // active Approval prompt (a Thinking event can't arrive during one, but
        // guard anyway).
        if matches!(self.phase, UiPhase::Idle) {
            self.phase = UiPhase::Streaming;
        }
        // Reuse the current pool label (don't bump the index — that's done
        // on submit, one rotation per turn not per state transition).
        let idx = self.thinking_idx.saturating_sub(1) % THINKING_LABELS.len();
        self.spinner_label = THINKING_LABELS[idx].to_string();
        // New LLM round-trip → new phase clock. Without this reset the
        // displayed time keeps growing across consecutive thinks/tools
        // and ends up showing "Noodling… 1301s" mid-turn.
        //
        // EXCEPT during a parallel batch: the bridge emits PhaseChange(Thinking)
        // after every ToolResult, so in a batch these interleave with the tool
        // events and would restart the clock on each completion — the spinner's
        // elapsed-ms flicker 0→N→0. The batch anchors the clock once
        // (`on_tool_batch_started`); leave it alone until the batch finishes.
        if self.active_tool_batches.is_empty() {
            self.phase_started_at = Some(std::time::Instant::now());
        }
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

    /// `display_tool` is the already-PascalCased name (e.g. `"Bash"`,
    /// `"ReadFile"`) — same form `on_tool_call_started` writes into
    /// `spinner_label`. Stashed so `on_approval_resolved` can put the
    /// label back when execution resumes.
    pub fn on_approval_needed(&mut self, display_tool: &str) {
        self.phase = UiPhase::Approval;
        // Stash the running-tool label so we can restore it after the
        // user answers. Fall back to inferring from `display_tool` if no
        // ToolCallStarted ran first (defensive — should not normally
        // happen, since runner emits ToolCallStarted before approval).
        if !self.spinner_label.is_empty() {
            self.prior_spinner_label = Some(self.spinner_label.clone());
        } else {
            self.prior_spinner_label = Some(format!("Running {}", display_tool));
        }
        self.spinner_label = "Waiting approval".to_string();
        // Reset the phase clock so the elapsed suffix tracks how long
        // we've been waiting on the user, not how long the prior phase
        // (often the just-emitted ToolCallStarted) had been running.
        self.phase_started_at = Some(std::time::Instant::now());
    }

    pub fn on_approval_resolved(&mut self) {
        self.phase = UiPhase::Streaming;
        if let Some(prior) = self.prior_spinner_label.take() {
            self.spinner_label = prior;
            // The tool is about to actually start running now (hook +
            // bash_execute). Restart the clock so the spinner suffix
            // reflects that, not the cumulative wait-then-run time.
            self.phase_started_at = Some(std::time::Instant::now());
        }
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

    /// Cycle through reasoning_effort values: None → "high" → "max" → None.
    /// Returns the new value (None = use API default).
    pub fn cycle_reasoning_effort(&mut self) -> Option<&'static str> {
        let next: Option<&'static str> = match self.reasoning_effort.as_deref() {
            None => Some("high"),
            Some("high") => Some("max"),
            Some("max") => None,
            _ => Some("high"),
        };
        self.reasoning_effort = next.map(|s| s.to_string());
        next
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

    #[test]
    fn spinner_warns_only_when_model_stream_silent_past_threshold() {
        use std::time::Duration;
        // Awaiting model + silent at/over the threshold → warn (network may be down).
        assert!(stream_stalled_for(true, Some(STREAM_STALL_HINT)));
        assert!(stream_stalled_for(true, Some(STREAM_STALL_HINT + Duration::from_secs(5))));
        // Awaiting model but a chunk arrived recently → no warn (stream is alive).
        assert!(!stream_stalled_for(true, Some(Duration::from_secs(1))));
        // No activity recorded yet (fresh turn) → no warn, so it can't flash on submit.
        assert!(!stream_stalled_for(true, None));
        // NOT awaiting the model (tool running / approval / idle) → never warn, even
        // after long silence: a slow local tool is not a network stall.
        assert!(!stream_stalled_for(false, Some(STREAM_STALL_HINT * 100)));
    }

    #[test]
    fn model_switch_refreshes_cached_ctx_window_without_a_turn() {
        // Footer shows `used/window` from the cached ContextStats snapshot.
        // Switching models updates the window immediately (no turn runs), so
        // the denominator must follow while the used count stays put.
        let mut s = UiState::new();
        // Prior turn on a 200k-window model.
        s.on_context_stats(0, 10_100, 0, 0, 0, 200_000, "", "");
        assert_eq!(s.last_context.as_ref().unwrap().ctx_window, 200_000);
        // User switches to a 128k model via the picker (no turn fired).
        s.on_model_window_changed(128_000);
        let snap = s.last_context.as_ref().unwrap();
        assert_eq!(snap.ctx_window, 128_000, "window must follow the new model");
        assert_eq!(snap.sent_tokens, 10_100, "used count must be preserved");
    }

    #[test]
    fn model_switch_with_no_prior_snapshot_seeds_the_window() {
        // Switching before any turn should still surface the new model's
        // window (used=0) rather than leaving the footer window-less.
        let mut s = UiState::new();
        assert!(s.last_context.is_none());
        s.on_model_window_changed(1_000_000);
        let snap = s.last_context.as_ref().expect("snapshot seeded");
        assert_eq!(snap.ctx_window, 1_000_000);
        assert_eq!(snap.sent_tokens, 0);
    }

    #[test]
    fn stream_stalled_is_gated_to_the_model_thinking_phase() {
        // End-to-end on UiState: a thinking label past the threshold warns; a tool
        // label (Running …) with the same silence does NOT.
        let mut s = UiState::new();
        s.on_submit(); // phase=Streaming, spinner = a THINKING_LABEL
        s.last_stream_activity = Some(std::time::Instant::now() - STREAM_STALL_HINT * 2);
        assert!(s.stream_stalled(), "silent model stream past threshold must warn");
        s.on_tool_call_started("Bash"); // spinner = "Running Bash" — not a thinking label
        assert!(!s.stream_stalled(), "a slow tool must NOT trip the network warning");
    }

    #[test]
    fn streaming_tool_args_anchor_the_clock_once_not_per_delta() {
        // The kernel emits a ToolCallStreaming per streamed argument delta. Resetting
        // the phase clock on each made the spinner elapsed ping-pong 0ms↔Nms (user
        // report: "Preparing … · 7ms" snapping back to 0, looking like a stuck loop).
        // The clock must anchor ONCE when the tool-call streaming BEGINS and then tick
        // steadily across the remaining deltas — mirroring the parallel-batch anchor.
        let mut s = UiState::new();
        s.on_submit(); // spinner = a THINKING label
        s.on_tool_call_streaming(""); // first delta: name unknown yet → "Preparing …"
        let anchored = s
            .phase_started_at
            .expect("clock anchored on the first streaming delta");
        s.on_tool_call_streaming(""); // more arg deltas of the SAME call …
        s.on_tool_call_streaming("Bash"); // … then the name fills in
        assert_eq!(
            s.phase_started_at,
            Some(anchored),
            "the phase clock must NOT reset on each streamed delta (would flicker 0ms)"
        );

        // A genuinely new tool call (after the prior one ran) DOES re-anchor.
        s.on_tool_call_started("Bash"); // "Running Bash"
        s.on_tool_call_streaming(""); // a new call begins streaming
        assert_ne!(
            s.phase_started_at,
            Some(anchored),
            "a new tool call must re-anchor the clock"
        );
    }

    #[test]
    fn turn_token_summary_reports_billable_and_cached_pct() {
        // A heavily-cached round: 118K context, 114.46K of it cached, 2K output.
        // Billable = output + uncached input = 2000 + (118000 - 114460) = 5540.
        // Cached% = 114460 / 118000 ≈ 97%.
        let (billable, pct) = turn_token_summary(118_000, 2_000, 114_460);
        assert_eq!(billable, 5_540);
        assert_eq!(pct, Some(97));
    }

    #[test]
    fn turn_token_summary_no_cache_info_omits_pct() {
        // Provider didn't report cached tokens → no annotation, billable = prompt+completion.
        let (billable, pct) = turn_token_summary(100, 10, 0);
        assert_eq!(billable, 110);
        assert_eq!(pct, None);
    }

    #[test]
    fn turn_token_summary_clamps_degenerate_cached() {
        // Defensive: a provider reporting cached > prompt must not underflow or exceed 100%.
        let (billable, pct) = turn_token_summary(100, 5, 150);
        assert_eq!(billable, 5); // prompt.saturating_sub(cached) == 0
        assert_eq!(pct, Some(100));
    }

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

    /// Regression for the cross-turn `[Image #N]` ambiguity: the marker
    /// counter must NOT reset when `pending_images` drains on submit —
    /// otherwise turn 1's first paste and turn 2's first paste would
    /// both render as `[Image #1]` in scrollback. The counter lives on
    /// `session_image_count`, monotonically increasing for the whole
    /// session.
    #[test]
    fn session_image_count_starts_at_zero_on_new_state() {
        let s = UiState::new();
        assert_eq!(s.session_image_count, 0);
    }

    /// Simulate two-turn paste flow: paste image in turn 1, drain
    /// `pending_images` on submit, paste image in turn 2. The second
    /// paste must get marker `#2`, not `#1`.
    #[test]
    fn session_image_count_survives_pending_images_drain() {
        let mut s = UiState::new();
        // Turn 1: simulate paste sites' increment-then-push pattern.
        s.session_image_count += 1;
        let n1 = s.session_image_count;
        s.pending_images.push(atomcode_core::conversation::message::ImagePart {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        });
        s.pending_image_hashes.push(0xdead_beef);
        // Submit drains pending_images / hashes (mirrors event_loop logic).
        let _ = std::mem::take(&mut s.pending_images);
        let _ = std::mem::take(&mut s.pending_image_hashes);
        // Turn 2: another paste.
        s.session_image_count += 1;
        let n2 = s.session_image_count;
        assert_eq!(n1, 1, "first paste of session is #1");
        assert_eq!(n2, 2, "first paste of next turn must be #2, not #1");
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
        s.on_approval_needed("Bash");
        assert_eq!(s.phase, UiPhase::Approval);
    }

    #[test]
    fn thinking_restores_streaming_from_idle() {
        // A server-driven /goal continuation: the turn ended (Idle), then the
        // next round's PhaseChange(Thinking) arrives with NO local on_submit.
        // on_thinking must pull the TUI back into Streaming so Esc/Ctrl+C can
        // reach the cancel path — otherwise the goal is uninterruptible.
        let mut s = UiState::new();
        s.on_submit();
        s.on_turn_complete();
        assert_eq!(s.phase, UiPhase::Idle);
        s.on_thinking();
        assert_eq!(s.phase, UiPhase::Streaming);
    }

    #[test]
    fn thinking_does_not_clobber_approval() {
        // A Thinking event must never yank the UI out of an open approval
        // prompt (the guard is Idle→Streaming only).
        let mut s = UiState::new();
        s.on_submit();
        s.on_approval_needed("Bash");
        assert_eq!(s.phase, UiPhase::Approval);
        s.on_thinking();
        assert_eq!(s.phase, UiPhase::Approval);
    }

    #[test]
    fn approval_resolved_back_to_streaming() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_approval_needed("Bash");
        s.on_approval_resolved();
        assert_eq!(s.phase, UiPhase::Streaming);
    }

    // Without this, the spinner shows "Running Bash… · 273s" while the
    // agent is actually blocked on `permission.decide().await` — looks
    // identical to a real hang. Fixed by stashing/restoring the label
    // around the prompt.
    #[test]
    fn approval_needed_swaps_spinner_label_to_waiting() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_tool_call_started("Bash");
        assert_eq!(s.spinner_label, "Running Bash");
        s.on_approval_needed("Bash");
        assert_eq!(s.spinner_label, "Waiting approval");
    }

    #[test]
    fn approval_resolved_restores_running_label() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_tool_call_started("Bash");
        s.on_approval_needed("Bash");
        s.on_approval_resolved();
        assert_eq!(s.spinner_label, "Running Bash");
    }

    // Spinner suffix is `· {phase_elapsed}` — if we don't reset the clock
    // on the Approval transition, the user sees the cumulative
    // ToolCallStarted-to-now time and can't tell wait from work.
    #[test]
    fn approval_needed_resets_phase_clock() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_tool_call_started("Bash");
        let started = s.phase_started_at.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        s.on_approval_needed("Bash");
        let after = s.phase_started_at.unwrap();
        assert!(after > started, "phase_started_at should advance");
    }

    #[test]
    fn approval_resolved_resets_phase_clock() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_tool_call_started("Bash");
        s.on_approval_needed("Bash");
        let waiting_started = s.phase_started_at.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        s.on_approval_resolved();
        let resumed = s.phase_started_at.unwrap();
        assert!(resumed > waiting_started, "phase_started_at should advance");
    }

    #[test]
    fn tool_events_during_batch_keep_phase_clock_stable() {
        // Parallel batch: interleaved ToolCallStreaming/Started events must NOT
        // restart the phase clock — that reset-on-each made the spinner's
        // elapsed-ms flicker 0→N→0 (the reported "Preparing … · 0ms" bug).
        let mut s = UiState::new();
        s.on_submit();
        s.on_tool_batch_started();
        let anchor = s.phase_started_at.unwrap();
        s.active_tool_batches
            .insert("b1".into(), ActiveToolBatch { call_ids: vec![] });
        std::thread::sleep(std::time::Duration::from_millis(15));
        s.on_tool_call_streaming("Bash(a)");
        assert_eq!(s.phase_started_at, Some(anchor), "streaming restarted the batch clock");
        s.on_tool_call_started("Bash(b)");
        assert_eq!(s.phase_started_at, Some(anchor), "started restarted the batch clock");
    }

    #[test]
    fn standalone_tool_event_resets_phase_clock() {
        // No active batch → a single tool call anchors a fresh clock (unchanged).
        let mut s = UiState::new();
        s.on_submit();
        let before = s.phase_started_at.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        s.on_tool_call_streaming("Bash(x)");
        assert!(
            s.phase_started_at.unwrap() > before,
            "standalone tool event must reset the clock"
        );
    }

    #[test]
    fn thinking_does_not_reset_phase_clock_during_batch() {
        // The bridge emits PhaseChange(Thinking) after EVERY ToolResult, so in a
        // parallel batch these interleave with the tool events. on_thinking must
        // NOT restart the phase clock then — otherwise the spinner ms still
        // flickers 0→N→0 even with the tool-event guards.
        let mut s = UiState::new();
        s.on_submit();
        s.on_tool_batch_started();
        let anchor = s.phase_started_at.unwrap();
        s.active_tool_batches
            .insert("b1".into(), ActiveToolBatch { call_ids: vec![] });
        std::thread::sleep(std::time::Duration::from_millis(15));
        s.on_thinking();
        assert_eq!(s.phase_started_at, Some(anchor), "thinking restarted the batch clock");
    }

    #[test]
    fn thinking_resets_phase_clock_when_no_batch() {
        // Outside a batch, a new think is a genuine new phase → clock resets.
        let mut s = UiState::new();
        s.on_submit();
        let before = s.phase_started_at.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        s.on_thinking();
        assert!(s.phase_started_at.unwrap() > before, "think must reset the clock outside a batch");
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

    #[test]
    fn pending_recalled_attachments_starts_empty() {
        let s = UiState::new();
        assert!(s.pending_recalled_attachments.is_empty());
    }

    #[test]
    fn queued_message_carries_images() {
        let q = QueuedMessage {
            text: "hi".into(),
            images: vec![atomcode_core::conversation::message::ImagePart {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }],
            image_markers: vec![1],
        };
        assert_eq!(q.images.len(), 1);
        assert_eq!(q.image_markers, vec![1]);
    }

    #[test]
    fn compaction_stalled_only_when_compacting_and_silent() {
        let mut s = UiState::new();
        // Not compacting → never "compaction stalled" regardless of silence.
        s.compacting = false;
        s.last_stream_activity = Some(std::time::Instant::now() - STREAM_STALL_HINT);
        assert!(!s.compaction_stalled());
        // Compacting + silent past the threshold → stalled.
        s.compacting = true;
        assert!(s.compaction_stalled());
        // Compacting but fresh activity → not stalled.
        s.last_stream_activity = Some(std::time::Instant::now());
        assert!(!s.compaction_stalled());
    }
}
