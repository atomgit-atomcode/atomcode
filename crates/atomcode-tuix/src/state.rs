// crates/atomcode-tuix/src/state.rs

/// Execution mode (unified). Alias of the shared coding runtime enum so TUI, daemon,
/// webui share one type. Build = interactive approval; Auto = auto-approve all
/// (bypass); Plan = read-only. Cycled by Tab / Shift+Tab.
pub use atomcode_coding::RuntimeMode as AgentMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiPhase {
    Idle,
    Streaming,
    Approval,
    UserInput,
    Suspended,
}

/// A tool-approval decision offered in the footer approval panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    AllowOnce,
    AlwaysAllow,
    Deny,
}

/// One selectable row of the approval panel.
#[derive(Debug, Clone)]
pub struct ApprovalOption {
    pub label: String,
    pub kind: ApprovalKind,
    /// Single-key accelerator (lower-case): 'y' / 'a' / 'n'.
    pub accel: char,
}

/// The active tool-approval prompt, shown as a footer panel while
/// `UiPhase::Approval`. `None` when no approval is pending.
#[derive(Debug, Clone)]
pub struct ApprovalPanel {
    pub tool: String,
    pub detail: String,
    pub options: Vec<ApprovalOption>,
    pub selected: usize,
    pub cache_key: String,
}

impl ApprovalPanel {
    pub fn move_up(&mut self) {
        if self.options.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.options.len() - 1
        } else {
            self.selected - 1
        };
    }
    pub fn move_down(&mut self) {
        if self.options.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.options.len();
    }
    /// Index of the option whose accelerator matches `c` (case-insensitive).
    pub fn accel_index(&self, c: char) -> Option<usize> {
        let c = c.to_ascii_lowercase();
        self.options.iter().position(|o| o.accel == c)
    }
}

/// The active `request_user_input` prompt, shown as a footer panel while
/// `UiPhase::UserInput`. `None` when no interactive question is pending.
/// Mirrors `ApprovalPanel`: it replaces the input box and the user answers with
/// arrow keys / space / Enter (single/multiple) or by typing (text).
#[derive(Debug, Clone)]
pub struct UserInputPanel {
    /// Kernel round-trip id — echoed back in `AgentCommand::Respond { id, .. }`.
    pub request_id: u64,
    pub header: String,
    pub question: String,
    pub mode: atomcode_capabilities::tools::request_user_input::UserInputMode,
    /// Concrete options (single/multiple): label + optional model-provided
    /// description (rendered as a faint second line). Empty for text mode.
    /// Does NOT include the always-appended "Other" free-text option — that is
    /// synthesized at index `options.len()`.
    pub options: Vec<(String, Option<String>)>,
    /// Highlighted row (single/multiple). Ranges over `0..options.len()`
    /// (concrete options) and `options.len()` (the always-appended "Other"
    /// free-text row). There is NO separate submit row. See
    /// [`UserInputPanel::is_other_row`].
    pub cursor: usize,
    /// Per-row checked flags (multiple mode only). Length `options.len() + 1`:
    /// one slot per concrete option PLUS a trailing slot for the "Other" row.
    pub checked: Vec<bool>,
    /// Free-form buffer (text mode only) — the standalone `text` mode's input.
    pub text: String,
    /// Free-text buffer for the always-appended "Other" row in single/multiple
    /// mode. Distinct from `text` (which is the standalone text-mode input).
    pub custom_text: String,
    /// Whether the "Other" free-text row is offered (mirrors `UserInputRequest.custom`).
    /// When false, the Other row does not exist — indices below account for its absence.
    pub custom: bool,
}

impl UserInputPanel {
    pub fn new(
        request_id: u64,
        r: &atomcode_capabilities::tools::request_user_input::UserInputRequest,
    ) -> Self {
        let options: Vec<(String, Option<String>)> = r
            .options
            .iter()
            .map(|o| (o.label.clone(), o.description.clone()))
            .collect();
        // One checkbox slot per concrete option PLUS the trailing "Other" row —
        // but only when custom answers are offered.
        let checked = vec![false; options.len() + r.custom as usize];
        Self {
            request_id,
            header: r.header.clone(),
            question: r.question.clone(),
            mode: r.mode.clone(),
            options,
            cursor: 0,
            checked,
            text: String::new(),
            custom_text: String::new(),
            custom: r.custom,
        }
    }
    /// Index of the always-appended "Other" free-text row (`options.len()`).
    pub fn other_index(&self) -> usize {
        self.options.len()
    }
    /// Index of the Submit row (multiple mode only). After the concrete options,
    /// plus the "Other" row when `custom` is on. `None` for single mode.
    pub fn submit_index(&self) -> Option<usize> {
        use atomcode_capabilities::tools::request_user_input::UserInputMode;
        if matches!(self.mode, UserInputMode::Multiple) {
            Some(self.options.len() + self.custom as usize)
        } else {
            None
        }
    }
    /// Last navigable cursor index.
    /// - Multiple: the Submit row.
    /// - Single/text: the "Other" row when `custom`, else the last concrete option.
    pub(crate) fn last_row(&self) -> usize {
        use atomcode_capabilities::tools::request_user_input::UserInputMode;
        match self.mode {
            UserInputMode::Multiple => self.submit_index().unwrap(),
            _ => {
                if self.custom {
                    self.other_index()
                } else {
                    self.options.len().saturating_sub(1)
                }
            }
        }
    }
    /// Whether `cursor` is on the always-appended "Other" free-text row.
    pub fn is_other_row(&self) -> bool {
        self.custom && self.cursor == self.other_index()
    }
    /// Whether `cursor` is on the Submit row (multiple mode only).
    pub fn is_submit_row(&self) -> bool {
        self.submit_index().is_some_and(|i| self.cursor == i)
    }
    /// Back-compat alias for the "Other" row (formerly `✎ 自己输入…`).
    pub fn is_custom_row(&self) -> bool {
        self.is_other_row()
    }
    pub fn move_up(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }
    pub fn move_down(&mut self) {
        if self.cursor < self.last_row() {
            self.cursor += 1;
        }
    }
    /// Select/toggle the option under the cursor. Single mode is exclusive:
    /// the cursor IS the radio selection, so this only clears the custom text
    /// when landing on a concrete option. Multiple mode toggles the row's
    /// checkbox (including the "Other" row).
    pub fn select_current_option(&mut self) {
        use atomcode_capabilities::tools::request_user_input::UserInputMode::*;
        match self.mode {
            Single => {
                // Single: the cursor row IS the selection (radio). Choosing a
                // concrete option supersedes any typed custom text.
                if self.cursor < self.options.len() {
                    self.custom_text.clear();
                }
            }
            Multiple => {
                if let Some(c) = self.checked.get_mut(self.cursor) {
                    *c = !*c;
                }
            }
            Text => {}
        }
    }
    /// Toggle the highlighted row (multiple-mode Space / number convenience).
    /// In single mode this is a no-op beyond clearing custom text on a concrete
    /// option (the cursor is the radio).
    pub fn toggle(&mut self) {
        self.select_current_option();
    }
    /// Toggle a specific row by index (multiple mode). Single mode moves the
    /// cursor to it instead (the cursor is the radio) — handled by the caller.
    pub fn toggle_index(&mut self, i: usize) {
        if let Some(c) = self.checked.get_mut(i) {
            *c = !*c;
        }
    }
    /// Typed characters edit the "Other" custom-text buffer. In single mode
    /// this supersedes any concrete-option radio selection (the cursor should
    /// already be on the "Other" row). Inclusion of the custom answer is derived
    /// from `custom_text.trim()` being non-empty — no separate checkbox state.
    pub fn push_custom(&mut self, c: char) {
        self.custom_text.push(c);
    }
    /// Backspace on the "Other" row.
    pub fn pop_custom(&mut self) {
        self.custom_text.pop();
    }
    /// The chosen concrete-option labels for MULTIPLE mode (all checked
    /// concrete rows; the "Other" row is handled separately via `custom_text`).
    /// For single mode this reads the cursor as the radio.
    pub fn chosen(&self) -> Vec<String> {
        use atomcode_capabilities::tools::request_user_input::UserInputMode::*;
        match self.mode {
            Single => {
                // Cursor-as-radio: a concrete option row → that label.
                if self.cursor < self.options.len() {
                    vec![self.options[self.cursor].0.clone()]
                } else {
                    vec![]
                }
            }
            Multiple => self
                .options
                .iter()
                .enumerate()
                .filter(|(i, _)| self.checked.get(*i).copied().unwrap_or(false))
                .map(|(_, (l, _))| l.clone())
                .collect(),
            Text => vec![],
        }
    }
    /// Build the `UserInputResponse` for the current panel state.
    /// - single: the cursor row → its label, OR (cursor on "Other")
    ///   `custom_text.trim()` when non-empty; `None` when nothing to submit.
    /// - multiple: all checked concrete labels, PLUS `custom_text.trim()` when
    ///   non-empty (inclusion is text-derived, NOT from `checked[other_index]`);
    ///   `None` when nothing selected.
    /// Text mode is handled separately by the caller.
    pub fn build_response(
        &self,
    ) -> Option<atomcode_capabilities::tools::request_user_input::UserInputResponse> {
        use atomcode_capabilities::tools::request_user_input::{
            UserInputMode::*, UserInputResponse,
        };
        match self.mode {
            Single => {
                let selected = if self.cursor < self.options.len() {
                    vec![self.options[self.cursor].0.clone()]
                } else {
                    let custom = self.custom_text.trim();
                    if custom.is_empty() {
                        return None; // cursor on "Other" with no text → no-op
                    }
                    vec![custom.to_string()]
                };
                Some(UserInputResponse {
                    declined: false,
                    selected,
                    text: None,
                })
            }
            Multiple => {
                let mut selected = self.chosen();
                // Inclusion of the custom answer is derived from the text itself,
                // NOT from a separate checked flag — so Enter on the Other row
                // (which is a no-op) can never desync the checkbox from the text.
                let custom = self.custom_text.trim();
                if !custom.is_empty() {
                    selected.push(custom.to_string());
                }
                if selected.is_empty() {
                    return None; // nothing chosen → Submit is a no-op
                }
                Some(UserInputResponse {
                    declined: false,
                    selected,
                    text: None,
                })
            }
            Text => Some(UserInputResponse {
                declined: false,
                selected: vec![],
                text: Some(self.text.clone()),
            }),
        }
    }

    /// For **single** mode only: Enter confirms the cursor row. On a concrete
    /// option → `{selected:[label]}`; on the "Other" row → `{selected:[custom]}`
    /// when non-empty, else `None` (no-op). Multiple / text return `None`
    /// (their Enter path is handled differently: multiple Enter on the Submit row
    /// calls `build_response()` directly; Enter on option/Other rows toggles).
    pub fn try_immediate_response(
        &self,
    ) -> Option<atomcode_capabilities::tools::request_user_input::UserInputResponse> {
        use atomcode_capabilities::tools::request_user_input::UserInputMode;
        if !matches!(self.mode, UserInputMode::Single) {
            return None;
        }
        self.build_response()
    }
}

/// A batch of 1..=4 questions answered in one interaction. Wraps per-question
/// `UserInputPanel`s; `request_id` lives here (the panels' own `request_id` is unused
/// in a batch). `current` ranges `0..questions.len()` (question panels) plus
/// `questions.len()` (the Submit stop that `Tab` cycles to).
pub struct UserInputBatch {
    pub request_id: u64,
    pub questions: Vec<UserInputPanel>,
    pub current: usize,
}

impl UserInputBatch {
    pub fn new(
        request_id: u64,
        reqs: &[atomcode_capabilities::tools::request_user_input::UserInputRequest],
    ) -> Self {
        let questions = reqs
            .iter()
            .map(|r| UserInputPanel::new(request_id, r))
            .collect();
        Self {
            request_id,
            questions,
            current: 0,
        }
    }

    /// More than one question → render the navigator + Tab/Submit chrome.
    pub fn is_multi(&self) -> bool {
        self.questions.len() > 1
    }

    /// The Submit stop index (one past the last question).
    pub fn submit_stop(&self) -> usize {
        self.questions.len()
    }

    pub fn on_submit_stop(&self) -> bool {
        self.current == self.submit_stop()
    }

    /// `Tab`: next question, wrapping through the Submit stop back to the first.
    pub fn next_question(&mut self) {
        self.current = if self.current >= self.submit_stop() {
            0
        } else {
            self.current + 1
        };
    }

    /// `Shift+Tab`: previous question, wrapping to the Submit stop.
    pub fn prev_question(&mut self) {
        self.current = if self.current == 0 {
            self.submit_stop()
        } else {
            self.current - 1
        };
    }

    /// Whether question `i` has real content (used for the ✓/○ navigator marker).
    pub fn is_answered(&self, i: usize) -> bool {
        self.questions.get(i).is_some_and(Self::panel_answered)
    }

    /// One response per question, in order. A question with no real content becomes
    /// `declined` (partial-submit semantics).
    pub fn build_batch_response(
        &self,
    ) -> Vec<atomcode_capabilities::tools::request_user_input::UserInputResponse> {
        use atomcode_capabilities::tools::request_user_input::UserInputResponse;
        self.questions
            .iter()
            .map(|p| {
                if Self::panel_answered(p) {
                    p.build_response()
                        .unwrap_or_else(UserInputResponse::declined)
                } else {
                    UserInputResponse::declined()
                }
            })
            .collect()
    }

    /// A panel counts as answered when it builds a response with a non-empty selection
    /// or non-blank text. (Text mode's `build_response` is always `Some`, possibly empty.)
    fn panel_answered(p: &UserInputPanel) -> bool {
        match p.build_response() {
            Some(r) => {
                !r.selected.is_empty()
                    || r.text
                        .as_deref()
                        .map(|t| !t.trim().is_empty())
                        .unwrap_or(false)
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod user_input_custom_tests {
    use super::*;
    use atomcode_capabilities::tools::request_user_input::{
        UserInputMode, UserInputOption, UserInputRequest,
    };

    fn req(mode: UserInputMode, custom: bool) -> UserInputRequest {
        UserInputRequest {
            header: "H".into(),
            question: "Q?".into(),
            mode,
            options: vec![
                UserInputOption {
                    label: "A".into(),
                    description: None,
                },
                UserInputOption {
                    label: "B".into(),
                    description: None,
                },
            ],
            custom,
        }
    }

    #[test]
    fn single_last_row_gates_on_custom() {
        let with = UserInputPanel::new(1, &req(UserInputMode::Single, true));
        assert_eq!(
            with.last_row(),
            2,
            "single, custom → Other row is last (idx 2)"
        );
        let without = UserInputPanel::new(1, &req(UserInputMode::Single, false));
        assert_eq!(
            without.last_row(),
            1,
            "single, no custom → last option (idx 1)"
        );
    }

    #[test]
    fn multiple_submit_index_and_checked_gate_on_custom() {
        let with = UserInputPanel::new(1, &req(UserInputMode::Multiple, true));
        assert_eq!(with.submit_index(), Some(3), "custom → other@2, submit@3");
        assert_eq!(
            with.checked.len(),
            3,
            "custom → Other checkbox slot present"
        );
        let without = UserInputPanel::new(1, &req(UserInputMode::Multiple, false));
        assert_eq!(
            without.submit_index(),
            Some(2),
            "no custom → submit right after options@2"
        );
        assert_eq!(
            without.checked.len(),
            2,
            "no custom → no Other checkbox slot"
        );
    }
}

#[cfg(test)]
mod user_input_batch_tests {
    use super::*;
    use atomcode_capabilities::tools::request_user_input::{
        UserInputMode, UserInputOption, UserInputRequest,
    };

    fn text_q(h: &str) -> UserInputRequest {
        UserInputRequest {
            header: h.into(),
            question: "?".into(),
            mode: UserInputMode::Text,
            options: vec![],
            custom: true,
        }
    }
    fn single_q(h: &str) -> UserInputRequest {
        UserInputRequest {
            header: h.into(),
            question: "?".into(),
            mode: UserInputMode::Single,
            options: vec![UserInputOption {
                label: "x".into(),
                description: None,
            }],
            custom: true,
        }
    }

    #[test]
    fn tab_wraps_through_submit_stop() {
        let mut b = UserInputBatch::new(7, &[text_q("a"), text_q("b")]);
        assert_eq!(b.current, 0);
        assert_eq!(b.submit_stop(), 2);
        b.next_question();
        assert_eq!(b.current, 1);
        b.next_question();
        assert_eq!(b.current, 2); // submit stop
        assert!(b.on_submit_stop());
        b.next_question();
        assert_eq!(b.current, 0); // wrap
        b.prev_question();
        assert_eq!(b.current, 2); // wrap back to submit stop
    }

    #[test]
    fn build_batch_response_declines_untouched_questions() {
        let mut b = UserInputBatch::new(1, &[single_q("a"), text_q("b")]);
        // Answer q0 by selecting the concrete option under its cursor (cursor 0).
        b.questions[0].select_current_option();
        let resps = b.build_batch_response();
        assert_eq!(resps.len(), 2);
        assert!(!resps[0].declined, "answered question 0");
        assert_eq!(resps[0].selected, vec!["x".to_string()]);
        assert!(resps[1].declined, "untouched text question 1 → declined");
    }

    #[test]
    fn is_answered_tracks_content() {
        let mut b = UserInputBatch::new(1, &[text_q("a")]);
        assert!(!b.is_answered(0), "empty text → not answered");
        b.questions[0].text.push_str("hi");
        assert!(b.is_answered(0));
    }

    #[test]
    fn single_question_batch_is_not_multi() {
        let b = UserInputBatch::new(1, &[text_q("only")]);
        assert!(!b.is_multi());
        assert_eq!(b.submit_stop(), 1);
    }
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
pub fn stream_stalled_for(
    awaiting_model: bool,
    since_activity: Option<std::time::Duration>,
) -> bool {
    awaiting_model && since_activity.is_some_and(|d| d >= STREAM_STALL_HINT)
}

/// Minimum wall-clock gap between spinner frame advances. Slightly under the
/// 100ms interval-timer period so a timer tick always advances the glyph (no
/// visible stall), while the far more frequent per-token post-event redraws
/// during streaming are collapsed to at most one advance per gap — otherwise
/// the spinner blurs at token rate. See [`UiState::tick_spinner_at`].
pub const SPINNER_MIN_ADVANCE: std::time::Duration = std::time::Duration::from_millis(80);

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
/// Merged across runtime emission paths
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
    pub images: Vec<atomcode_kernel::message::ImageContent>,
    pub image_markers: Vec<usize>,
}

/// A message already sent to the kernel as an in-turn steer, retained by the
/// TUI until `Steered` confirms delivery. The copy is also the recovery payload
/// for Esc: cancellation clears the kernel steer buffer, so the TUI replays
/// these messages as fresh turns after the authoritative cancel terminal.
#[derive(Debug, Clone)]
pub struct PendingSteer {
    pub message: QueuedMessage,
    pub display_text: String,
}

pub struct UiState {
    pub phase: UiPhase,
    pub agent_mode: AgentMode,
    pub spinner_label: String,
    pub spinner_frame: usize,
    /// When the spinner frame last actually advanced. `tick_spinner` is called
    /// from BOTH the 100ms interval timer AND an opportunistic post-event
    /// redraw that fires once per streamed token (to keep the footer's elapsed
    /// clock fresh). Without a gate the per-token redraws spun the glyph dozens
    /// of times a second during output — it looked like a blur. We rate-limit
    /// the advance to [`SPINNER_MIN_ADVANCE`] regardless of call frequency.
    pub spinner_last_advance: Option<std::time::Instant>,
    /// A compaction's slow LLM summary is currently running — the footer
    /// spinner shows "Compacting…" instead of a thinking label and the stall
    /// hint is compaction-specific. Set by
    /// `CodingRuntimeEvent::CompactionStarted`, then cleared when compaction
    /// finishes.
    pub compacting: bool,
    /// Whether `CodingRuntimeEvent::CompactionStarted` forced `phase = Streaming` (a standalone
    /// manual `/compact` started from Idle, which otherwise animates no
    /// spinner). If so, completion restores Idle; an auto-compaction runs
    /// mid-turn and leaves the phase to the turn's own lifecycle.
    pub compaction_forced_streaming: bool,
    /// Latest live activity of a running `task` subagent fan-out (e.g.
    /// `explore#4 · grep unwrap`), set from marker-prefixed progress chunks and
    /// spliced into the spinner label in-place so a multi-minute fan-out isn't a
    /// silent `Pondering…`. `None` when no subtask activity is current; cleared
    /// when the `task` tool finishes and at every authoritative turn terminal.
    pub subagent_activity: Option<String>,
    /// Mirrors `TerminalCaps::unicode_symbols` — frozen at construction.
    /// When false, `tick_spinner` and the spinner-label ellipsis fall
    /// back to ASCII so terminals whose font lacks `◐` / `…` (notably
    /// Windows legacy conhost) don't show `□` tofu.
    pub unicode_symbols: bool,
    /// Mirrors `TerminalCaps::colors` — frozen at construction.
    pub colors: bool,
    pub total_tokens: usize,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub cached_tokens: usize,
    /// Read-only command report shown in the footer directly below the input
    /// box while a turn is streaming. `/usage` and `/cost` use this instead of
    /// appending their multi-line report to conversation scrollback. Cleared
    /// explicitly by Esc; the Esc press is consumed before turn cancellation.
    pub footer_command_output: Option<String>,
    /// Live `/usage` panel state backing [`footer_command_output`] while a turn
    /// is streaming. The interactive modal can't install mid-turn (live token
    /// redraws own the footer), so tab switching re-renders the active tab from
    /// this into `footer_command_output`. `Some` only for streaming `/usage`;
    /// `/cost` and idle `/usage` leave it `None`. Cleared wherever
    /// `footer_command_output` is.
    pub footer_usage: Option<crate::modals::usage::UsageModal>,
    /// Per-turn token tallies (reset at turn end). Feed the footer's billable
    /// token count + cache-hit annotation via [`turn_token_summary`]; kept
    /// separate from the session-cumulative `*_tokens` above.
    pub turn_prompt_tokens: usize,
    pub turn_completion_tokens: usize,
    pub turn_cached_tokens: usize,
    /// Chars of model OUTPUT streamed this turn — visible text + reasoning +
    /// tool-call arguments — accumulated live from the delta events (unlike
    /// `turn_completion_tokens`, which the provider only reports once per round,
    /// so it can't tick during a long single generation). Drives the spinner's
    /// `↑ N tokens` activity indicator (via [`turn_output_token_estimate`]) so a
    /// multi-minute turn visibly proves it's alive rather than looking hung.
    /// Counting tool-call args is the point: a turn that spends minutes emitting
    /// one huge tool call (e.g. a giant script) would otherwise show no motion.
    /// Reset at turn start/end.
    pub turn_output_chars: usize,
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
    /// Set on an authoritative turn terminal to seal
    /// `last_assistant_response`; the next turn's first delta clears it.
    pub response_finalized: bool,
    /// The failure reason (provider `Error` / `RateLimited`) captured for the
    /// CURRENT turn, FOLDED into the errored `✗ 已中断：<reason>` summary. The
    /// standalone red error line is rendered mid-turn (phase `Streaming`, spinner
    /// active) and can be clobbered by the physical Streaming→Idle redraw on a real
    /// terminal; the summary renders cleanly at Idle, so folding the reason into it
    /// guarantees the user always sees WHY the turn stopped. Set by the Error /
    /// RateLimited handlers; `take()`n when the errored summary renders, and reset
    /// to `None` when a CLEAN summary renders (`turn_summary_label`) so a reason
    /// from an error path that produced no summary can never fold into a later turn.
    pub last_turn_error: Option<String>,
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
    /// The active footer approval panel (arrow-key selectable). `None` when no
    /// tool approval is pending. Set in the `ApprovalNeeded` handler, cleared on
    /// resolve / turn-end / session reset.
    pub approval_panel: Option<ApprovalPanel>,
    /// The active footer `request_user_input` panel. `None` when no interactive
    /// question is pending. Set in the `Request` handler, cleared on resolve /
    /// turn-end / session reset. Mirrors `approval_panel`.
    pub user_input_panel: Option<UserInputPanel>,
    /// A multi-question `request_user_input` batch is pending (mutually exclusive with
    /// `user_input_panel`). Set in the `Request` handler when the payload carries a
    /// `questions` array; cleared alongside `user_input_panel` on resolve.
    pub user_input_batch: Option<UserInputBatch>,
    /// Round-robin index into THINKING_LABELS; bumped on each on_submit.
    pub thinking_idx: usize,
    /// When the current turn started. Set by on_submit, cleared on
    /// turn-complete / turn-cancelled. Used to surface the
    /// total wall-clock duration in the TurnComplete event payload.
    pub turn_started_at: Option<std::time::Instant>,
    /// When the current phase began. Reset on every phase transition
    /// (on_submit, on_thinking, on_tool_call_streaming,
    /// on_tool_call_started) so the spinner shows time spent on the
    /// CURRENT operation — `Pondering… 12s`, `Running ReadFile… 4s`
    /// — instead of accumulating over the whole turn. Cleared on
    /// turn-complete / turn-cancelled so the idle spinner
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
    /// Temporary occupancy projected from a committed native compaction. A context
    /// refresh can still hold pre-compaction usage, so its sent-token field is
    /// ignored until the next real TokenUsage arrives.
    pub post_compaction_used_tokens: Option<usize>,
    /// Verbatim text of the message that is currently running. Set
    /// on every submit, cleared on turn-complete. When the user hits
    /// Ctrl+C / Esc mid-stream the streaming-key handler takes this
    /// and restores it to the input buffer so the cancelled message
    /// can be edited + resent without re-typing. `None` between
    /// turns and after any successful completion.
    pub last_submitted_message: Option<String>,
    /// Authoritative pasted `(images, markers)` of the last submitted turn,
    /// captured at submit BEFORE typed-path attachment so the image↔marker
    /// pairing stays exact (never re-derived from text, which can misorder).
    /// Re-attached on `VisionPreprocessFailed` so the user can retry without
    /// re-pasting. Overwritten each image-carrying submit; only read on
    /// failure. Parallel vecs (aligned by index).
    pub last_submitted_pasted_images: Vec<atomcode_kernel::message::ImageContent>,
    pub last_submitted_pasted_markers: Vec<usize>,
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
    pub pending_images: Vec<atomcode_kernel::message::ImageContent>,
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
    pub sub_agent_tasks: Vec<crate::event_loop::ui_event::SubAgentTaskInfo>,
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
    /// Active todo list for the persistent footer todo PANEL. Written from the
    /// turn's `todowrite` calls, seeded from the transcript on resume/switch
    /// (`replay_session`), reset on `/clear`/`/new` (`reset_to_new_session`).
    /// Unlike the old live-only row, this PERSISTS across turn boundaries — the
    /// panel is a standing view, hidden only when the list is empty or all done.
    pub active_todos: Option<crate::render::TodoProgress>,
    /// Current reasoning_effort level for the active provider.
    pub reasoning_effort: Option<String>,
    /// Active goal condition string, if a `/goal` is running.
    pub goal_condition: Option<String>,
    /// Current round number of the running goal loop.
    pub goal_round: u32,
    /// When the goal was started, for elapsed-time display.
    pub goal_started_at: Option<std::time::Instant>,
    /// Active loop label (prompt text), if a `/loop` is running.
    pub loop_label: Option<String>,
    /// Current round number of the running loop.
    pub loop_round: u32,
    /// When the loop was started, for elapsed-time display.
    pub loop_started_at: Option<std::time::Instant>,
    /// Per-turn stats buffered from the most recent `TurnComplete`. The
    /// separator line is NOT rendered immediately so that — if the next
    /// event happens to be `GoalUpdate(active=false)` (the goal just
    /// ended) — we can render the goal verdict banner ABOVE the line.
    /// Any other event flushes this buffer with the usual `↻ goal round N`
    /// or `✓ done` label.
    pub pending_separator: Option<PendingSeparator>,
    /// Mid-turn messages waiting for the next model-request boundary. Keeping
    /// the actual payload (not only a count) enables the Codex-style pending
    /// panel and fail-safe Esc → cancel → submit-as-new-turn behavior.
    pub pending_steers: std::collections::VecDeque<PendingSteer>,
    /// Whether the user has explicitly switched to Build mode via
    /// Tab/Shift+Tab or `/build`. When `false`, the status bar hides
    /// the `⏸ build` badge so the default startup state stays clean.
    pub build_badge_visible: bool,
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
    /// Whether the turn ran inside an active `/loop` (decides `⚡ loop round N`
    /// vs the normal summary when flushed mid-loop).
    pub was_loop_round: bool,
    /// Whether the turn ended abnormally (error, cancellation, or a safety
    /// limit). Lets the deferred flush render the ✗ "stopped" summary instead
    /// of a celebratory ✓ for an incomplete turn.
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
        Self::with_caps(unicode_symbols, true)
    }

    /// Construct a `UiState` with explicit Unicode and color capabilities.
    pub fn with_caps(unicode_symbols: bool, colors: bool) -> Self {
        Self {
            phase: UiPhase::Idle,
            agent_mode: AgentMode::default(),
            spinner_label: String::new(),
            spinner_frame: 0,
            spinner_last_advance: None,
            compacting: false,
            compaction_forced_streaming: false,
            subagent_activity: None,
            unicode_symbols,
            colors,
            total_tokens: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            footer_command_output: None,
            footer_usage: None,
            turn_prompt_tokens: 0,
            turn_completion_tokens: 0,
            turn_cached_tokens: 0,
            turn_output_chars: 0,
            turn_rendered_visible_text: false,
            turn_saw_reasoning: false,
            last_assistant_response: String::new(),
            response_finalized: false,
            last_turn_error: None,
            prior_phase: None,
            prior_spinner_label: None,
            approval_panel: None,
            user_input_panel: None,
            user_input_batch: None,
            thinking_idx: 0,
            turn_started_at: None,
            phase_started_at: None,
            last_stream_activity: None,
            last_context: None,
            post_compaction_used_tokens: None,
            last_submitted_message: None,
            last_submitted_pasted_images: Vec::new(),
            last_submitted_pasted_markers: Vec::new(),
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
            active_todos: None,
            reasoning_effort: None,
            goal_condition: None,
            goal_round: 0,
            goal_started_at: None,
            loop_label: None,
            loop_round: 0,
            loop_started_at: None,
            pending_separator: None,
            pending_steers: std::collections::VecDeque::new(),
            build_badge_visible: false,
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
    /// snapshot. The runtime may emit a narrow update followed by a rich update.
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
            // A rich emission with `sent_tokens == 0` means "no completed turn yet
            // this runtime generation" (`last_usage` is None), NOT a
            // genuine zero occupancy — `emit_context_stats` never reports a real 0.
            // Preserve a value restored from the persisted session on resume so
            // `/context`'s `RefreshContextStats` round-trip can't re-zero the gauge
            // the moment the user runs it. A real turn always sends `sent > 0`.
            if sent_tokens > 0 && self.post_compaction_used_tokens.is_none() {
                snap.sent_tokens = sent_tokens;
            }
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
        if sent_tokens > 0 && self.post_compaction_used_tokens.is_none() {
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

    /// Rehydrate the context gauge for the session being replayed, from its
    /// persisted last-turn usage (`TurnStat.used_tokens` / `ctx_window`).
    /// `replay_session` re-renders the transcript but never sees live
    /// `ContextStats` events, so without this the footer + `/context` read
    /// `0/100%` after `atomcode -c`, `/resume`, the session picker, or `/bg`
    /// until the next live turn lands.
    ///
    /// `sent_tokens` is set unconditionally (even to `0`): a session switch must
    /// show THIS session's occupancy, never the previous one's — passing `0` for
    /// a session with no completed turns correctly clears a stale gauge. The
    /// window is only filled when none is cached yet: a startup / model-switch
    /// seed reflects the CURRENT model and is a better denominator than a window
    /// persisted under a possibly-different model last session.
    ///
    /// No-ops only when there is nothing to show AND nothing to clear (`0/0`
    /// with no cached snapshot) so a fresh session still reads "no turns yet"
    /// rather than a fabricated `0/0` "waiting for window" state.
    pub fn restore_context(&mut self, used_tokens: usize, ctx_window: usize) {
        self.post_compaction_used_tokens = None;
        if used_tokens == 0 && ctx_window == 0 && self.last_context.is_none() {
            return;
        }
        let snap = self
            .last_context
            .get_or_insert_with(ContextSnapshot::default);
        snap.sent_tokens = used_tokens;
        if ctx_window > 0 && snap.ctx_window == 0 {
            snap.ctx_window = ctx_window;
        }
    }

    /// Project the committed `/compact` result into the context gauge without
    /// migrating the legacy `/context` command protocol in this slice.
    pub fn on_compaction_committed(&mut self, used_tokens: usize) {
        self.last_context
            .get_or_insert_with(ContextSnapshot::default)
            .sent_tokens = used_tokens;
        self.post_compaction_used_tokens = Some(used_tokens);
    }

    /// A provider usage event is authoritative and releases the temporary
    /// post-compaction projection before its ContextStats companion arrives.
    pub fn on_token_usage(&mut self) {
        self.post_compaction_used_tokens = None;
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

    /// Rough token estimate of this turn's streamed output, for the spinner's
    /// `↑ N tokens` activity indicator. ~4 chars/token (the usual English
    /// rule of thumb; only an at-a-glance liveness signal, not billing — the
    /// authoritative per-turn count comes from `turn_completion_tokens` at
    /// round end). Zero until the model starts emitting.
    pub fn turn_output_token_estimate(&self) -> usize {
        self.turn_output_chars / 4
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
        stream_stalled_for(
            awaiting_model,
            self.last_stream_activity.map(|t| t.elapsed()),
        )
    }

    /// Whether a running compaction's summary has stalled past
    /// [`STREAM_STALL_HINT`]. Mirrors [`Self::stream_stalled`] but gates on
    /// `compacting` instead of a THINKING_LABEL (the spinner shows
    /// "Compacting…", not a thinking label, during a compaction).
    pub fn compaction_stalled(&self) -> bool {
        stream_stalled_for(
            self.compacting,
            self.last_stream_activity.map(|t| t.elapsed()),
        )
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
        self.turn_output_chars = 0;
        // Seed the stall clock so the first silent stretch is measured from submit,
        // not a stale stamp from the previous turn (which would flash the warning).
        self.last_stream_activity = Some(now);
        // Belt-and-suspenders: a fresh turn always starts with zero steers, even
        // if the previous turn ended abnormally and never fired on_turn_complete.
        self.pending_steers.clear();
    }

    /// The runtime rejected the turn before accepting it. Keep the UI idle and
    /// discard the cancel-only restore stash; the committed text remains
    /// available through input history.
    pub(crate) fn on_submit_rejected(&mut self) {
        self.last_submitted_message = None;
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
        self.turn_output_chars = 0;
        self.turn_rendered_visible_text = false;
        self.turn_saw_reasoning = false;
        // Turn finished normally — no need to offer resubmit of the
        // message any more. (On cancel, the streaming-key handler
        // already took() the Option before the TurnCancelled event
        // reaches here, so the cancelled path naturally leaves this
        // None too.)
        self.last_submitted_message = None;
        // Same discipline for the subagent fan-out activity: no turn-end path may
        // leave a stale `explore#4 · …` pinned onto the next turn's spinner.
        self.subagent_activity = None;
        // Safety clear: if a turn ends without resolving an approval (e.g. error
        // path or session switch), ensure the panel is not left stale.
        self.approval_panel = None;
        self.user_input_panel = None;
        self.user_input_batch = None;
        self.pending_steers.clear();
        // The interactive `/usage` tab panel is streaming-only. Drop it (but keep
        // the rendered text) so its tab keys can't bleed into idle or across into
        // the next streaming turn; a fresh `/usage` re-arms it.
        self.footer_usage = None;
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
        self.turn_output_chars = 0;
        self.turn_rendered_visible_text = false;
        self.turn_saw_reasoning = false;
        self.subagent_activity = None;
        self.approval_panel = None;
        self.user_input_panel = None;
        self.user_input_batch = None;
        self.pending_steers.clear();
        // Streaming-only `/usage` tab panel — drop it on cancel too (mirrors
        // on_turn_complete); the rendered text stays until Esc.
        self.footer_usage = None;
        // The todo panel is per-session, not per-turn: it survives turn
        // termination (mirrors on_turn_complete). Clearing it here nuked the
        // plan, and a "继续" turn only sends incremental todowrite updates that
        // fold against the existing panel (a no-op on an empty base), so the
        // panel could never rebuild — it stayed gone while the model kept
        // executing. Only a session switch / new session / explicit clear drops
        // `active_todos` + `todo_titles`.
    }

    /// Drop presentation state that belongs to the outgoing session. Unlike a
    /// normal turn terminal, a session replacement must not keep `/usage` or
    /// `/cost` from the previous foreground session visible.
    pub fn on_session_replaced(&mut self) {
        self.footer_command_output = None;
        self.footer_usage = None;
    }

    /// The TUI dispatched a mid-turn steer to the kernel — one prompt now waiting
    /// to fold into the running turn at the next model-request boundary.
    pub fn on_steer_sent(&mut self, pending: PendingSteer) {
        self.pending_steers.push_back(pending);
    }

    /// The kernel confirmed it folded `count` steered prompt(s) into the turn
    /// (`AgentEvent::Steered`). Drain the pending count; `saturating_sub` so a
    /// steer folded from another client (no local `on_steer_sent`) can't underflow.
    pub fn on_steered(&mut self, count: usize) {
        for _ in 0..count {
            if self.pending_steers.pop_front().is_none() {
                break;
            }
        }
    }

    /// Record a diagnostic error observation without ending the turn locally.
    ///
    /// Kernel errors can precede the runtime-owned `TurnFinished` event (for
    /// example when a safety limit trips). Moving to `Idle` here would let the
    /// event loop submit type-ahead input into a turn that is still active.
    pub fn on_error(&mut self) {
        // The authoritative terminal owns all phase and per-turn cleanup.
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
        // Idle: a `/goal` continuation runs inside CodingRuntime (its controller injects the
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
        // EXCEPT during a parallel batch: the runtime projection emits PhaseChange(Thinking)
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
        tasks: Vec<crate::event_loop::ui_event::SubAgentTaskInfo>,
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
        self.spinner_label = format!(
            "Sub-agents {}/{}",
            self.sub_agent_done, self.sub_agent_total
        );
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
        self.approval_panel = None;
        self.phase = UiPhase::Streaming;
        if let Some(prior) = self.prior_spinner_label.take() {
            self.spinner_label = prior;
            // The tool is about to actually start running now (hook +
            // bash_execute). Restart the clock so the spinner suffix
            // reflects that, not the cumulative wait-then-run time.
            self.phase_started_at = Some(std::time::Instant::now());
        }
    }

    /// Clear the interactive question panel and return to streaming — the same
    /// shape as `on_approval_resolved` (the turn resumes once the tool receives
    /// its answer). No spinner-label stash to restore: unlike approval, the
    /// `Request` handler does not swap `spinner_label` when the panel opens.
    pub fn on_user_input_resolved(&mut self) {
        self.user_input_panel = None;
        self.user_input_batch = None;
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
        self.tick_spinner_at(std::time::Instant::now())
    }

    /// Frame-advance core, taking `now` so the rate limit is testable. Advances
    /// the frame at most once per [`SPINNER_MIN_ADVANCE`], no matter how often
    /// it's called: the interval timer (every 100ms) always clears the gate, but
    /// the per-token post-event redraws during streaming do NOT — that's what
    /// keeps the glyph from blurring while the model emits output.
    fn tick_spinner_at(&mut self, now: std::time::Instant) -> &'static str {
        // Two frame sets, picked once at construction:
        //   Unicode → braille dot rotation: a true single-cell glyph. The old
        //   half-moon set (◐◓◑◒, Geometric Shapes) is often given emoji
        //   presentation and renders oversized/double-width on modern
        //   terminals — braille avoids that and is the de-facto CLI spinner.
        //   ASCII   → classic `|/-\` for legacy terminals whose font lacks
        //   the Braille block (notably Windows legacy conhost); those already
        //   run with unicode_symbols = false, so they take this path.
        const UNICODE_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        const ASCII_FRAMES: &[&str] = &["|", "/", "-", "\\"];
        let frames = if self.unicode_symbols {
            UNICODE_FRAMES
        } else {
            ASCII_FRAMES
        };
        // Rate-limit the advance. `None` (first tick of a stream, or after a
        // reset) always advances so the spinner never appears frozen on start.
        let due = match self.spinner_last_advance {
            Some(prev) => now.duration_since(prev) >= SPINNER_MIN_ADVANCE,
            None => true,
        };
        if due {
            self.spinner_frame = (self.spinner_frame + 1) % frames.len();
            self.spinner_last_advance = Some(now);
        }
        frames[self.spinner_frame]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_frame_is_rate_limited_across_rapid_redraws() {
        use std::time::Duration;
        // Reproduces the "spins too fast during output" report: `tick_spinner`
        // runs once per streamed token (post-event redraw), which used to
        // advance the glyph every call. It must now advance at most once per
        // SPINNER_MIN_ADVANCE regardless of how often it's called.
        let mut s = UiState::with_unicode(true);
        let t0 = std::time::Instant::now();

        // First tick of the stream always advances (last_advance is None).
        let _ = s.tick_spinner_at(t0);
        let after_first = s.spinner_frame;

        // A flood of per-token redraws inside one gap must NOT advance.
        for ms in [1, 10, 40, 79] {
            let _ = s.tick_spinner_at(t0 + Duration::from_millis(ms));
            assert_eq!(
                s.spinner_frame, after_first,
                "redraw at +{ms}ms (< gap) must not advance the spinner",
            );
        }

        // Once the gap elapses (the 100ms interval timer), it advances once.
        let _ = s.tick_spinner_at(t0 + Duration::from_millis(80));
        assert_ne!(
            s.spinner_frame, after_first,
            "spinner must advance once SPINNER_MIN_ADVANCE has elapsed",
        );
        let after_second = s.spinner_frame;

        // ...and only once more even if hammered again within the next gap.
        for ms in [85, 120, 159] {
            let _ = s.tick_spinner_at(t0 + Duration::from_millis(ms));
        }
        assert_eq!(
            s.spinner_frame, after_second,
            "a second flood inside the next gap must not advance again",
        );
    }

    #[test]
    fn spinner_warns_only_when_model_stream_silent_past_threshold() {
        use std::time::Duration;
        // Awaiting model + silent at/over the threshold → warn (network may be down).
        assert!(stream_stalled_for(true, Some(STREAM_STALL_HINT)));
        assert!(stream_stalled_for(
            true,
            Some(STREAM_STALL_HINT + Duration::from_secs(5))
        ));
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
    fn restore_context_rehydrates_gauge_after_resume() {
        // After `/resume`, replay never sees live ContextStats — restore_context
        // seeds the gauge from the persisted last-turn usage so the footer +
        // `/context` show the real occupancy immediately.
        let mut s = UiState::new();
        assert!(s.last_context.is_none());
        s.restore_context(42_000, 200_000);
        let snap = s.last_context.as_ref().expect("gauge rehydrated");
        assert_eq!(snap.sent_tokens, 42_000);
        assert_eq!(snap.ctx_window, 200_000);
    }

    #[test]
    fn restore_context_noop_for_old_session_without_persisted_usage() {
        // Sessions saved before the persisted usage fields load as 0/0 — restore
        // must not fabricate a snapshot (gauge stays empty until the next turn).
        let mut s = UiState::new();
        s.restore_context(0, 0);
        assert!(
            s.last_context.is_none(),
            "must not seed a snapshot from 0/0"
        );
    }

    #[test]
    fn refresh_with_zero_sent_does_not_clobber_restored_gauge() {
        // `/context` after resume dispatches RefreshContextStats; the runtime can
        // reply with sent_tokens=0 (last_usage None) but a real window. That
        // "unknown" 0 must not wipe the value restored from the persisted session.
        let mut s = UiState::new();
        s.restore_context(42_000, 200_000);
        // Simulate the RefreshContextStats reply (rich: window > 0, sent = 0).
        s.on_context_stats(0, 0, 0, 0, 0, 200_000, "engine-v2", "");
        let snap = s.last_context.as_ref().unwrap();
        assert_eq!(snap.sent_tokens, 42_000, "restored occupancy preserved");
        assert_eq!(snap.ctx_window, 200_000);
    }

    #[test]
    fn stale_context_refresh_does_not_clobber_post_compaction_usage() {
        let mut s = UiState::new();
        s.on_context_stats(0, 40_000, 0, 0, 10, 128_000, "engine-v2", "");
        s.on_compaction_committed(12_000);

        s.on_context_stats(0, 40_000, 0, 0, 6, 128_000, "engine-v2", "");

        assert_eq!(s.last_context.as_ref().unwrap().sent_tokens, 12_000);
        assert_eq!(s.post_compaction_used_tokens, Some(12_000));
    }

    #[test]
    fn real_usage_releases_post_compaction_projection() {
        let mut s = UiState::new();
        s.on_compaction_committed(12_000);
        s.on_token_usage();
        s.on_context_stats(0, 14_000, 0, 0, 8, 128_000, "engine-v2", "");

        assert_eq!(s.last_context.as_ref().unwrap().sent_tokens, 14_000);
        assert!(s.post_compaction_used_tokens.is_none());
    }

    #[test]
    fn restore_context_keeps_seeded_window_when_none_persisted() {
        // If startup already seeded the current model's window, a persisted
        // used-only restore must fill the used count without zeroing the window.
        let mut s = UiState::new();
        s.on_model_window_changed(128_000);
        s.restore_context(9_000, 0);
        let snap = s.last_context.as_ref().unwrap();
        assert_eq!(snap.sent_tokens, 9_000);
        assert_eq!(snap.ctx_window, 128_000, "seeded window preserved");
    }

    #[test]
    fn restore_context_prefers_seeded_window_over_stale_persisted() {
        // Current model's window (seeded at startup) beats a window persisted
        // under a possibly-different model last session.
        let mut s = UiState::new();
        s.on_model_window_changed(128_000);
        s.restore_context(9_000, 1_000_000);
        assert_eq!(s.last_context.as_ref().unwrap().ctx_window, 128_000);
    }

    #[test]
    fn restore_context_clears_previous_session_gauge_on_switch() {
        // Switching from a big session (A) to one with no completed turns (B)
        // must reset the gauge to 0, not leave A's occupancy on screen.
        let mut s = UiState::new();
        s.on_context_stats(0, 500_000, 0, 0, 0, 1_000_000, "engine-v2", ""); // session A
        s.restore_context(0, 0); // replay of empty session B
        let snap = s.last_context.as_ref().unwrap();
        assert_eq!(snap.sent_tokens, 0, "A's occupancy must be cleared");
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
        assert!(
            s.stream_stalled(),
            "silent model stream past threshold must warn"
        );
        s.on_tool_call_started("Bash"); // spinner = "Running Bash" — not a thinking label
        assert!(
            !s.stream_stalled(),
            "a slow tool must NOT trip the network warning"
        );
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
        use std::time::Duration;
        let mut s = UiState::with_unicode(false);
        let t0 = std::time::Instant::now();
        let mut seen = Vec::new();
        // Space the ticks past SPINNER_MIN_ADVANCE so each one actually advances
        // (the rate limit only collapses redraws inside a single gap).
        for i in 0..4 {
            seen.push(s.tick_spinner_at(t0 + Duration::from_millis(i * 100)));
        }
        // Order is implementation detail; the SET must match.
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["-", "/", "\\", "|"]);
    }

    #[test]
    fn unicode_spinner_uses_single_cell_braille() {
        use std::time::Duration;
        // Braille dots are true single-cell glyphs; the old half-moon set
        // (◐◓◑◒) was often given emoji presentation and rendered oversized.
        let mut s = UiState::with_unicode(true);
        let t0 = std::time::Instant::now();
        let mut seen = Vec::new();
        // Walk a full cycle (frame count is an implementation detail), spacing
        // ticks past the frame-advance rate limit.
        for i in 0..10 {
            seen.push(s.tick_spinner_at(t0 + Duration::from_millis(i * 100)));
        }
        // Every frame is a Braille Patterns glyph (U+2800..=U+28FF).
        for f in &seen {
            let ch = f.chars().next().unwrap();
            assert_eq!(f.chars().count(), 1, "frame {f:?} must be a single char");
            assert!(
                ('\u{2800}'..='\u{28FF}').contains(&ch),
                "frame {f:?} must be a Braille glyph"
            );
        }
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
    fn footer_report_survives_turn_terminal_but_not_session_replacement() {
        let mut s = UiState::new();
        s.footer_command_output = Some("current session usage".into());
        s.on_turn_complete();
        assert!(
            s.footer_command_output.is_some(),
            "normal turn completion keeps the report until Esc"
        );

        s.on_session_replaced();
        assert!(
            s.footer_command_output.is_none(),
            "a new foreground session must not inherit the old report"
        );
    }

    fn sample_usage_panel() -> crate::modals::usage::UsageModal {
        crate::modals::usage::UsageModal::new(crate::modals::usage::UsageData {
            window: None,
            plan: None,
            usage: None,
            overview: None,
            error: None,
        })
    }

    #[test]
    fn turn_terminal_drops_live_usage_panel_but_keeps_the_text() {
        // The interactive `/usage` panel is a streaming-only affordance. When the
        // turn ends it must degrade to the static footer text (the report stays
        // visible until Esc), so its tab keys can't leak into — or bleed across
        // into the next streaming turn from — the idle phase.
        for terminal in [UiState::on_turn_complete, UiState::on_turn_cancelled] {
            let mut s = UiState::new();
            s.footer_command_output = Some("usage report".into());
            s.footer_usage = Some(sample_usage_panel());

            terminal(&mut s);

            assert!(
                s.footer_usage.is_none(),
                "turn end must drop the interactive panel"
            );
            assert!(
                s.footer_command_output.is_some(),
                "but the rendered report text stays until Esc"
            );
        }
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
        s.pending_images
            .push(atomcode_kernel::message::ImageContent {
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
        assert_eq!(
            s.phase_started_at,
            Some(anchor),
            "streaming restarted the batch clock"
        );
        s.on_tool_call_started("Bash(b)");
        assert_eq!(
            s.phase_started_at,
            Some(anchor),
            "started restarted the batch clock"
        );
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
        // The runtime projection emits PhaseChange(Thinking) after every ToolResult, so in a
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
        assert_eq!(
            s.phase_started_at,
            Some(anchor),
            "thinking restarted the batch clock"
        );
    }

    #[test]
    fn thinking_resets_phase_clock_when_no_batch() {
        // Outside a batch, a new think is a genuine new phase → clock resets.
        let mut s = UiState::new();
        s.on_submit();
        let before = s.phase_started_at.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        s.on_thinking();
        assert!(
            s.phase_started_at.unwrap() > before,
            "think must reset the clock outside a batch"
        );
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
    fn error_is_diagnostic_until_the_authoritative_terminal() {
        let mut s = UiState::new();
        s.on_submit();
        s.on_error();
        assert_eq!(s.phase, UiPhase::Streaming);
    }

    #[test]
    fn agent_mode_default_is_build() {
        assert_eq!(AgentMode::default(), AgentMode::Build);
    }

    fn task_info(path: &str, dedup: &str) -> crate::event_loop::ui_event::SubAgentTaskInfo {
        crate::event_loop::ui_event::SubAgentTaskInfo {
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
        let tasks = (0..6)
            .map(|i| task_info(&format!("a{}.rs", i), ""))
            .collect();
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
    fn agent_mode_build_next_is_accept_edits() {
        assert_eq!(AgentMode::Build.next(), AgentMode::AcceptEdits);
        assert_eq!(AgentMode::AcceptEdits.next(), AgentMode::Auto);
    }

    #[test]
    fn agent_mode_auto_next_is_plan() {
        assert_eq!(AgentMode::Auto.next(), AgentMode::Plan);
    }

    #[test]
    fn agent_mode_plan_next_is_build() {
        assert_eq!(AgentMode::Plan.next(), AgentMode::Build);
    }

    #[test]
    fn agent_mode_cycle_forward_and_back() {
        assert_eq!(AgentMode::Build.next().prev(), AgentMode::Build);
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
            images: vec![atomcode_kernel::message::ImageContent {
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

    #[test]
    fn active_todos_persists_across_turn_end() {
        let mut s = UiState::new();
        s.active_todos = Some(crate::render::TodoProgress {
            current: Some("x".into()),
            completed: 0,
            in_progress: 0,
            total: 2,
            items: vec![],
        });
        s.on_turn_complete();
        assert!(s.active_todos.is_some(), "panel must survive turn end");
    }

    #[test]
    fn active_todos_persist_across_cancel_and_error() {
        // The todo panel is PER-SESSION state (see reset_to_new_session /
        // native SessionChanged, which drop it), NOT per-turn — it already survives
        // turn COMPLETE (active_todos_persists_across_turn_end). Cancel and
        // error are just turn terminations too, so they must preserve it as
        // well. Regression: clearing on cancel/error nuked the plan, and since
        // a "继续" turn sends only INCREMENTAL todowrite updates (which fold
        // against the existing panel via apply_todo_action — a no-op on an
        // empty base), the panel could never rebuild. It stayed gone on screen
        // while the model kept executing the plan from its own context. Only an
        // explicit clear or a session switch may drop the panel.
        let mut s = UiState::new();
        s.active_todos = Some(crate::render::TodoProgress {
            current: Some("Task".to_string()),
            completed: 2,
            in_progress: 1,
            total: 3,
            items: vec![],
        });
        s.todo_titles.insert(1, "Todo".to_string());

        s.on_turn_cancelled();
        assert!(s.active_todos.is_some(), "panel must survive cancellation");
        assert!(
            !s.todo_titles.is_empty(),
            "titles must survive cancellation"
        );

        s.on_error();
        assert!(s.active_todos.is_some(), "panel must survive error");
        assert!(!s.todo_titles.is_empty(), "titles must survive error");
    }

    #[test]
    fn approval_panel_selection_wraps_and_accel_maps() {
        use crate::state::{ApprovalKind, ApprovalOption, ApprovalPanel};
        let mut p = ApprovalPanel {
            tool: "bash".into(),
            detail: "rm -rf build/".into(),
            options: vec![
                ApprovalOption {
                    label: "Allow once".into(),
                    kind: ApprovalKind::AllowOnce,
                    accel: 'y',
                },
                ApprovalOption {
                    label: "Always allow bash".into(),
                    kind: ApprovalKind::AlwaysAllow,
                    accel: 'a',
                },
                ApprovalOption {
                    label: "Deny".into(),
                    kind: ApprovalKind::Deny,
                    accel: 'n',
                },
            ],
            selected: 0,
            cache_key: String::new(),
        };
        p.move_up();
        assert_eq!(p.selected, 2, "up from 0 wraps to last");
        p.move_down();
        assert_eq!(p.selected, 0, "down from last wraps to 0");
        p.move_down();
        assert_eq!(p.selected, 1);
        assert_eq!(p.accel_index('A'), Some(1), "accel is case-insensitive");
        assert_eq!(p.accel_index('n'), Some(2));
        assert_eq!(p.accel_index('z'), None);
    }

    #[test]
    fn user_input_panel_navigation_toggle_and_chosen() {
        use crate::state::UserInputPanel;
        use atomcode_capabilities::tools::request_user_input::{
            UserInputMode, UserInputOption, UserInputRequest,
        };

        let single_req = UserInputRequest {
            header: "Auth".into(),
            question: "Which flow?".into(),
            mode: UserInputMode::Single,
            options: vec![
                UserInputOption {
                    label: "OAuth".into(),
                    description: None,
                },
                UserInputOption {
                    label: "API key".into(),
                    description: None,
                },
                UserInputOption {
                    label: "Device".into(),
                    description: None,
                },
            ],
            custom: true,
        };
        let mut p = UserInputPanel::new(7, &single_req);
        assert_eq!(p.request_id, 7);
        assert_eq!(p.cursor, 0);
        // Single: cursor spans concrete options (0..3) and the always-appended
        // "Other" row (3). No submit row. move_down clamps at the "Other" row.
        for _ in 0..10 {
            p.move_down();
        }
        assert_eq!(
            p.cursor, 3,
            "single move_down clamps at the Other row (options.len())"
        );
        assert!(p.is_other_row(), "single cursor clamps at the Other row");
        // move_up clamps at 0.
        for _ in 0..10 {
            p.move_up();
        }
        assert_eq!(p.cursor, 0, "move_up clamps at 0");
        // Single is cursor-as-radio: chosen() reads the cursor row's label.
        p.move_down(); // cursor → "API key"
        assert_eq!(p.chosen(), vec!["API key".to_string()]);
        let resp = p
            .build_response()
            .expect("single with a cursor selection submits");
        assert_eq!(resp.selected, vec!["API key".to_string()]);
        assert_eq!(resp.text, None);
        // Moving the cursor changes the radio selection.
        p.move_up();
        assert_eq!(
            p.chosen(),
            vec!["OAuth".to_string()],
            "single radio follows the cursor"
        );

        // Single: cursor on the "Other" row + typed custom text submits it.
        for _ in 0..10 {
            p.move_down();
        }
        assert!(p.is_other_row());
        p.push_custom('h');
        p.push_custom('i');
        let resp = p.build_response().expect("custom text submits");
        assert_eq!(resp.selected, vec!["hi".to_string()]);
        // "Other" row with empty custom text → no-op.
        let mut other_empty = UserInputPanel::new(12, &single_req);
        for _ in 0..10 {
            other_empty.move_down();
        }
        assert!(other_empty.is_other_row());
        assert!(
            other_empty.build_response().is_none(),
            "empty Other on single does not submit"
        );

        // Multiple: toggle flips the highlighted option; chosen() = all checked.
        let mut m = UserInputPanel::new(
            9,
            &UserInputRequest {
                mode: UserInputMode::Multiple,
                ..single_req.clone()
            },
        );
        m.toggle(); // check "OAuth"
        m.move_down();
        m.move_down();
        m.toggle(); // check "Device"
        assert_eq!(m.chosen(), vec!["OAuth".to_string(), "Device".to_string()]);
        m.toggle(); // uncheck "Device"
        assert_eq!(m.chosen(), vec!["OAuth".to_string()], "toggle flips off");
        // Multiple: custom text is APPENDED whenever non-empty (text-derived,
        // no separate checked flag needed).
        m.push_custom('x');
        let resp = m.build_response().expect("multiple always submits");
        assert_eq!(resp.selected, vec!["OAuth".to_string(), "x".to_string()]);

        // Text: chosen() is empty (answer rides `text`).
        let mut t = UserInputPanel::new(
            1,
            &UserInputRequest {
                mode: UserInputMode::Text,
                options: vec![],
                ..single_req.clone()
            },
        );
        t.text.push_str("hello");
        assert!(t.chosen().is_empty(), "text mode has no selected labels");
        assert_eq!(t.text, "hello");
        assert_eq!(t.build_response().unwrap().text.as_deref(), Some("hello"));
    }

    #[test]
    fn on_user_input_resolved_clears_panel_and_resumes() {
        use atomcode_capabilities::tools::request_user_input::{UserInputMode, UserInputRequest};
        let mut s = UiState::new();
        s.user_input_panel = Some(crate::state::UserInputPanel::new(
            3,
            &UserInputRequest {
                header: "H".into(),
                question: "Q?".into(),
                mode: UserInputMode::Text,
                options: vec![],
                custom: true,
            },
        ));
        s.phase = UiPhase::UserInput;
        s.on_user_input_resolved();
        assert!(s.user_input_panel.is_none(), "panel cleared on resolve");
        assert_eq!(s.phase, UiPhase::Streaming, "phase resumes to Streaming");
    }

    #[test]
    fn pending_steers_retain_payload_and_drain_on_fold() {
        fn pending(text: &str) -> PendingSteer {
            PendingSteer {
                message: QueuedMessage {
                    text: text.to_owned(),
                    images: Vec::new(),
                    image_markers: Vec::new(),
                },
                display_text: text.to_owned(),
            }
        }

        let mut st = UiState::new();
        st.on_steer_sent(pending("one"));
        st.on_steer_sent(pending("two"));
        assert_eq!(
            st.pending_steers
                .iter()
                .map(|pending| pending.display_text.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"],
            "payload and display order survive until kernel confirmation"
        );
        st.on_steered(1); // kernel confirms one fold
        assert_eq!(
            st.pending_steers.front().map(|p| p.display_text.as_str()),
            Some("two"),
            "drains from the front as folds are confirmed"
        );
        st.on_steered(1);
        assert!(st.pending_steers.is_empty());
        st.on_steered(3); // spurious / cross-client fold never underflows
        assert!(st.pending_steers.is_empty());
        // Every turn-end path clears it, parity with the other per-turn counters.
        st.on_steer_sent(pending("complete"));
        st.on_turn_complete();
        assert!(st.pending_steers.is_empty(), "cleared at turn end");
        st.on_steer_sent(pending("cancel"));
        st.on_turn_cancelled();
        assert!(st.pending_steers.is_empty(), "cleared on cancel");
        st.on_steer_sent(pending("diagnostic"));
        st.on_error();
        assert_eq!(
            st.pending_steers.len(),
            1,
            "a diagnostic error must not clean up a still-active turn"
        );
        st.on_turn_complete();
        assert!(st.pending_steers.is_empty(), "cleared by the terminal");
        st.on_steer_sent(pending("fresh"));
        st.on_submit();
        assert!(st.pending_steers.is_empty(), "a fresh turn starts empty");
    }

    #[test]
    fn rejected_submit_discards_cancel_restore_without_entering_streaming() {
        let mut st = UiState::new();
        st.last_submitted_message = Some("not accepted".to_string());

        st.on_submit_rejected();

        assert_eq!(st.phase, UiPhase::Idle);
        assert!(st.last_submitted_message.is_none());
    }
}
