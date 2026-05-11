// crates/atomcode-tuix/src/modals/onboarding_wizard.rs
//
// Multi-step first-run / `/welcome` onboarding wizard. Three real
// steps (Intro / Language / Setup) plus one synthetic Confirm step
// that fires only when `/welcome` runs mid-session and would clobber
// existing scrollback.
//
// Replaces `welcome_wizard.rs` (deleted in Task 9). Same `LoopCtx`
// post-close flag side-channel (`pending_run_codingplan`,
// `pending_open_provider_wizard`) as before — only the in-modal flow
// changes.
//
// This file lands in slices across the plan tasks:
//   * Task 2 (this slice): `draw_panel` box-drawing helper + tests.
//   * Task 3: `OnboardingWizard` struct + Step enum + transitions.
//   * Task 4-6: per-step `draw_*` + Modal trait impl.

use unicode_width::UnicodeWidthStr;

/// Build the lines of a Cyan-bordered panel.
///
/// Returns one string per terminal row: top border with title, content
/// lines with side borders + padding, bottom border with step indicator.
/// `width` is the total external width including both border columns;
/// inner content area is `width - 4` (2 padding cells on each side).
///
/// The returned strings include SGR colour codes so the renderer paints
/// the borders cyan and the title brand-magenta. Pass these strings to
/// `UiLine::CommandOutput`.
pub(super) fn draw_panel(
    title: &str,
    content: &[String],
    step_indicator: &str,
    width: usize,
) -> Vec<String> {
    use crossterm::style::{Color, ResetColor, SetForegroundColor};
    let border = Color::Cyan; // Palette::BORDER
    let brand = Color::Magenta; // Palette::BRAND

    let mut out = Vec::with_capacity(content.len() + 2);
    let inner_width = width.saturating_sub(4);

    // Top border: ┌─ <title> ─...─┐
    let title_seg = format!(" {title} ");
    let title_width = UnicodeWidthStr::width(title_seg.as_str());
    let dashes_after = inner_width.saturating_sub(title_width);
    let top = format!(
        "{b}┌─{r}{br}{tt}{r}{b}{dash}─┐{r}",
        b = SetForegroundColor(border),
        br = SetForegroundColor(brand),
        tt = title_seg,
        dash = "─".repeat(dashes_after),
        r = ResetColor,
    );
    out.push(top);

    // Content rows: │ <2 sp pad> <line padded to inner_width-2> <2 sp pad> │
    for line in content {
        let line_width = UnicodeWidthStr::width(line.as_str());
        let pad = (inner_width.saturating_sub(2)).saturating_sub(line_width);
        let row = format!(
            "{b}│{r}  {line}{pad}  {b}│{r}",
            b = SetForegroundColor(border),
            r = ResetColor,
            line = line,
            pad = " ".repeat(pad),
        );
        out.push(row);
    }

    // Bottom border: └─ <step_indicator> ─...─┘
    let step_seg = format!(" {step_indicator} ");
    let step_w = UnicodeWidthStr::width(step_seg.as_str());
    let dashes_after_step = inner_width.saturating_sub(step_w);
    let bot = format!(
        "{b}└─{step_seg}{dash}─┘{r}",
        b = SetForegroundColor(border),
        step_seg = step_seg,
        dash = "─".repeat(dashes_after_step),
        r = ResetColor,
    );
    out.push(bot);

    out
}

// ───────────────────────────────────────────────────────────────────
// State machine
// ───────────────────────────────────────────────────────────────────
//
// `OnboardingWizard` owns the selection indices and a `Step` cursor.
// Keyboard input flows through `handle_key_pure`, which mutates state
// and returns a `PureOutcome` describing what the Modal-trait wrapper
// (Task 6) should do with the world (apply locale, set pending_*
// flags, clear+redraw, etc.). Splitting the side effects out keeps
// state-machine tests trivially `LoopCtx`-free.

use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Synthetic pre-step shown only when `/welcome` is invoked
    /// mid-session AND there's prior conversation. y/Y advances to
    /// Intro after `clear_screen`; n/N/Esc closes without clearing.
    Confirm,
    Intro,
    Language,
    Setup,
}

pub struct OnboardingWizard {
    pub(super) step: Step,
    /// 0=Auto-detect, 1=English, 2=ZhCn
    pub(super) language_idx: usize,
    /// 0=CodingPlan, 1=Manual, 2=Skip
    pub(super) setup_idx: usize,
    /// Set when constructed via `/welcome` mid-session with non-empty
    /// body. Drives whether step starts at Confirm or Intro.
    pub(super) needs_confirm: bool,
}

impl OnboardingWizard {
    /// Standard constructor — first-run or `/welcome` with empty body.
    pub fn new() -> Self {
        Self {
            step: Step::Intro,
            language_idx: 0,
            setup_idx: 0,
            needs_confirm: false,
        }
    }

    /// Constructor for `/welcome` mid-session when body is non-empty.
    /// Wizard opens at the synthetic Confirm step; user must press y
    /// before any clear or further drawing happens.
    pub fn new_with_confirm() -> Self {
        Self {
            step: Step::Confirm,
            language_idx: 0,
            setup_idx: 0,
            needs_confirm: true,
        }
    }

    /// Pre-select the language idx based on existing config. Used by
    /// `/welcome` so a user who already picked ZhCn lands on row 3 of
    /// step 2 instead of Auto-detect.
    pub fn with_initial_language(
        mut self,
        config_lang: Option<atomcode_core::locale::Locale>,
    ) -> Self {
        self.language_idx = match config_lang {
            None => 0,
            Some(atomcode_core::locale::Locale::En) => 1,
            Some(atomcode_core::locale::Locale::ZhCn) => 2,
        };
        self
    }

    /// Test-only: dispatch a single key with no modifiers, ignoring
    /// any side-effect outcome the pure handler returns. Used for
    /// state-machine unit tests; real Modal::handle_key (Task 6) reads
    /// the outcome and drives ctx mutations + redraws accordingly.
    #[cfg(test)]
    pub(super) fn handle_key_for_test(&mut self, code: KeyCode) {
        let _ = self.handle_key_pure(code, KeyModifiers::NONE);
    }

    /// Pure key handling: only mutates `self`, no side effects against
    /// the world. The Modal::handle_key wrapper (Task 6) calls this,
    /// then performs the i18n / config / flag side effects based on
    /// the returned `PureOutcome`.
    pub(super) fn handle_key_pure(
        &mut self,
        code: KeyCode,
        _mods: KeyModifiers,
    ) -> PureOutcome {
        use Step::*;
        match (self.step, code) {
            // Confirm
            (Confirm, KeyCode::Char('y')) | (Confirm, KeyCode::Char('Y')) => {
                self.step = Intro;
                PureOutcome::ClearAndRedraw
            }
            (Confirm, KeyCode::Char('n'))
            | (Confirm, KeyCode::Char('N'))
            | (Confirm, KeyCode::Esc) => PureOutcome::Close,

            // Intro
            (Intro, KeyCode::Enter) => {
                self.step = Language;
                PureOutcome::ClearAndRedraw
            }
            (Intro, KeyCode::Esc) => PureOutcome::Close,
            // Left arrow at intro is no-op (no previous step).

            // Language
            (Language, KeyCode::Up) => {
                self.language_idx = self.language_idx.saturating_sub(1);
                PureOutcome::Redraw
            }
            (Language, KeyCode::Down) => {
                if self.language_idx < 2 {
                    self.language_idx += 1;
                }
                PureOutcome::Redraw
            }
            (Language, KeyCode::Char('1')) => {
                self.language_idx = 0;
                PureOutcome::Redraw
            }
            (Language, KeyCode::Char('2')) => {
                self.language_idx = 1;
                PureOutcome::Redraw
            }
            (Language, KeyCode::Char('3')) => {
                self.language_idx = 2;
                PureOutcome::Redraw
            }
            (Language, KeyCode::Enter) => PureOutcome::ApplyLanguageThenAdvance,
            (Language, KeyCode::Left) => {
                self.step = Intro;
                PureOutcome::ClearAndRedraw
            }
            (Language, KeyCode::Esc) => PureOutcome::Close,

            // Setup
            (Setup, KeyCode::Up) => {
                self.setup_idx = self.setup_idx.saturating_sub(1);
                PureOutcome::Redraw
            }
            (Setup, KeyCode::Down) => {
                if self.setup_idx < 2 {
                    self.setup_idx += 1;
                }
                PureOutcome::Redraw
            }
            (Setup, KeyCode::Char('1')) => {
                self.setup_idx = 0;
                PureOutcome::Redraw
            }
            (Setup, KeyCode::Char('2')) => {
                self.setup_idx = 1;
                PureOutcome::Redraw
            }
            (Setup, KeyCode::Char('3')) => {
                self.setup_idx = 2;
                PureOutcome::Redraw
            }
            (Setup, KeyCode::Enter) => PureOutcome::ApplySetupThenClose,
            (Setup, KeyCode::Left) => {
                self.step = Language;
                PureOutcome::ClearAndRedraw
            }
            (Setup, KeyCode::Esc) => PureOutcome::Close,

            _ => PureOutcome::Noop,
        }
    }
}

impl Default for OnboardingWizard {
    fn default() -> Self {
        Self::new()
    }
}

/// Outcome of `handle_key_pure` — what the Modal-trait wrapper should
/// do with the world after the pure transition. Splitting this out
/// keeps state-machine tests free of LoopCtx / renderer mocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PureOutcome {
    /// Modal stays open; redraw on next tick (selection moved within
    /// step, no step transition).
    Redraw,
    /// Modal stays open; clear screen + redraw (step transitioned).
    ClearAndRedraw,
    /// Apply language pick (i18n::set_locale + persist), then
    /// transition to Setup + ClearAndRedraw.
    ApplyLanguageThenAdvance,
    /// Set the appropriate `pending_*` flag based on `setup_idx`, then
    /// close.
    ApplySetupThenClose,
    /// Close modal, no side effect.
    Close,
    /// Ignore the key.
    Noop,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Strip every SGR escape so we can assert on the visible glyphs.
    fn strip_sgr(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == 'm' || n.is_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn top_border_has_title() {
        let lines = draw_panel("AtomCode", &[], "Step 1/3", 60);
        let plain = strip_sgr(&lines[0]);
        assert!(plain.starts_with("┌─ AtomCode "));
        assert!(plain.ends_with("─┐"));
    }

    #[test]
    fn bottom_border_has_step_indicator() {
        let lines = draw_panel("AtomCode", &[], "Step 1/3", 60);
        let plain = strip_sgr(lines.last().unwrap());
        assert!(plain.starts_with("└─ Step 1/3 "));
        assert!(plain.ends_with("─┘"));
    }

    #[test]
    fn content_lines_are_padded_to_width() {
        let content = vec!["hello".to_string(), "".to_string()];
        let lines = draw_panel("X", &content, "Y", 30);
        // Lines 1 & 2 are content. Each must be exactly `width` wide
        // when measured by visible-grid columns.
        for line in &lines[1..=2] {
            let plain = strip_sgr(line);
            assert_eq!(
                UnicodeWidthStr::width(plain.as_str()),
                30,
                "line not padded to 30: {plain:?}"
            );
        }
    }

    #[test]
    fn cjk_content_pads_correctly() {
        // Each CJK char is 2 cols. "中文" = 4 cols.
        let content = vec!["中文".to_string()];
        let lines = draw_panel("X", &content, "Y", 30);
        let plain = strip_sgr(&lines[1]);
        assert_eq!(UnicodeWidthStr::width(plain.as_str()), 30);
    }

    #[test]
    fn narrow_terminal_does_not_panic() {
        // width=10 has inner_width=6 which won't fit "AtomCode" title;
        // saturating_sub guards against underflow. We just assert it
        // doesn't panic and produces *some* output.
        let lines = draw_panel("AtomCode", &["x".into()], "S", 10);
        assert_eq!(lines.len(), 3); // top + 1 content + bottom
    }

    // ── state-machine transition tests ──

    fn make_wizard() -> OnboardingWizard {
        OnboardingWizard::new()
    }

    #[test]
    fn new_starts_at_intro() {
        let w = make_wizard();
        assert_eq!(w.step, Step::Intro);
        assert_eq!(w.setup_idx, 0);
        assert_eq!(w.language_idx, 0);
        assert!(!w.needs_confirm);
    }

    #[test]
    fn new_with_confirm_starts_at_confirm_step() {
        let w = OnboardingWizard::new_with_confirm();
        assert_eq!(w.step, Step::Confirm);
        assert!(w.needs_confirm);
    }

    #[test]
    fn with_initial_language_seeds_idx() {
        use atomcode_core::locale::Locale;
        assert_eq!(make_wizard().with_initial_language(None).language_idx, 0);
        assert_eq!(
            make_wizard()
                .with_initial_language(Some(Locale::En))
                .language_idx,
            1
        );
        assert_eq!(
            make_wizard()
                .with_initial_language(Some(Locale::ZhCn))
                .language_idx,
            2
        );
    }

    #[test]
    fn intro_enter_advances_to_language() {
        let mut w = make_wizard();
        w.handle_key_for_test(KeyCode::Enter);
        assert_eq!(w.step, Step::Language);
    }

    #[test]
    fn language_left_arrow_returns_to_intro() {
        let mut w = make_wizard();
        w.step = Step::Language;
        w.handle_key_for_test(KeyCode::Left);
        assert_eq!(w.step, Step::Intro);
    }

    #[test]
    fn intro_left_arrow_is_noop() {
        let mut w = make_wizard();
        w.handle_key_for_test(KeyCode::Left);
        assert_eq!(w.step, Step::Intro);
    }

    #[test]
    fn language_up_down_moves_idx() {
        let mut w = make_wizard();
        w.step = Step::Language;
        w.language_idx = 1;
        w.handle_key_for_test(KeyCode::Down);
        assert_eq!(w.language_idx, 2);
        w.handle_key_for_test(KeyCode::Down);
        assert_eq!(w.language_idx, 2, "should not exceed last index");
        w.handle_key_for_test(KeyCode::Up);
        assert_eq!(w.language_idx, 1);
        w.handle_key_for_test(KeyCode::Up);
        w.handle_key_for_test(KeyCode::Up);
        assert_eq!(w.language_idx, 0, "saturating_sub keeps idx at 0");
    }

    #[test]
    fn setup_up_down_bounded() {
        let mut w = make_wizard();
        w.step = Step::Setup;
        w.setup_idx = 0;
        w.handle_key_for_test(KeyCode::Up);
        assert_eq!(w.setup_idx, 0);
        w.handle_key_for_test(KeyCode::Down);
        assert_eq!(w.setup_idx, 1);
        w.handle_key_for_test(KeyCode::Down);
        w.handle_key_for_test(KeyCode::Down);
        assert_eq!(w.setup_idx, 2);
        w.handle_key_for_test(KeyCode::Down);
        assert_eq!(w.setup_idx, 2);
    }

    #[test]
    fn number_keys_jump_select() {
        let mut w = make_wizard();
        w.step = Step::Language;
        w.handle_key_for_test(KeyCode::Char('3'));
        assert_eq!(w.language_idx, 2);
        w.handle_key_for_test(KeyCode::Char('1'));
        assert_eq!(w.language_idx, 0);
    }

    #[test]
    fn number_out_of_range_is_noop() {
        let mut w = make_wizard();
        w.step = Step::Setup;
        w.setup_idx = 1;
        w.handle_key_for_test(KeyCode::Char('5'));
        assert_eq!(w.setup_idx, 1);
        w.handle_key_for_test(KeyCode::Char('0'));
        assert_eq!(w.setup_idx, 1);
    }

    #[test]
    fn confirm_y_advances_to_intro() {
        let mut w = OnboardingWizard::new_with_confirm();
        let outcome = w.handle_key_pure(KeyCode::Char('y'), KeyModifiers::NONE);
        assert_eq!(w.step, Step::Intro);
        assert_eq!(outcome, PureOutcome::ClearAndRedraw);
    }

    #[test]
    fn confirm_capital_y_also_advances() {
        let mut w = OnboardingWizard::new_with_confirm();
        let outcome = w.handle_key_pure(KeyCode::Char('Y'), KeyModifiers::NONE);
        assert_eq!(w.step, Step::Intro);
        assert_eq!(outcome, PureOutcome::ClearAndRedraw);
    }

    #[test]
    fn confirm_n_closes_without_advancing() {
        let mut w = OnboardingWizard::new_with_confirm();
        let outcome = w.handle_key_pure(KeyCode::Char('n'), KeyModifiers::NONE);
        assert_eq!(w.step, Step::Confirm, "n must NOT advance step");
        assert_eq!(outcome, PureOutcome::Close);
    }

    #[test]
    fn intro_enter_outcome_is_clear_and_redraw() {
        let mut w = make_wizard();
        let outcome = w.handle_key_pure(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(w.step, Step::Language);
        assert_eq!(outcome, PureOutcome::ClearAndRedraw);
    }

    #[test]
    fn language_enter_outcome_is_apply_then_advance() {
        let mut w = make_wizard();
        w.step = Step::Language;
        w.language_idx = 2;
        let outcome = w.handle_key_pure(KeyCode::Enter, KeyModifiers::NONE);
        // step stays Language — Modal wrapper performs the apply +
        // advance based on the outcome variant. Pure handler only
        // reports the intent.
        assert_eq!(outcome, PureOutcome::ApplyLanguageThenAdvance);
    }

    #[test]
    fn setup_enter_outcome_is_apply_then_close() {
        let mut w = make_wizard();
        w.step = Step::Setup;
        let outcome = w.handle_key_pure(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(outcome, PureOutcome::ApplySetupThenClose);
    }

    #[test]
    fn esc_at_any_step_closes() {
        for start in [Step::Intro, Step::Language, Step::Setup] {
            let mut w = make_wizard();
            w.step = start;
            let outcome = w.handle_key_pure(KeyCode::Esc, KeyModifiers::NONE);
            assert_eq!(outcome, PureOutcome::Close, "Esc at {start:?} must Close");
        }
    }
}
