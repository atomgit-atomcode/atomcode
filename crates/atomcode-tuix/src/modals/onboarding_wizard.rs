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

/// Right-pad `s` with spaces until its visible width reaches
/// `target`. Returns the input unchanged if it's already that wide
/// or wider. Used to align option-label columns in Setup step so
/// hints sit at the same x-position across all 3 rows.
fn pad_to_width(s: &str, target: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w >= target {
        return s.to_string();
    }
    format!("{s}{}", " ".repeat(target - w))
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
    /// body. Read by Task 8's slash command path to decide cleanup
    /// behaviour after Close (whether to skip the post-modal idle
    /// redraw to avoid double-painting).
    #[allow(dead_code)] // consumed in Task 8 (/welcome slash command)
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

impl OnboardingWizard {
    /// Build all output lines for step 1 (Intro). `term_cols` /
    /// `term_rows` are taken from `crossterm::terminal::size()` at the
    /// caller; passed in so tests don't need a real terminal.
    /// Returns SGR-laced strings ready for `UiLine::CommandOutput`.
    ///
    /// `term_rows < 22` triggers the compact fallback — drops the
    /// 5-line ASCII logo + Ctrl+C hint so the box fits 18-row
    /// terminals. Spec threshold: full layout needs 18 rows (16 box +
    /// 2 header); compact needs 13 (11 box + 2 header).
    pub(super) fn draw_intro_lines(&self, term_cols: u16, term_rows: u16) -> Vec<String> {
        use crate::i18n::{t, Msg};
        let compact = term_rows < 22;

        // Step header (above box)
        let mut out = Vec::new();
        out.push(t(Msg::OnboardingStepHeaderWelcome).into_owned());
        out.push(String::new()); // blank line between header and box

        // Build content lines
        let mut content: Vec<String> = Vec::new();
        content.push(String::new()); // top padding

        if !compact {
            // 5-line ASCII logo. Leading 3-space pad keeps it
            // visually grouped inside the panel; draw_panel handles
            // right-side width padding.
            content.push(r#"      _   _                  ____          _"#.to_string());
            content.push(r#"     / \ | |_ ___  _ __ ___ / ___|___   __| | ___"#.to_string());
            content.push(r#"    / _ \| __/ _ \| '_ ` _ \ |   / _ \ / _` |/ _ \"#.to_string());
            content.push(r#"   / ___ \ || (_) | | | | | | |__| (_) | (_| |  __/"#.to_string());
            content.push(r#"  /_/   \_\__\___/|_| |_| |_|\____\___/ \__,_|\___|"#.to_string());
            content.push(String::new());
            content.push(
                t(Msg::OnboardingIntroVersionLine {
                    v: env!("CARGO_PKG_VERSION"),
                })
                .into_owned(),
            );
            content.push(String::new());
            content.push(t(Msg::OnboardingIntroBullet1).into_owned());
            content.push(t(Msg::OnboardingIntroBullet2).into_owned());
            content.push(t(Msg::OnboardingIntroBullet3).into_owned());
            content.push(String::new());
            content.push(t(Msg::OnboardingIntroPressEnter).into_owned());
            content.push(t(Msg::OnboardingIntroCtrlC).into_owned());
        } else {
            // Compact: no logo + no Ctrl+C hint. Just product line +
            // tagline + bullets + press-enter.
            content.push(format!("AtomCode v{}", env!("CARGO_PKG_VERSION")));
            content.push(t(Msg::OnboardingIntroCompactTagline).into_owned());
            content.push(String::new());
            content.push(t(Msg::OnboardingIntroBullet1).into_owned());
            content.push(t(Msg::OnboardingIntroBullet2).into_owned());
            content.push(t(Msg::OnboardingIntroBullet3).into_owned());
            content.push(String::new());
            content.push(t(Msg::OnboardingIntroPressEnter).into_owned());
        }
        content.push(String::new()); // bottom padding

        out.extend(draw_panel(
            &t(Msg::OnboardingPanelTitle),
            &content,
            "Step 1/3",
            (term_cols as usize).min(80),
        ));
        out
    }

    /// Build all output lines for step 2 (Language). Bilingual title
    /// is locale-independent (it IS the moment the user picks
    /// locale); the prompt + option labels + nav hint follow the
    /// current global locale.
    pub(super) fn draw_language_lines(&self, term_cols: u16) -> Vec<String> {
        use crate::i18n::{t, Msg};

        let mut out = Vec::new();
        out.push(t(Msg::OnboardingStepHeaderLanguage).into_owned());
        out.push(String::new());

        let options = [
            t(Msg::OnboardingLanguageOptionAuto).into_owned(),
            t(Msg::OnboardingLanguageOptionEn).into_owned(),
            t(Msg::OnboardingLanguageOptionZhCn).into_owned(),
        ];

        let mut content: Vec<String> = Vec::new();
        content.push(String::new());
        content.push(t(Msg::OnboardingLanguageTitleBilingual).into_owned());
        content.push(String::new());
        content.push(t(Msg::OnboardingLanguagePrompt).into_owned());
        content.push(String::new());
        for (i, label) in options.iter().enumerate() {
            let bullet = if i == self.language_idx { '●' } else { '○' };
            content.push(format!("{bullet}  [{}] {}", i + 1, label));
        }
        content.push(String::new());
        content.push(t(Msg::OnboardingNavHint).into_owned());
        content.push(String::new());

        out.extend(draw_panel(
            &t(Msg::OnboardingPanelTitle),
            &content,
            "Step 2/3",
            (term_cols as usize).min(80),
        ));
        out
    }

    /// Apply the user's language choice — called when Enter pressed
    /// in step 2. Mutates `config.language`, flips the global locale,
    /// and persists the config to disk. Returns the locale that was
    /// applied so the caller can also surface a confirmation message.
    ///
    /// Auto-detect (`language_idx == 0`) clears `config.language` so
    /// the resolver re-derives from env on next launch; the running
    /// session also re-resolves immediately so the next redraw uses
    /// the env-detected locale.
    pub(super) fn apply_language(
        &self,
        config: &mut atomcode_core::config::Config,
    ) -> anyhow::Result<atomcode_core::locale::Locale> {
        use atomcode_core::locale::Locale;
        let new_locale = match self.language_idx {
            0 => {
                // Auto-detect: clear config field, re-resolve from env.
                config.language = None;
                crate::i18n::resolve_initial_locale(None, None)
            }
            1 => {
                config.language = Some(Locale::En);
                Locale::En
            }
            2 => {
                config.language = Some(Locale::ZhCn);
                Locale::ZhCn
            }
            _ => unreachable!("language_idx is bounded 0..=2"),
        };
        crate::i18n::set_locale(new_locale);
        config.save(&atomcode_core::config::Config::default_path())?;
        Ok(new_locale)
    }

    /// Build all output lines for step 3 (Setup). Reuses the existing
    /// `WelcomeOption*` Msg variants from the old wizard so the
    /// already-translated CodingPlan / Manual / Skip labels stay
    /// consistent. Labels are right-padded to 22 visible cols so the
    /// hint column lines up across rows even when one label is
    /// English ("Configure manually") and another is Chinese
    /// ("配置 CodingPlan") that takes fewer chars but more grid cells.
    pub(super) fn draw_setup_lines(&self, term_cols: u16) -> Vec<String> {
        use crate::i18n::{t, Msg};

        let mut out = Vec::new();
        out.push(t(Msg::OnboardingStepHeaderSetup).into_owned());
        out.push(String::new());

        let options = [
            (
                t(Msg::WelcomeOptionCodingPlan).into_owned(),
                t(Msg::WelcomeOptionCodingPlanHint).into_owned(),
            ),
            (
                t(Msg::WelcomeOptionConfigureManually).into_owned(),
                t(Msg::WelcomeOptionConfigureManuallyHint).into_owned(),
            ),
            (
                t(Msg::WelcomeOptionSkip).into_owned(),
                t(Msg::WelcomeOptionSkipHint).into_owned(),
            ),
        ];

        let mut content: Vec<String> = Vec::new();
        content.push(String::new());
        content.push(t(Msg::OnboardingSetupTitle).into_owned());
        content.push(String::new());
        for (i, (label, hint)) in options.iter().enumerate() {
            let bullet = if i == self.setup_idx { '●' } else { '○' };
            let label_padded = pad_to_width(label, 22);
            content.push(format!("{bullet}  [{}] {} {}", i + 1, label_padded, hint));
        }
        content.push(String::new());
        content.push(t(Msg::OnboardingNavHint).into_owned());
        content.push(String::new());

        out.extend(draw_panel(
            &t(Msg::OnboardingPanelTitle),
            &content,
            "Step 3/3",
            (term_cols as usize).min(80),
        ));
        out
    }
}

impl Default for OnboardingWizard {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::modals::Modal for OnboardingWizard {
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut crate::event_loop::Buffer,
        state: &mut crate::state::UiState,
        ctx: &mut crate::event_loop::LoopCtx,
        renderer: &mut dyn crate::render::Renderer,
    ) -> anyhow::Result<crate::modals::ModalAction> {
        use crate::modals::ModalAction;
        let outcome = self.handle_key_pure(code, mods);
        match outcome {
            PureOutcome::Noop => Ok(ModalAction::Continue),
            PureOutcome::Redraw => {
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            PureOutcome::ClearAndRedraw => {
                renderer.clear_screen();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            PureOutcome::ApplyLanguageThenAdvance => {
                if let Err(e) = self.apply_language(&mut ctx.config) {
                    let msg = crate::i18n::t(crate::i18n::Msg::ConfigSaveFailed {
                        error: &e.to_string(),
                    });
                    renderer.render(crate::render::UiLine::CommandOutput(
                        format!("{}\n", msg),
                    ));
                }
                self.step = Step::Setup;
                renderer.clear_screen();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            PureOutcome::ApplySetupThenClose => {
                match self.setup_idx {
                    0 => ctx.pending_run_codingplan = true,
                    1 => ctx.pending_open_provider_wizard = true,
                    _ => { /* Skip — no flag */ }
                }
                Ok(ModalAction::Close)
            }
            PureOutcome::Close => Ok(ModalAction::Close),
        }
    }

    fn draw(
        &self,
        _buf: &crate::event_loop::Buffer,
        _state: &crate::state::UiState,
        _ctx: &crate::event_loop::LoopCtx,
        renderer: &mut dyn crate::render::Renderer,
    ) {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let lines = match self.step {
            Step::Confirm => {
                // No box for the y/N prompt — one inline line.
                vec![crate::i18n::t(crate::i18n::Msg::OnboardingConfirmClear).into_owned()]
            }
            Step::Intro => self.draw_intro_lines(cols, rows),
            Step::Language => self.draw_language_lines(cols),
            Step::Setup => self.draw_setup_lines(cols),
        };
        for line in lines {
            renderer.render(crate::render::UiLine::CommandOutput(format!("{line}\n")));
        }
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

    // ── Step 1 (Intro) draw tests ──

    /// Full-height layout assertions: ASCII logo + version + all
    /// three bullets + press-enter + ctrl-c lines all present.
    #[test]
    fn intro_full_layout_has_all_pieces() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_intro_lines(80, 24);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        // ASCII logo signature (the last row of the 5-line glyph
        // block is unique enough to pin).
        assert!(
            joined.contains("/_/   \\_\\__\\___/"),
            "logo missing: {joined}"
        );
        assert!(joined.contains("Version "));
        assert!(joined.contains("Multi-step agent loop"));
        assert!(joined.contains("Connects to any OpenAI"));
        assert!(joined.contains("Free tokens via CodingPlan"));
        assert!(joined.contains("Press Enter to continue"));
        assert!(joined.contains("Ctrl+C exits"));
        // Header above the box.
        assert!(joined.contains("Step 1/3 · Welcome"));
        // Box step indicator at bottom.
        assert!(joined.contains("Step 1/3"));
    }

    /// `term_rows < 22` drops the logo + Ctrl+C lines. Bullets,
    /// version, and Press-Enter still render so the user can advance.
    #[test]
    fn intro_compact_drops_logo() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_intro_lines(80, 18);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains("/_/   \\_\\__\\___/"),
            "logo should be hidden in compact mode: {joined}"
        );
        // Compact replaces the version block with a compact product
        // line `AtomCode vX.Y.Z` + tagline.
        assert!(joined.contains("AtomCode v"));
        assert!(joined.contains("AI coding agent that lives in your terminal"));
        assert!(joined.contains("Free tokens"));
        assert!(joined.contains("Press Enter to continue"));
    }

    // ── Step 2 (Language) draw + apply tests ──

    /// Bilingual title + 3 numbered options + nav hint all present in
    /// the rendered output.
    #[test]
    fn language_layout_has_three_options_with_numbers() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_language_lines(80);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        // Bilingual title (locale-independent).
        assert!(joined.contains("Choose your language / 选择语言"));
        // Three numbered options.
        assert!(joined.contains("[1] Auto-detect"));
        assert!(joined.contains("[2] English"));
        assert!(joined.contains("[3] 简体中文"));
        // Step header + indicator.
        assert!(joined.contains("Step 2/3 · Language"));
        // Nav hint.
        assert!(joined.contains("1-3 select"));
    }

    /// Selected marker `●` sits on the row matching language_idx;
    /// the other rows get the hollow `○` marker.
    #[test]
    fn language_selected_marker_follows_idx() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let mut w = OnboardingWizard::new();
        w.step = Step::Language;
        w.language_idx = 2;
        let lines = w.draw_language_lines(80);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        // `●  [3] 简体中文` selected; `○  [2] English` unselected.
        let pos_filled = joined.find("●  [3]").expect("filled marker missing");
        let pos_hollow = joined.find("○  [2]").expect("hollow marker missing");
        assert!(
            pos_hollow < pos_filled,
            "expected hollow before filled marker"
        );
    }

    /// apply_language writes the picked locale into config + flips
    /// the global locale + persists to disk under an ATOMCODE_HOME
    /// override so tests don't touch real `~/.atomcode`.
    #[test]
    fn apply_language_writes_config_and_sets_locale() {
        use atomcode_core::locale::Locale;
        let _g = crate::i18n::test_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        // ATOMCODE_HOME drives Config::config_dir() ahead of $HOME, so
        // the test's config.save lands in `<tmp>/config.toml` and not
        // the real home dir. Saved+restored around the test to keep
        // parallel tests from racing on the global env.
        let prev_atomcode_home = std::env::var("ATOMCODE_HOME").ok();
        std::env::set_var("ATOMCODE_HOME", tmp.path());

        let mut cfg = blank_config_for_test();
        let mut w = OnboardingWizard::new();
        w.language_idx = 2;
        let applied = w.apply_language(&mut cfg).unwrap();
        assert_eq!(applied, Locale::ZhCn);
        assert_eq!(cfg.language, Some(Locale::ZhCn));
        assert_eq!(crate::i18n::current_locale(), Locale::ZhCn);
        // File must actually exist on disk.
        assert!(tmp.path().join("config.toml").exists());

        // Restore env.
        match prev_atomcode_home {
            Some(v) => std::env::set_var("ATOMCODE_HOME", v),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
    }

    /// Auto-detect (idx 0) blanks `config.language` so the next-launch
    /// resolver re-derives from env. Even when the prior config carried
    /// an explicit choice.
    #[test]
    fn apply_language_auto_clears_config_field() {
        use atomcode_core::locale::Locale;
        let _g = crate::i18n::test_lock();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev = std::env::var("ATOMCODE_HOME").ok();
        std::env::set_var("ATOMCODE_HOME", tmp.path());

        let mut cfg = blank_config_for_test();
        cfg.language = Some(Locale::En); // start with non-None
        let mut w = OnboardingWizard::new();
        w.language_idx = 0;
        w.apply_language(&mut cfg).unwrap();
        assert_eq!(cfg.language, None);

        match prev {
            Some(v) => std::env::set_var("ATOMCODE_HOME", v),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
    }

    /// Minimal Config used by the apply_language tests. Config has no
    /// Default impl (every field is intentionally required so adding
    /// a new field forces every test to update), so we mirror the
    /// blank_config_with_lsp helper from `core::config::tests` here.
    fn blank_config_for_test() -> atomcode_core::config::Config {
        atomcode_core::config::Config {
            default_provider: String::new(),
            default_workdir: None,
            providers: Default::default(),
            datalog: Default::default(),
            auto_update: true,
            notifications: Default::default(),
            telemetry: Default::default(),
            lsp: Default::default(),
            auto_commit: false,
            subagent: Default::default(),
            vision_preprocessor_provider: None,
            language: None,
        }
    }

    // ── Step 3 (Setup) draw tests ──

    /// Setup panel renders 3 numbered options with localised
    /// CodingPlan / Manual / Skip labels (reusing WelcomeOption* Msg
    /// variants), the SetupTitle, and the nav hint.
    #[test]
    fn setup_layout_has_three_options() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_setup_lines(80);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("Step 3/3 · Setup"));
        assert!(joined.contains("How would you like to set up?"));
        assert!(joined.contains("[1] Set up CodingPlan"));
        assert!(joined.contains("[2] Configure manually"));
        assert!(joined.contains("[3] Skip for now"));
        // Hints sit after each option.
        assert!(joined.contains("Free tokens"));
        assert!(joined.contains("API key"));
        // Nav hint.
        assert!(joined.contains("1-3 select"));
    }

    /// ZhCn locale flips every label + hint to the Chinese strings
    /// that the i18n shipped originally for WelcomeOption*.
    #[test]
    fn setup_zh_renders_chinese_labels() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::ZhCn);
        let lines = OnboardingWizard::new().draw_setup_lines(80);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("第 3/3 步 · 配置"));
        assert!(joined.contains("配置 CodingPlan"));
        assert!(joined.contains("手动配置"));
        assert!(joined.contains("暂时跳过"));
    }

    /// Filled marker tracks setup_idx.
    #[test]
    fn setup_selected_marker_follows_idx() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let mut w = OnboardingWizard::new();
        w.setup_idx = 1;
        let lines = w.draw_setup_lines(80);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        // Selected: idx 1 → ●  [2]; others get ○.
        assert!(joined.contains("●  [2]"));
        assert!(joined.contains("○  [1]"));
        assert!(joined.contains("○  [3]"));
    }

    /// pad_to_width: short strings get right-padded to target; long
    /// strings pass through unchanged.
    #[test]
    fn pad_to_width_handles_cjk_and_short_strings() {
        assert_eq!(pad_to_width("hi", 6), "hi    ");
        // CJK char = 2 cols, so "中文" is 4 cols + 2 pad = "中文  ".
        assert_eq!(pad_to_width("中文", 6), "中文  ");
        // Already wider — returned as-is, no truncation.
        assert_eq!(pad_to_width("hello world", 5), "hello world");
    }

    /// Locale-driven copy lookup — boot in ZhCn, every string in the
    /// intro panel should be the Chinese translation.
    #[test]
    fn intro_renders_in_zh_cn() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::ZhCn);
        let lines = OnboardingWizard::new().draw_intro_lines(80, 24);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("第 1/3 步 · 欢迎"));
        assert!(joined.contains("版本 "));
        assert!(joined.contains("按 Enter 继续"));
        assert!(joined.contains("Ctrl+C 可随时退出"));
        // Brand title stays English on purpose.
        assert!(joined.contains("AtomCode"));
    }
}
