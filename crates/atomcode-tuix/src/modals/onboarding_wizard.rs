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

/// ASCII fallback set for the box-drawing glyphs and decorative
/// content chars. Switched on when `caps.unicode_symbols == false`
/// (Windows legacy conhost, `LANG=C`, `TERM=dumb`, etc.) so users on
/// fonts that miss the Unicode glyphs see a tidy ASCII box instead
/// of tofu + drifting borders. The drift is the real bug — `●`, `·`,
/// `←` all return width 1 from `unicode-width` but conhost allocates
/// them slightly wider in practice, so the right `│` lands at a
/// different column on every row that contains one.
fn box_chars(unicode_symbols: bool) -> (&'static str, &'static str, &'static str, &'static str, &'static str, &'static str) {
    if unicode_symbols {
        ("┌", "┐", "└", "┘", "─", "│")
    } else {
        ("+", "+", "+", "+", "-", "|")
    }
}

/// Strip decorative Unicode glyphs out of `s` when running on a
/// terminal that lacks reliable Unicode rendering / cell-width
/// accounting (Windows legacy conhost et al). Each substitution
/// returns an ASCII string of EQUAL display width to what
/// `unicode-width` thought the original was — keeps the right border
/// pinned to the same column on every row.
fn ascii_fallback(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '●' | '•' => out.push('*'),
            '○' => out.push('o'),
            '·' => out.push('-'),
            '←' => out.push('<'),
            '→' => out.push('>'),
            '↑' => out.push('^'),
            '↓' => out.push('v'),
            '█' => out.push('#'),
            // Box-drawing glyphs in content (e.g. tables emitted by
            // markdown into the panel) get the same swap as the
            // outer panel border.
            '┌' | '┐' | '└' | '┘' | '┬' | '┴' | '├' | '┤' | '┼' => out.push('+'),
            '─' => out.push('-'),
            '│' => out.push('|'),
            other => out.push(other),
        }
    }
    out
}

/// Build the lines of a Cyan-bordered panel.
///
/// Returns one string per terminal row: top border with title, content
/// lines with side borders + padding, bottom border with step indicator.
/// `width` is the total external width including both border columns;
/// inner content area is `width - 4` (2 padding cells on each side).
///
/// `unicode_symbols=false` swaps the box-drawing glyphs for `+ - |`
/// and substitutes the decorative chars (`●`, `○`, `·`, `←`, `•`,
/// `█`) inside each content line. Wired from `state.unicode_symbols`
/// so Windows legacy conhost / `LANG=C` / `TERM=dumb` users see a
/// clean ASCII box with the right border still column-aligned.
///
/// The returned strings include SGR colour codes so the renderer paints
/// the borders cyan and the title brand-magenta. Pass these strings to
/// `UiLine::CommandOutput`.
pub(super) fn draw_panel(
    title: &str,
    content: &[String],
    step_indicator: &str,
    width: usize,
    unicode_symbols: bool,
) -> Vec<String> {
    use crossterm::style::{Color, ResetColor, SetForegroundColor};
    let border = Color::Cyan; // Palette::BORDER
    let brand = Color::Magenta; // Palette::BRAND
    let (tl, tr, bl, br_c, h, v) = box_chars(unicode_symbols);

    // Sanitise content lines and the title/indicator strings when
    // ASCII fallback is active. Done once here rather than at every
    // call site so call sites stay readable.
    let title_owned: String;
    let title_seg_src: &str = if unicode_symbols {
        title
    } else {
        title_owned = ascii_fallback(title);
        &title_owned
    };
    let step_owned: String;
    let step_src: &str = if unicode_symbols {
        step_indicator
    } else {
        step_owned = ascii_fallback(step_indicator);
        &step_owned
    };

    let mut out = Vec::with_capacity(content.len() + 2);
    let inner_width = width.saturating_sub(4);

    // Top border: ┌─ <title> ─...─┐
    let title_seg = format!(" {title_seg_src} ");
    let title_width = UnicodeWidthStr::width(title_seg.as_str());
    let dashes_after = inner_width.saturating_sub(title_width);
    let top = format!(
        "{b}{tl}{h}{r}{br}{tt}{r}{b}{dash}{h}{tr}{r}",
        b = SetForegroundColor(border),
        br = SetForegroundColor(brand),
        tl = tl,
        tr = tr,
        h = h,
        tt = title_seg,
        dash = h.repeat(dashes_after),
        r = ResetColor,
    );
    out.push(top);

    // Content rows: │ <2 sp pad> <line padded to inner_width-2> <2 sp pad> │
    for raw in content {
        let owned;
        let line: &str = if unicode_symbols {
            raw.as_str()
        } else {
            owned = ascii_fallback(raw);
            &owned
        };
        let line_width = UnicodeWidthStr::width(line);
        let pad = (inner_width.saturating_sub(2)).saturating_sub(line_width);
        let row = format!(
            "{b}{v}{r}  {line}{pad}  {b}{v}{r}",
            b = SetForegroundColor(border),
            r = ResetColor,
            v = v,
            line = line,
            pad = " ".repeat(pad),
        );
        out.push(row);
    }

    // Bottom border: └─ <step_indicator> ─...─┘
    let step_seg = format!(" {step_src} ");
    let step_w = UnicodeWidthStr::width(step_seg.as_str());
    let dashes_after_step = inner_width.saturating_sub(step_w);
    let bot = format!(
        "{b}{bl}{h}{step_seg}{dash}{h}{br_c}{r}",
        b = SetForegroundColor(border),
        bl = bl,
        br_c = br_c,
        h = h,
        step_seg = step_seg,
        dash = h.repeat(dashes_after_step),
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

/// Footer rows the renderer reserves at the bottom (spinner +
/// top_rule + input + bot_rule + status). Used to compute how much
/// vertical body space is available for centering — the wizard
/// panel must not push into the footer.
const FOOTER_ROWS: usize = 5;

/// Wrap `lines` with top + bottom padding blanks and a left
/// indent so the wizard panel sits at the visual centre of the
/// visible body area. `panel_width` is the horizontal extent of
/// the widest line (typically the bordered panel — 80 cols
/// capped); callers pass this in rather than scanning every line
/// for SGR width because draw_panel-shaped output already commits
/// to a known width.
///
/// RetainedRenderer anchors body content to the body region's BOTTOM:
/// when total pushed rows < body_height, the auto-empty rows appear
/// ABOVE the content, not below. So top-padding alone just stacks
/// redundant blanks against the existing auto-empty strip while the
/// panel stays glued to the footer. Filling the region to exactly
/// body_height with top_blanks + content + bottom_blanks pushes the
/// panel into the actual middle row.
///
/// Each rendered line keeps its own SGR; we prepend bare spaces,
/// which contribute no styling, so colour spans on the original
/// line stay intact.
fn center_lines(
    lines: Vec<String>,
    panel_width: usize,
    term_cols: u16,
    term_rows: u16,
) -> Vec<String> {
    let term_cols = term_cols as usize;
    let term_rows = term_rows as usize;
    let body_rows = term_rows.saturating_sub(FOOTER_ROWS);
    let free = body_rows.saturating_sub(lines.len());
    let top_blanks = free / 2;
    let bottom_blanks = free - top_blanks;
    let left_pad = term_cols.saturating_sub(panel_width) / 2;
    let pad_str = " ".repeat(left_pad);
    let mut out = Vec::with_capacity(lines.len() + free);
    for _ in 0..top_blanks {
        out.push(String::new());
    }
    for line in lines {
        if line.is_empty() || left_pad == 0 {
            out.push(line);
        } else {
            out.push(format!("{pad_str}{line}"));
        }
    }
    for _ in 0..bottom_blanks {
        out.push(String::new());
    }
    out
}

/// Mirror of `/clear`'s "fresh idle view" emission: clear has
/// already been issued by the caller, this pushes the AtomCode
/// banner + cwd + model + slash-hint tips that the renderer's
/// `UiLine::Welcome` handler paints. Used when OnboardingWizard
/// exits to drop the user onto the standard session view (same
/// frame the `/clear` slash command produces) instead of a blank
/// canvas with only the idle prompt. Does NOT emit the
/// `CmdNewSession` "新会话已开始" toast — onboarding-exit is not
/// the same intent as `/session` and the toast would be noise.
pub(crate) fn paint_welcome(
    ctx: &crate::event_loop::LoopCtx,
    renderer: &mut dyn crate::render::Renderer,
) {
    let dir_display = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
    renderer.render(crate::render::UiLine::Welcome {
        model: ctx.model_name.clone(),
        working_dir: dir_display,
    });
    renderer.flush();
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
    /// One-shot CodingPlan fast path entered on first-launch only
    /// (NOT from `/welcome`). Renders a QR for the AtomGit OAuth
    /// short link + the raw URL fallback. Enter → close with
    /// `pending_run_codingplan = true` so the existing `/codingplan`
    /// driver picks up the just-completed login + claim flow.
    /// Esc bails to the welcome banner with no auth changes.
    ///
    /// PR 1a (this commit): user manually presses Enter after
    /// scanning. PR 1b will spawn a polling task that closes the
    /// modal automatically the moment AtomGit reports authorisation.
    QrLogin,
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
    /// Populated by `new_qr_fast_path` after a successful
    /// `start_login()` round-trip. `None` for every other constructor.
    /// Shown verbatim under the QR as the paste-into-browser fallback.
    pub(super) qr_login_url: Option<String>,
    /// Populated by `new_qr_fast_path` when `start_login()` itself
    /// fails (network down, broker 5xx). Surfaced in place of the QR
    /// so the user can read what went wrong instead of staring at a
    /// blank panel; Esc bails, Enter retries by re-running
    /// `start_login()` in `handle_key_pure`'s `Retry` outcome.
    pub(super) qr_login_error: Option<String>,
    /// Live `LoginSession` produced by the most recent `start_login()`
    /// call. The event loop pulls this out via `take_pending_session`
    /// right after constructing the wizard so a background poll
    /// thread (see `event_loop::oauth_poll`) can watch for the user
    /// completing the in-browser consent and auto-close the modal —
    /// no manual Enter required. `None` after a take, after an Esc,
    /// or when `start_login()` itself errored at construction.
    pub(super) pending_session:
        Option<atomcode_core::auth::oauth::LoginSession>,
}

impl OnboardingWizard {
    /// Standard constructor — `/welcome` with empty body. The historic
    /// 3-step (Intro → Language → Setup) flow stays here intact for
    /// `/welcome` re-runs; first-launch onboarding now goes through
    /// [`Self::new_qr_fast_path`] instead.
    pub fn new() -> Self {
        Self {
            step: Step::Intro,
            language_idx: 0,
            setup_idx: 0,
            needs_confirm: false,
            qr_login_url: None,
            qr_login_error: None,
            pending_session: None,
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
            qr_login_url: None,
            qr_login_error: None,
            pending_session: None,
        }
    }

    /// First-launch fast path. Skips the old 3-step Intro / Language /
    /// Setup flow and goes straight to a single QR screen for the
    /// AtomGit OAuth short link — scan, log in, the background poll
    /// thread auto-closes the modal and hands off to `/codingplan`
    /// for the claim. Language defaults to auto-detect from `$LC_ALL`
    /// / `$LANG` (i18n step gone); user can switch later via
    /// `/language`.
    ///
    /// Synchronously calls [`atomcode_core::auth::oauth::start_login`]
    /// up front so the QR is paintable the moment the modal opens.
    /// On network failure the error is stashed on the wizard and
    /// rendered in place of the QR — Esc bails, Enter retries via
    /// `handle_key_pure`'s `RetryQrLogin` outcome.
    ///
    /// The successful `LoginSession` is held on `pending_session` so
    /// the event loop can pull it out (see `take_pending_session`)
    /// and hand it to a background poll thread. The wizard itself
    /// doesn't know about polling — that plumbing stays in the event
    /// loop.
    pub fn new_qr_fast_path() -> Self {
        let (qr_login_url, qr_login_error, pending_session) =
            match atomcode_core::auth::oauth::start_login() {
                Ok(session) => (Some(session.url().to_string()), None, Some(session)),
                Err(e) => (None, Some(format!("{e:#}")), None),
            };
        Self {
            step: Step::QrLogin,
            language_idx: 0,
            setup_idx: 0,
            needs_confirm: false,
            qr_login_url,
            qr_login_error,
            pending_session,
        }
    }

    /// Pull the freshly-constructed `LoginSession` out so the event
    /// loop can spawn a background poll thread against it. Returns
    /// `None` if there is no session to take (Esc was hit, or
    /// `start_login` errored at construction, or another caller
    /// already took it). Called exactly once per QR session by
    /// `event_loop::run_loop`'s first-launch setup; subsequent calls
    /// return None and are harmless.
    pub fn take_pending_session(
        &mut self,
    ) -> Option<atomcode_core::auth::oauth::LoginSession> {
        self.pending_session.take()
    }

    // (set_qr_login_error removed — the event-loop's Failed handler
    // closes the modal instead of injecting state, so this is unused.
    // Re-add if PR 1c lands a Modal trait extension + downcast path
    // that keeps the wizard open on poll failure.)

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
            // Number keys are shortcuts: pick AND commit in one
            // keystroke. The arrow-key path still requires Enter,
            // but typing a digit is unambiguous — the user already
            // expressed intent, no second confirmation needed.
            (Language, KeyCode::Char('1')) => {
                self.language_idx = 0;
                PureOutcome::ApplyLanguageThenAdvance
            }
            (Language, KeyCode::Char('2')) => {
                self.language_idx = 1;
                PureOutcome::ApplyLanguageThenAdvance
            }
            (Language, KeyCode::Char('3')) => {
                self.language_idx = 2;
                PureOutcome::ApplyLanguageThenAdvance
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
                PureOutcome::ApplySetupThenClose
            }
            (Setup, KeyCode::Char('2')) => {
                self.setup_idx = 1;
                PureOutcome::ApplySetupThenClose
            }
            (Setup, KeyCode::Char('3')) => {
                self.setup_idx = 2;
                PureOutcome::ApplySetupThenClose
            }
            (Setup, KeyCode::Enter) => PureOutcome::ApplySetupThenClose,
            (Setup, KeyCode::Left) => {
                self.step = Language;
                PureOutcome::ClearAndRedraw
            }
            (Setup, KeyCode::Esc) => PureOutcome::Close,

            // QrLogin (fast path, first-launch only).
            // - start_login failed → Enter retries, Esc bails.
            // - start_login ok → Enter mirrors the
            //   `session.open_browser_best_effort()` call that
            //   `/codingplan`'s `run_oauth_with_renderer` makes
            //   automatically, so a user who'd rather click than scan
            //   gets a one-key path into the consent page. We
            //   deliberately do NOT re-run start_login here — that's
            //   what the old ApplyQrLoginThenClose did, and it raced
            //   the background poll's auth.toml write, painting a
            //   duplicate QR + URL block into scrollback. The new
            //   outcome only spawns the platform browser command;
            //   failures (xdg-open missing on Linux, no $DISPLAY,
            //   etc.) are silently swallowed and the on-screen QR
            //   + URL stay as fallbacks. The in-flight poll still
            //   auto-closes the modal on its own.
            (QrLogin, KeyCode::Enter) => {
                if self.qr_login_error.is_some() {
                    PureOutcome::RetryQrLogin
                } else if self.qr_login_url.is_some() {
                    PureOutcome::OpenQrUrlInBrowser
                } else {
                    PureOutcome::Noop
                }
            }
            (QrLogin, KeyCode::Esc) => PureOutcome::Close,

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
    pub(super) fn draw_intro_lines(
        &self,
        term_cols: u16,
        term_rows: u16,
        unicode_symbols: bool,
    ) -> Vec<String> {
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
            // 5-line pure block-glyph logo. Uses only `█` and spaces
            // so it renders uniformly in every monospaced font —
            // mixing solid blocks with thin box-drawing chars (the
            // ANSI Shadow style) broke in fonts that draw `█` at
            // 100% cell coverage while keeping `╔═` at line weight,
            // leaving the shadow outline floating disjointly from
            // the letter bodies. Each row is 49 cells; 12-col
            // leading pad centres the 49-wide logo inside
            // draw_panel's 74-col content area (12 + 49 + 13 = 74).
            content.push("             ███  █████  ███  █     █  ████  ███  ████  █████".to_string());
            content.push("            █   █   █   █   █ ██   ██ █     █   █ █   █ █    ".to_string());
            content.push("            █████   █   █   █ █ █ █ █ █     █   █ █   █ ████ ".to_string());
            content.push("            █   █   █   █   █ █  █  █ █     █   █ █   █ █    ".to_string());
            content.push("            █   █   █    ███  █     █  ████  ███  ████  █████".to_string());
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
            unicode_symbols,
        ));
        ascii_fallback_step(out, unicode_symbols)
    }

    /// Build all output lines for step 2 (Language). Bilingual title
    /// is locale-independent (it IS the moment the user picks
    /// locale); the prompt + option labels + nav hint follow the
    /// current global locale.
    pub(super) fn draw_language_lines(&self, term_cols: u16, unicode_symbols: bool) -> Vec<String> {
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
            unicode_symbols,
        ));
        ascii_fallback_step(out, unicode_symbols)
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
    pub(super) fn draw_setup_lines(&self, term_cols: u16, unicode_symbols: bool) -> Vec<String> {
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
            unicode_symbols,
        ));
        ascii_fallback_step(out, unicode_symbols)
    }

    /// QR fast-path (single-page first-launch onboarding).
    ///
    /// Layout (when `start_login` succeeded):
    /// ```text
    /// Step 1/1 · 扫码登录
    /// ┌─ AtomCode ──────────────────────────────────┐
    /// │   扫码登录,自动领取 CodingPlan 免费额度    │
    /// │                                              │
    /// │              <QR block>                      │
    /// │                                              │
    /// │   手机扫码,或在浏览器打开:                   │
    /// │   https://acs.atomgit.com/s/AbC123          │
    /// │                                              │
    /// │   扫码完成后按 Enter 继续                    │
    /// │                                              │
    /// │   Esc 跳过 · /login 重试 · /provider … │
    /// └─ Step 1/1 ─────────────────────────────────┘
    /// ```
    ///
    /// Failure layout (`start_login` errored at construction time):
    /// the QR block is replaced with the error message, and the
    /// instruction line below reads "按 Enter 重试" — handled by
    /// `RetryQrLogin` in `handle_key_pure`.
    ///
    /// ASCII-only terminals (`unicode_symbols == false`): QR is
    /// omitted entirely; URL is shown as the only login affordance
    /// so the user can paste it into a browser on a different machine.
    /// QR glyphs render as `□` tofu on Windows legacy conhost / `LANG=C`
    /// and a tofu QR is silently unscannable — better to show nothing.
    pub(super) fn draw_qr_login_lines(
        &self,
        term_cols: u16,
        unicode_symbols: bool,
    ) -> Vec<String> {
        let panel_width = (term_cols as usize).min(80);
        let inner_width = panel_width.saturating_sub(4);
        // Cells available for content inside the panel's `│ <2sp> ... <2sp> │`
        // padding. Centring is leading-space prefix; draw_panel adds the
        // trailing pad to inner_width-2.
        let cell_w = inner_width.saturating_sub(2);
        let center = |s: &str| -> String {
            let w = UnicodeWidthStr::width(s);
            let pad = cell_w.saturating_sub(w) / 2;
            format!("{}{}", " ".repeat(pad), s)
        };

        let mut content: Vec<String> = Vec::new();
        content.push(String::new());
        content.push(center("微信扫码登录,自动领取 CodingPlan 免费额度"));
        content.push(String::new());

        if let Some(reason) = &self.qr_login_error {
            // start_login failed at construction. Surface the cause
            // so the user knows whether to check network, broker, or
            // their own clock; offer Enter-to-retry below.
            content.push(center("× 无法生成登录链接"));
            content.push(String::new());
            // Error reason may be long; just left-align with indent
            // rather than centre — easier to scan.
            content.push(format!("    {}", reason));
            content.push(String::new());
            content.push(center("按 Enter 重试 · Esc 跳过"));
        } else if let Some(url) = &self.qr_login_url {
            // Render QR block (Unicode mode) or skip it (ASCII).
            if let Some(qr_rows) = super::qr::render_for_terminal(url, unicode_symbols) {
                for row in qr_rows {
                    content.push(center(&row));
                }
                content.push(String::new());
            }
            content.push(center(if unicode_symbols {
                "或在浏览器打开:"
            } else {
                "无法显示二维码 — 请在浏览器打开:"
            }));
            content.push(center(url));
            content.push(center("(按 Enter 自动打开)"));
            content.push(String::new());
            // Polling thread auto-closes the modal the moment AtomGit
            // reports authorisation, so no force-continue Enter is
            // needed. Enter is wired to a best-effort browser launch
            // on the URL above (mirrors what /codingplan does); see
            // handle_key_pure's QrLogin Enter arm for the rationale
            // and the historical duplicate-QR bug that gates it.
            content.push(center("扫码完成后自动跳转"));
        } else {
            // Shouldn't happen — `new_qr_fast_path` always populates
            // exactly one of url / error. Defensive fallback so a
            // broken constructor doesn't paint a blank panel.
            content.push(center("(状态未初始化)"));
        }
        content.push(String::new());
        content.push(center("Esc 跳过 · /login 重试 · /provider 手动配置"));
        content.push(String::new());

        let mut out = Vec::new();
        out.push("扫码登录 · 领取CodingPlan".to_string());
        out.push(String::new());
        // Panel title carries the running atomcode version so users
        // reporting a screenshot tell us the build their bug landed
        // in without having to /status first. CARGO_PKG_VERSION is
        // workspace-bound (e.g. "4.23.2") — matches the convention
        // used by the Step::Intro version line above.
        let panel_title = format!("AtomCode · v{}", env!("CARGO_PKG_VERSION"));
        out.extend(draw_panel(
            &panel_title,
            &content,
            "Step 1/1",
            panel_width,
            unicode_symbols,
        ));
        ascii_fallback_step(out, unicode_symbols)
    }
}

/// Trailing pass over a step's full output (header + box + footer
/// blanks). `draw_panel` already substitutes Unicode inside its
/// boxed rows, but the step header rows pushed BEFORE the box don't
/// go through it — so e.g. "Step 3/3 · Setup" would still carry the
/// middle dot on a Windows-legacy-console session. Running the
/// fallback over the whole vec catches those; it's a no-op on rows
/// the panel already sanitised (none of `+ - | * o < > ^ v #` are in
/// the substitution set, so they pass through unchanged).
fn ascii_fallback_step(lines: Vec<String>, unicode_symbols: bool) -> Vec<String> {
    if unicode_symbols {
        return lines;
    }
    lines.into_iter().map(|l| ascii_fallback(&l)).collect()
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
            PureOutcome::Redraw | PureOutcome::ClearAndRedraw => {
                // Both within-step navigation (1/2/3, ↑/↓) and step
                // transitions need to wipe the previous panel before
                // repainting. The wizard's draw() pushes CommandOutput
                // rows that the retained renderer appends to
                // scrollback — without clear_screen, every keystroke
                // would stack another full panel below the last one.
                // The body context above is already gone by the time
                // any non-Confirm step runs (Confirm→Intro is itself
                // a clear-and-redraw), so reclearing here is free.
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
            PureOutcome::OpenQrUrlInBrowser => {
                // Same call /codingplan's run_oauth_with_renderer
                // makes after rendering the QR. Best-effort: failures
                // (xdg-open missing on a minimal Linux image, no
                // $DISPLAY in an SSH session, etc.) are swallowed so
                // the modal stays put and the user falls back to
                // scanning the QR or copying the URL. No redraw —
                // panel content is unchanged; the browser launch is
                // a pure side effect.
                if let Some(url) = &self.qr_login_url {
                    let _ = atomcode_core::auth::oauth::open_browser(url);
                }
                Ok(ModalAction::Continue)
            }
            PureOutcome::RetryQrLogin => {
                // Re-run start_login() in-place so the user can recover
                // from a transient network blip without restarting
                // atomcode. Mirrors the constructor — synchronous
                // round-trip, store either url or error, AND on success
                // spawn a fresh background poll thread so the new
                // session auto-completes the way the original did.
                match atomcode_core::auth::oauth::start_login() {
                    Ok(session) => {
                        self.qr_login_url = Some(session.url().to_string());
                        self.qr_login_error = None;
                        // session is consumed by `spawn_oauth_poll`;
                        // `pending_session` stays None because the
                        // task owns it now.
                        crate::event_loop::oauth_poll::spawn_oauth_poll(
                            session,
                            Some(std::sync::Arc::clone(&ctx.telemetry)),
                            ctx.oauth_event_tx.clone(),
                            ctx.wake_tx.clone(),
                        );
                    }
                    Err(e) => {
                        self.qr_login_url = None;
                        self.qr_login_error = Some(format!("{e:#}"));
                    }
                }
                renderer.clear_screen();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            PureOutcome::ApplySetupThenClose => {
                match self.setup_idx {
                    0 => ctx.pending_run_login_setup = true,
                    1 => ctx.pending_open_provider_wizard = true,
                    _ => { /* Skip — no flag */ }
                }
                // Setup always runs on a wizard-owned screen
                // (Confirm→Intro and every subsequent transition is
                // a clear-and-redraw). Wipe the panel before
                // returning Close so the next view starts on a clean
                // canvas. For Skip (no follow-up flag) we also
                // render the welcome banner here so the user lands
                // on the regular idle session view (AtomCode banner
                // + cwd + model + tips), not a blank screen with
                // just an input prompt. CodingPlan and Provider
                // takeovers paint their own UI, so we only emit
                // Welcome for the Skip branch.
                renderer.clear_screen();
                if self.setup_idx == 2 {
                    paint_welcome(ctx, renderer);
                }
                Ok(ModalAction::Close)
            }
            PureOutcome::Close => {
                // Esc/N from Confirm preserves the body context —
                // clearing there would wipe the conversation the
                // user just declined to discard. Esc from any other
                // step bails out of onboarding entirely; render the
                // welcome banner so the user sees the standard idle
                // session view instead of a blank canvas.
                if self.step != Step::Confirm {
                    renderer.clear_screen();
                    paint_welcome(ctx, renderer);
                }
                Ok(ModalAction::Close)
            }
        }
    }

    fn draw(
        &self,
        _buf: &crate::event_loop::Buffer,
        state: &crate::state::UiState,
        ctx: &crate::event_loop::LoopCtx,
        renderer: &mut dyn crate::render::Renderer,
    ) {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        // The wizard panel is capped at 80 cols by draw_panel; use that
        // as the centering anchor so the bordered box stays at the
        // canvas middle in wide terminals. Confirm is deliberately
        // left uncentred — it's an inline scrollback message that
        // shares space with the preserved body context.
        let panel_width = (cols as usize).min(80);
        // Mirror of TerminalCaps::unicode_symbols — false on Windows
        // legacy conhost / LANG=C / TERM=dumb. Threaded into the
        // panel + content rendering so those terminals get an ASCII
        // box (`+ - |`) with `●·←` substituted out, which keeps the
        // right border column-aligned (the chief visible Win10 bug).
        let unicode = state.unicode_symbols;
        let lines = match self.step {
            Step::Confirm => {
                // No box for the y/N prompt — one inline line.
                let msg = crate::i18n::t(crate::i18n::Msg::OnboardingConfirmClear).into_owned();
                vec![if unicode { msg } else { ascii_fallback(&msg) }]
            }
            Step::Intro => center_lines(self.draw_intro_lines(cols, rows, unicode), panel_width, cols, rows),
            Step::Language => center_lines(self.draw_language_lines(cols, unicode), panel_width, cols, rows),
            Step::Setup => center_lines(self.draw_setup_lines(cols, unicode), panel_width, cols, rows),
            Step::QrLogin => center_lines(self.draw_qr_login_lines(cols, unicode), panel_width, cols, rows),
        };
        for line in lines {
            // No trailing `\n` — the retained renderer's
            // push_body_text splits on `\n` and treats the empty
            // chunk after a trailing newline as ANOTHER blank row,
            // so `"foo\n"` produced two rows (foo + blank) and the
            // wizard's letterforms ended up with a blank line
            // between every glyph row. Empty strings already in
            // `lines` (top/bottom pad, between-section blanks) emit
            // their own blank rows by themselves.
            renderer.render(crate::render::UiLine::CommandOutput(line));
        }
        // Reset the footer's cached input/menu state. The retained
        // renderer stores `input_buf`/`menu` separately from
        // scrollback; without an explicit InputPrompt re-render here,
        // a user who triggered `/welcome` from the slash menu (typed
        // `/w`, hit Enter on the highlighted `/welcome`) would still
        // see `❯ /w` plus the slash-menu dropdown lingering under the
        // wizard — and Backspace wouldn't budge it because the
        // in-memory buffer was already cleared by the dispatch path.
        // Empty buf + no menu wipes both visuals; the modal owns key
        // input until it closes, so the bare `❯ ` underneath is
        // purely cosmetic.
        renderer.render(crate::render::UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: crate::event_loop::build_status(state, ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
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
    /// QR step's `start_login()` previously errored; user pressed
    /// Enter to retry. Wrapper re-runs `start_login()` and resets
    /// the wizard's `qr_login_url` / `qr_login_error` fields, then
    /// ClearAndRedraw.
    RetryQrLogin,
    /// QR step Enter on the happy path — launch the platform browser
    /// at the already-displayed login URL, mirroring the
    /// `session.open_browser_best_effort()` call `/codingplan` makes
    /// automatically. Failures are silently swallowed (xdg-open
    /// missing, headless Linux, etc.); the QR + URL remain on screen
    /// as fallbacks, so the modal layout doesn't change.
    OpenQrUrlInBrowser,
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
        let lines = draw_panel("AtomCode", &[], "Step 1/3", 60, true);
        let plain = strip_sgr(&lines[0]);
        assert!(plain.starts_with("┌─ AtomCode "));
        assert!(plain.ends_with("─┐"));
    }

    #[test]
    fn bottom_border_has_step_indicator() {
        let lines = draw_panel("AtomCode", &[], "Step 1/3", 60, true);
        let plain = strip_sgr(lines.last().unwrap());
        assert!(plain.starts_with("└─ Step 1/3 "));
        assert!(plain.ends_with("─┘"));
    }

    #[test]
    fn content_lines_are_padded_to_width() {
        let content = vec!["hello".to_string(), "".to_string()];
        let lines = draw_panel("X", &content, "Y", 30, true);
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
        let lines = draw_panel("X", &content, "Y", 30, true);
        let plain = strip_sgr(&lines[1]);
        assert_eq!(UnicodeWidthStr::width(plain.as_str()), 30);
    }

    #[test]
    fn narrow_terminal_does_not_panic() {
        // width=10 has inner_width=6 which won't fit "AtomCode" title;
        // saturating_sub guards against underflow. We just assert it
        // doesn't panic and produces *some* output.
        let lines = draw_panel("AtomCode", &["x".into()], "S", 10, true);
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
        let lines = OnboardingWizard::new().draw_intro_lines(80, 24, true);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        // ASCII logo signature: M's row 3 collapses to alternating
        // `█ █ █ █`, unique to the new pure-block design.
        assert!(
            joined.contains("█ █ █ █"),
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
        let lines = OnboardingWizard::new().draw_intro_lines(80, 18, true);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !joined.contains("█ █ █ █"),
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
        let lines = OnboardingWizard::new().draw_language_lines(80, true);
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
        let lines = w.draw_language_lines(80, true);
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
        atomcode_core::config::Config::default()
    }

    // ── Step 3 (Setup) draw tests ──

    /// Setup panel renders 3 numbered options with localised
    /// CodingPlan / Manual / Skip labels (reusing WelcomeOption* Msg
    /// variants), the SetupTitle, and the nav hint.
    #[test]
    fn setup_layout_has_three_options() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_setup_lines(80, true);
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
        let lines = OnboardingWizard::new().draw_setup_lines(80, true);
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

    /// CodingPlan must come first in the rendered step-3 list, then
    /// Manual, then Skip. Migrated from the deleted welcome_wizard.rs's
    /// `options_put_codingplan_first` test; pins option order so a
    /// reorder needs a deliberate test update.
    #[test]
    fn setup_options_put_codingplan_first() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_setup_lines(80, true);
        let joined: String = lines
            .iter()
            .map(|s| strip_sgr(s))
            .collect::<Vec<_>>()
            .join("\n");
        let pos_codingplan = joined
            .find("Set up CodingPlan")
            .expect("CodingPlan label missing");
        let pos_manual = joined
            .find("Configure manually")
            .expect("manual label missing");
        let pos_skip = joined.find("Skip for now").expect("skip label missing");
        assert!(pos_codingplan < pos_manual);
        assert!(pos_manual < pos_skip);
    }

    /// Filled marker tracks setup_idx.
    #[test]
    fn setup_selected_marker_follows_idx() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let mut w = OnboardingWizard::new();
        w.setup_idx = 1;
        let lines = w.draw_setup_lines(80, true);
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

    // ── VirtualTerminal snapshot tests ──
    //
    // These tests feed the wizard's emitted lines through a real
    // VirtualTerminal — same vt100 path the prod renderer drives — so
    // we catch ANSI/SGR mishandling that the strip_sgr unit tests
    // can't surface. Plan called for a higher-level `new_vterm` /
    // `ctx_for_modal_tests` helper pair, but those don't exist in
    // the current tree; the wizard's `draw_*_lines` already returns
    // standalone strings (it's only the Modal trait wrapper that
    // needs the renderer / ctx), so we can short-circuit straight
    // into the vterm by feeding `lines.join("\n")` as bytes.

    /// Feed wizard lines into a fresh VirtualTerminal and return its
    /// post-paint screen dump. Each line gets a trailing `\r\n` so
    /// the vterm advances to column 0 of the next row — without the
    /// `\r`, lines would stack at whatever column the cursor happened
    /// to be at after the previous line's last SGR.
    fn paint_to_vterm(lines: Vec<String>, w: u16, h: u16) -> String {
        let mut vt = crate::test_term::VirtualTerminal::new(w, h);
        let mut bytes = Vec::new();
        for line in &lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        vt.feed(&bytes);
        vt.dump()
    }

    #[test]
    fn vterm_step1_shows_box_borders_in_en() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_intro_lines(80, 24, true);
        let screen = paint_to_vterm(lines, 80, 24);
        // Top + bottom corner glyphs visible in the painted grid.
        assert!(screen.contains("┌─"), "top border missing: {screen}");
        assert!(screen.contains("└─"), "bottom border missing: {screen}");
        // Brand title, step header, key copy.
        assert!(screen.contains("AtomCode"));
        assert!(screen.contains("Step 1/3 · Welcome"));
        assert!(screen.contains("Press Enter to continue"));
    }

    #[test]
    fn vterm_step2_zh_renders_bilingual_title_and_chinese_options() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::ZhCn);
        let mut w = OnboardingWizard::new();
        w.step = Step::Language;
        let lines = w.draw_language_lines(80, true);
        let screen = paint_to_vterm(lines, 80, 24);
        // vt100 places a CJK double-width char in one cell and leaves
        // the next cell blank, so `选择语言` reads back as
        // `选 择 语 言` in the grid dump.
        assert!(
            screen.contains("Choose your language / 选 择 语 言"),
            "bilingual title missing: {screen}"
        );
        assert!(
            screen.contains("自 动 检 测"),
            "Chinese auto-detect label missing: {screen}"
        );
        // Step header `第 2/3 步 · 语言` — the trailing-blank cell after
        // each CJK glyph collides with the source spaces around `2/3`
        // and `·`, doubling them.
        assert!(
            screen.contains("第  2/3 步  · 语 言"),
            "zh step header missing: {screen}"
        );
    }

    #[test]
    fn vterm_step1_compact_below_22_rows_drops_logo() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_intro_lines(80, 18, true);
        let screen = paint_to_vterm(lines, 80, 18);
        assert!(
            !screen.contains("█ █ █ █"),
            "ASCII logo present in compact mode: {screen}"
        );
        // Compact substitutes `AtomCode vX.Y.Z`.
        assert!(screen.contains("AtomCode v"));
        assert!(screen.contains("Press Enter to continue"));
    }

    /// Step 3 setup options align in the grid: bullet column, [N]
    /// column, label column all stay vertically aligned across the
    /// three rows. Pin via `any_row` since the wizard emits one row
    /// per option.
    #[test]
    fn vterm_step3_setup_options_align_vertically() {
        let _g = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_setup_lines(80, true);

        let mut vt = crate::test_term::VirtualTerminal::new(80, 24);
        let mut bytes = Vec::new();
        for line in &lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        vt.feed(&bytes);

        // Each option's row starts with `│  ●  [1]` or `│  ○  [N]`.
        // The bullet must sit at the same column across all three.
        let rows_with_bracket: Vec<String> = (0..24)
            .map(|r| vt.row_text(r))
            .filter(|r| r.contains("[1]") || r.contains("[2]") || r.contains("[3]"))
            .collect();
        assert_eq!(rows_with_bracket.len(), 3, "expected 3 option rows, got {rows_with_bracket:?}");
        // Bullet position (●/○) — all three rows must place it at
        // the same column index. Locate via find().
        let bullet_cols: Vec<Option<usize>> = rows_with_bracket
            .iter()
            .map(|r| r.find('●').or_else(|| r.find('○')))
            .collect();
        assert!(
            bullet_cols.iter().all(|c| c.is_some() && *c == bullet_cols[0]),
            "bullet column drift across rows: {bullet_cols:?}"
        );
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
        let lines = OnboardingWizard::new().draw_intro_lines(80, 24, true);
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

    // ── ASCII fallback (Windows legacy conhost / LANG=C / TERM=dumb) ──

    /// `unicode_symbols=false` swaps the box-drawing glyphs and the
    /// decorative content chars for ASCII equivalents. Regression
    /// guard for the Windows 10 cmd report where the right `│`
    /// landed at a different column on every row that contained
    /// `●` / `·` / `←`, because `unicode-width` reports them as 1
    /// cell while conhost allocates a slightly wider glyph.
    #[test]
    fn draw_panel_ascii_fallback_uses_plus_dash_pipe() {
        let lines = draw_panel(
            "AtomCode",
            &["row one".into(), "row two".into()],
            "Step 3/3",
            30,
            false,
        );
        let joined: String = lines.iter().map(|l| strip_sgr(l)).collect::<Vec<_>>().join("\n");
        // Box-drawing glyphs gone, ASCII fallbacks in their place.
        assert!(!joined.contains('┌'), "U+250C leaked: {:?}", joined);
        assert!(!joined.contains('┐'), "U+2510 leaked: {:?}", joined);
        assert!(!joined.contains('└'), "U+2514 leaked: {:?}", joined);
        assert!(!joined.contains('┘'), "U+2518 leaked: {:?}", joined);
        assert!(!joined.contains('─'), "U+2500 leaked: {:?}", joined);
        assert!(!joined.contains('│'), "U+2502 leaked: {:?}", joined);
        assert!(joined.contains('+'), "no + corner: {:?}", joined);
        assert!(joined.contains('-'), "no - horizontal: {:?}", joined);
        assert!(joined.contains('|'), "no | vertical: {:?}", joined);
    }

    /// `●`, `○`, `·`, `←`, `•` inside content rows must be substituted
    /// with width-equivalent ASCII so the right border stays
    /// column-aligned. We can't easily verify column alignment in a
    /// unit test (no real terminal), but we CAN assert the
    /// substitution happened.
    #[test]
    fn draw_panel_ascii_fallback_substitutes_decorative_chars_in_content() {
        let content = vec!["● filled".into(), "○ open · mid · ← back • bullet".into()];
        let lines = draw_panel("X", &content, "Y", 60, false);
        let joined: String = lines.iter().map(|l| strip_sgr(l)).collect::<Vec<_>>().join("\n");
        for bad in ['●', '○', '·', '←', '•'] {
            assert!(
                !joined.contains(bad),
                "Unicode {:?} leaked through ASCII fallback: {:?}",
                bad,
                joined
            );
        }
        assert!(joined.contains('*'), "● not replaced with *: {:?}", joined);
        assert!(joined.contains('o'), "○ not replaced with o: {:?}", joined);
        assert!(joined.contains('<'), "← not replaced with <: {:?}", joined);
    }

    /// On Windows-legacy-console paths, `state.unicode_symbols == false`
    /// flows through `draw_setup_lines` → `draw_panel`, so the full
    /// step 3 render must come out ASCII-only.
    #[test]
    fn draw_setup_lines_ascii_fallback_produces_pure_ascii_box() {
        let _g = atomcode_core::i18n::test_lock();
        atomcode_core::i18n::set_locale(atomcode_core::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_setup_lines(80, false);
        let joined: String = lines.iter().map(|l| strip_sgr(l)).collect::<Vec<_>>().join("\n");
        for bad in ['┌', '┐', '└', '┘', '─', '│', '●', '○', '·', '←', '•'] {
            assert!(
                !joined.contains(bad),
                "Unicode {:?} leaked through Setup ASCII fallback: {:?}",
                bad,
                joined
            );
        }
        // Visible content is still readable in ASCII.
        assert!(joined.contains("Set up CodingPlan"));
        assert!(joined.contains("[1]") && joined.contains("[2]") && joined.contains("[3]"));
    }

    /// Belt + braces: each row of an ASCII-fallback rendered Setup
    /// panel must end with `|` at the SAME column. This is the
    /// property that was visibly broken on Windows 10 cmd (right
    /// border zig-zagging).
    #[test]
    fn draw_panel_ascii_fallback_right_border_column_aligned() {
        let _g = atomcode_core::i18n::test_lock();
        atomcode_core::i18n::set_locale(atomcode_core::i18n::Locale::En);
        let lines = OnboardingWizard::new().draw_setup_lines(80, false);
        // Drop the step header row that sits ABOVE the panel — it's
        // not bordered.
        let bordered: Vec<String> = lines
            .iter()
            .map(|l| strip_sgr(l))
            .filter(|l| l.contains('|') || l.contains('+'))
            .collect();
        assert!(bordered.len() >= 3, "expected top + content + bottom: {:?}", bordered);
        let widths: std::collections::HashSet<usize> = bordered
            .iter()
            .map(|l| UnicodeWidthStr::width(l.as_str()))
            .collect();
        assert_eq!(
            widths.len(),
            1,
            "panel rows have different visible widths — right border would zig-zag: {:?}",
            bordered
        );
    }

    // ── QrLogin (PR 1a) state-machine + draw tests ──────────────────
    //
    // start_login() is a real network call so these tests can't drive
    // it end-to-end. Instead we manually construct an OnboardingWizard
    // pinned to Step::QrLogin with synthetic url / error state, then
    // exercise the keystroke transitions + draw output.

    fn qr_wizard_with_url(url: &str) -> OnboardingWizard {
        OnboardingWizard {
            step: Step::QrLogin,
            language_idx: 0,
            setup_idx: 0,
            needs_confirm: false,
            qr_login_url: Some(url.to_string()),
            qr_login_error: None,
            pending_session: None,
        }
    }

    fn qr_wizard_with_error(msg: &str) -> OnboardingWizard {
        OnboardingWizard {
            step: Step::QrLogin,
            language_idx: 0,
            setup_idx: 0,
            needs_confirm: false,
            qr_login_url: None,
            qr_login_error: Some(msg.to_string()),
            pending_session: None,
        }
    }

    #[test]
    fn qr_login_enter_when_url_present_opens_browser() {
        // Enter on the happy QR path now mirrors /codingplan's
        // `session.open_browser_best_effort()` — same platform
        // browser launch the CLI flow makes automatically, just
        // user-triggered. The historical Noop was a fix for a
        // duplicate-QR bug caused by re-running start_login on
        // Enter (ApplyQrLoginThenClose); the new outcome doesn't
        // touch start_login at all, so that bug stays gone.
        let mut w = qr_wizard_with_url("https://acs.atomgit.com/s/AbC123");
        let outcome = w.handle_key_pure(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(outcome, PureOutcome::OpenQrUrlInBrowser);
    }

    #[test]
    fn qr_login_enter_with_neither_url_nor_error_is_noop() {
        // Defensive: if construction landed in a state where neither
        // the URL nor the error is populated (shouldn't happen — the
        // constructor always produces exactly one), Enter must NOT
        // dispatch OpenQrUrlInBrowser (would try to open None) and
        // must NOT dispatch RetryQrLogin (no error to surface). Noop
        // keeps the modal inert until Esc.
        let mut w = qr_wizard_with_url("https://acs.atomgit.com/s/AbC123");
        w.qr_login_url = None;
        w.qr_login_error = None;
        let outcome = w.handle_key_pure(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(outcome, PureOutcome::Noop);
    }

    #[test]
    fn qr_login_enter_when_in_error_state_retries() {
        // start_login failed at construction. Enter re-runs it —
        // wrapper handles ctx mutations, the pure outcome just signals
        // intent.
        let mut w = qr_wizard_with_error("transport: connection refused");
        let outcome = w.handle_key_pure(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(outcome, PureOutcome::RetryQrLogin);
    }

    #[test]
    fn qr_login_esc_closes_without_codingplan_flag() {
        // Esc bails to the welcome banner — no pending_run_codingplan,
        // no setup_idx mutation. Pin against accidental future drift
        // into the Setup-step's ApplySetupThenClose flag-setting path.
        let mut w = qr_wizard_with_url("https://acs.atomgit.com/s/AbC123");
        let outcome = w.handle_key_pure(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(outcome, PureOutcome::Close);

        let mut w = qr_wizard_with_error("any");
        let outcome = w.handle_key_pure(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(outcome, PureOutcome::Close);
    }

    #[test]
    fn qr_login_random_keys_are_noop() {
        // Arrow keys / 1-3 / letters do nothing on QrLogin — pin so a
        // future copy-paste from the Setup-step arms doesn't acquire
        // unintended menu-navigation semantics on this single-page
        // screen.
        let mut w = qr_wizard_with_url("https://acs.atomgit.com/s/AbC123");
        for code in [KeyCode::Up, KeyCode::Down, KeyCode::Left,
                     KeyCode::Char('1'), KeyCode::Char('a')] {
            assert_eq!(
                w.handle_key_pure(code, KeyModifiers::NONE),
                PureOutcome::Noop,
                "{:?} should be Noop on QrLogin",
                code
            );
        }
    }

    #[test]
    fn qr_login_draw_with_url_includes_url_in_output() {
        let w = qr_wizard_with_url("https://acs.atomgit.com/s/AbC123");
        let lines = w.draw_qr_login_lines(80, true);
        let blob = lines.join("\n");
        assert!(
            blob.contains("https://acs.atomgit.com/s/AbC123"),
            "URL must be in render output as fallback for users who can't \
             scan: {:?}",
            blob
        );
        assert!(
            blob.contains("扫码登录"),
            "expected Chinese onboarding header text"
        );
    }

    #[test]
    fn qr_login_draw_with_error_surfaces_reason() {
        let w = qr_wizard_with_error("transport: timeout after 10s");
        let lines = w.draw_qr_login_lines(80, true);
        let blob = lines.join("\n");
        assert!(blob.contains("无法生成登录链接"));
        assert!(blob.contains("transport: timeout after 10s"));
        assert!(blob.contains("按 Enter 重试"));
    }

    #[test]
    fn qr_login_draw_surfaces_enter_to_open_hint() {
        // Users on a desktop terminal can press Enter to launch the
        // browser at the displayed URL — the modal must SHOW that
        // affordance, otherwise nobody knows it exists. Both Unicode
        // and ASCII renderings carry the hint since the action is
        // available in either layout.
        let w = qr_wizard_with_url("https://acs.atomgit.com/s/AbC123");
        let unicode_blob = w.draw_qr_login_lines(80, true).join("\n");
        assert!(
            unicode_blob.contains("Enter"),
            "Unicode QR step missing Enter-to-open affordance:\n{}",
            unicode_blob
        );
        let ascii_blob = w.draw_qr_login_lines(80, false).join("\n");
        assert!(
            ascii_blob.contains("Enter"),
            "ASCII QR step missing Enter-to-open affordance:\n{}",
            ascii_blob
        );
    }

    #[test]
    fn qr_login_draw_ascii_fallback_drops_qr_keeps_url() {
        // Half-block glyphs render as tofu on ASCII-only terminals,
        // so the QR is dropped entirely and we tell the user to use
        // the URL instead. URL itself MUST stay — otherwise the
        // screen has nothing actionable.
        let w = qr_wizard_with_url("https://acs.atomgit.com/s/AbC123");
        let lines = w.draw_qr_login_lines(80, false);
        let blob = lines.join("\n");
        assert!(blob.contains("https://acs.atomgit.com/s/AbC123"));
        assert!(blob.contains("无法显示二维码"));
        // Half-block glyphs must NOT leak through the ASCII fallback.
        assert!(!blob.contains('▀'));
        assert!(!blob.contains('▄'));
        assert!(!blob.contains('█'));
    }
}
