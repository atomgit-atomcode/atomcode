// crates/atomcode-tuix/src/state.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiPhase {
    Idle,
    Streaming,
    Approval,
    Suspended,
}

pub struct UiState {
    pub phase: UiPhase,
    pub spinner_label: String,
    pub spinner_frame: usize,
    pub total_tokens: usize,
    /// When Suspended, holds the phase to restore on resume.
    pub prior_phase: Option<UiPhase>,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            phase: UiPhase::Idle,
            spinner_label: String::new(),
            spinner_frame: 0,
            total_tokens: 0,
            prior_phase: None,
        }
    }

    pub fn on_submit(&mut self) {
        self.phase = UiPhase::Streaming;
        self.spinner_label = "Thinking...".into();
        self.spinner_frame = 0;
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
        self.spinner_label = "Thinking...".into();
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

    pub fn tick_spinner(&mut self) -> &'static str {
        const FRAMES: &[&str] = &["⠋","⠙","⠹","⠸","⠼","⠴","⠦","⠧","⠇","⠏"];
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
        assert_eq!(s.spinner_label, "Thinking...");
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
