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

pub struct UiState {
    pub phase: UiPhase,
    pub spinner_label: String,
    pub spinner_frame: usize,
    pub total_tokens: usize,
    /// When Suspended, holds the phase to restore on resume.
    pub prior_phase: Option<UiPhase>,
    /// Round-robin index into THINKING_LABELS; bumped on each on_submit.
    pub thinking_idx: usize,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        Self {
            phase: UiPhase::Idle,
            spinner_label: String::new(),
            spinner_frame: 0,
            total_tokens: 0,
            prior_phase: None,
            thinking_idx: 0,
        }
    }

    fn current_thinking(&self) -> &'static str {
        THINKING_LABELS[self.thinking_idx % THINKING_LABELS.len()]
    }

    pub fn on_submit(&mut self) {
        self.phase = UiPhase::Streaming;
        self.spinner_label = self.current_thinking().to_string();
        self.spinner_frame = 0;
        self.thinking_idx = self.thinking_idx.wrapping_add(1);
    }

    pub fn on_turn_complete(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
    }

    pub fn on_turn_cancelled(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
    }

    pub fn on_error(&mut self) {
        self.phase = UiPhase::Idle;
        self.spinner_label.clear();
    }

    pub fn on_tool_call_started(&mut self, name: &str) {
        self.spinner_label = format!("Running {}...", name);
    }

    pub fn on_tool_call_streaming(&mut self, name: &str) {
        self.spinner_label = format!("Preparing {}...", name);
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

    pub fn tick_spinner(&mut self) -> &'static str {
        // Half-moon rotation — renders cleanly in almost every terminal font.
        // Previously used Braille U+28xx, which fonts without that block
        // fall back to ":" or similar visible garbage.
        const FRAMES: &[&str] = &["◐", "◓", "◑", "◒"];
        self.spinner_frame = (self.spinner_frame + 1) % FRAMES.len();
        FRAMES[self.spinner_frame]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
