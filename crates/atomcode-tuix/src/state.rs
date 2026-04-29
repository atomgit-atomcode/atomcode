// crates/atomcode-tuix/src/state.rs
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
    /// turn-complete / turn-cancelled / error. Used by the spinner to
    /// display live elapsed time.
    pub turn_started_at: Option<std::time::Instant>,
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
    /// Whether to show real-time tool output (e.g., bash stdout/stderr).
    /// Toggled by Ctrl+O. When false (default), tool output is hidden
    /// during execution and only shown in the final result.
    pub show_tool_output: bool,
    /// Whether to show LLM reasoning/thinking content (e.g., DeepSeek-R1,
    /// MiniMax-M2.7). Toggled by Ctrl+O together with `show_tool_output`.
    /// When false (default), reasoning content is hidden during streaming.
    pub show_reasoning: bool,
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
            last_context: None,
            last_submitted_message: None,
            pending_context_render: None,
            pending_images: Vec::new(),
            show_tool_output: false,
            show_reasoning: false,
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
        let snap = self
            .last_context
            .get_or_insert_with(ContextSnapshot::default);
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
        if ctx_window > 0 {
            snap.cold_zone_tokens = cold_zone_tokens;
            snap.ctx_window = ctx_window;
        }
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

    fn current_thinking(&self) -> &'static str {
        THINKING_LABELS[self.thinking_idx % THINKING_LABELS.len()]
    }

    pub fn on_submit(&mut self) {
        self.phase = UiPhase::Streaming;
        self.spinner_label = self.current_thinking().to_string();
        self.spinner_frame = 0;
        self.thinking_idx = self.thinking_idx.wrapping_add(1);
        self.turn_started_at = Some(std::time::Instant::now());
    }

    pub fn on_turn_complete(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
        self.turn_started_at = None;
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
    }

    pub fn on_error(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
        self.turn_started_at = None;
    }

    /// Set the spinner label to `"Running {name}"` (no trailing ellipsis —
    /// the renderer appends `...` uniformly so it looks right even when
    /// the elapsed-time suffix is appended).
    pub fn on_tool_call_started(&mut self, name: &str) {
        self.spinner_label = format!("Running {}", name);
    }

    pub fn on_tool_call_streaming(&mut self, name: &str) {
        self.spinner_label = format!("Preparing {}", name);
    }

    pub fn on_thinking(&mut self) {
        // Reuse the current pool label (don't bump the index — that's done
        // on submit, one rotation per turn not per state transition).
        let idx = self.thinking_idx.saturating_sub(1) % THINKING_LABELS.len();
        self.spinner_label = THINKING_LABELS[idx].to_string();
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
}
