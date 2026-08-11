// crates/atomcode-tuix/src/lib.rs

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary
// runs, so the crate's own unit tests never persist commands/plugins/config into
// the developer's real `~/.atomcode`. Tests that set their own ATOMCODE_HOME still
// win (isolate_home is a no-op when the var is already set).
#[cfg(test)]
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

pub mod commands;
pub mod custom_commands;
pub mod event_loop;
pub mod git;
pub(crate) mod git_diff;
pub mod glyph;
pub mod highlight;
pub mod i18n;
pub mod input;
pub mod markdown;
pub mod modals;
pub mod platform;
pub mod render;
pub mod sanitize;
pub mod session;
#[cfg(unix)]
mod signal_restore;
pub mod state;
pub mod team;
pub mod terminal;
pub mod terminal_bg;
#[cfg(test)]
pub mod test_term;
pub mod think;
pub mod title;
pub mod trace;
pub mod version_check;
pub mod width;

use anyhow::Result;
use atomcode_config::config::Config;
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};
use std::io;
use tokio::sync::mpsc;

use crate::commands::CommandRegistry;
pub use crate::event_loop::bg_runtime::RuntimeEventPayload;
use crate::event_loop::{run_loop, LoopCtx};
pub use crate::event_loop::{
    RuntimeControl, RuntimeEndpoint, RuntimeSpawnOverride, SpawnedRuntime,
};
use crate::input::history::History;
use crate::input::reader;
use crate::render::{
    plain::PlainRenderer, retained::RetainedRenderer, worker::TaskRenderer, Renderer,
};
use crate::terminal::TerminalCaps;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderSelectionMode {
    /// Follow changes to the shared default provider in `config.toml`.
    /// Callers must opt into this explicitly; normal interactive sessions are
    /// pinned once opened.
    FollowGlobalDefault,
    /// Keep this runtime's provider selection. This includes ordinary opened
    /// sessions, launch-time overrides, and successful interactive `/model` switches.
    Pinned,
}

/// RAII guard: enables raw mode + bracketed paste on construction,
/// unconditionally restores both on drop (even during panic).
struct TerminalGuard {
    raw_enabled: bool,
    paste_enabled: bool,
    /// Set when the Kitty keyboard protocol (CSI u) was successfully
    /// pushed. Guards the matching pop in Drop so we don't send a stray
    /// pop sequence on terminals that rejected the push.
    kbd_flags_pushed: bool,
}

/// Whether to push the Kitty keyboard protocol (CSI u progressive
/// enhancement) for this terminal. True only on a real non-Windows TTY
/// positively identified as compatible. Windows has no `CSI u` decoder and JediTerm
/// mis-frames mouse reports as kitty key events once the protocol is
/// armed — both leak raw bytes into the input box (see the call site for
/// the full rationale). Pure so it can be unit-tested without touching
/// the real stdout. Mirrored by the resume-path gate in
/// `RetainedRenderer::resume_after_external`.
pub(crate) fn should_enable_kitty_keyboard(caps: &TerminalCaps) -> bool {
    caps.tty && !cfg!(windows) && caps.kitty_keyboard
}

/// Kitty keyboard features used by the TUI.
///
/// Event-type reporting is deliberately excluded. Some terminals split a
/// release report such as `ESC [ 101;1:3u` after the ESC byte; crossterm then
/// treats the lone ESC as a complete key and forwards the remaining bytes as
/// printable input. Disambiguation alone is enough for Shift+Enter, while the
/// reader's modifier+Enter deduplication handles terminals that report key
/// autorepeat as additional presses.
pub(crate) fn kitty_keyboard_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
}

impl TerminalGuard {
    /// Activate terminal capabilities. Returns `(guard, kbd_enhanced)` where
    /// `kbd_enhanced` indicates whether the Kitty keyboard protocol (CSI u)
    /// was successfully enabled. When false, terminals cannot distinguish
    /// Shift+Enter from plain Enter, and users should use Alt+Enter or
    /// Ctrl+Enter for newline insertion instead.
    fn activate(caps: TerminalCaps) -> Result<(Self, bool)> {
        use std::io::Write as _;
        let mut g = Self {
            raw_enabled: false,
            paste_enabled: false,
            kbd_flags_pushed: false,
        };
        if caps.raw_mode {
            // Capture the cooked termios and install fatal-signal terminal-restore
            // handlers BEFORE flipping to raw mode, so a SIGTERM/SIGHUP kill (which
            // runs no Drop) still leaves the shell a usable terminal. Unix-only.
            #[cfg(unix)]
            crate::signal_restore::arm();
            crossterm::terminal::enable_raw_mode()?;
            g.raw_enabled = true;
        }
        if caps.bracketed_paste {
            execute!(io::stdout(), EnableBracketedPaste)?;
            g.paste_enabled = true;
        }
        // Enable Kitty keyboard protocol (CSI u / progressive enhancement)
        // so terminals that support it report modifier+Enter as a distinct
        // key event instead of collapsing Shift+Enter to plain Enter. Without
        // this, crossterm sees `Enter, NONE` on both Enter and Shift+Enter
        // and the input box can't insert a newline.
        //
        // Do not request REPORT_EVENT_TYPES. Release reports are unnecessary
        // for our input model and can leak into the input box when a terminal
        // splits the leading ESC from the rest of a CSI-u report. The reader
        // already deduplicates modifier+Enter autorepeat reported as Press.
        //
        // `execute!` is best-effort — terminals that don't support CSI u
        // (notably Apple Terminal.app, some Linux terminals) ignore the
        // sequence; we just don't set `kbd_flags_pushed` and Drop won't try
        // to pop. Terminals that don't implement disambiguation ignore the
        // request, leaving input behavior unchanged.
        //
        // WINDOWS EXCLUSION: never push on Windows. crossterm's Windows
        // input backend reads Win32 console KEY_EVENT records (not an ANSI
        // parser), so it already reports Shift+Enter modifiers and autorepeat
        // (Press/Repeat/Release) natively — the two things this push buys on
        // Unix — making the protocol pure downside here. Worse, it has no
        // `CSI u` decoder at all, so once a terminal honours the push and
        // starts encoding KEYPAD keys as functional codes (numpad 1 →
        // `ESC[57400u`), ConPTY (VSCode integrated terminal, Windows Terminal)
        // delivers the un-decoded bytes as the literal characters `[57400u`
        // straight into the input box. Unix crossterm decodes 57400 → '1';
        // Windows can't, so we simply don't ask the terminal to use it.
        //
        // JEDITERM EXCLUSION: same failure class on JetBrains' JediTerm (the
        // terminal inside DevEco Studio, IDEA, Android Studio, …). Recent
        // JediTerm advertises the Kitty protocol but mis-implements it: while
        // the progressive-enhancement flags are active it re-frames the
        // terminal's mouse-tracking reports as `CSI <n> u` key events, so a
        // bare mouse *move* over the panel floods stdin with kitty key
        // sequences. crossterm faithfully decodes each `<n>` codepoint to a
        // `Char`, which lands in the input box as a stream of coordinate
        // gibberish (`#B'#B(#@)…`). The push buys nothing here either — the
        // IDE terminals deliver Shift+Enter usably without it — so, exactly
        // like Windows, we don't arm the protocol. Detection reuses
        // `caps.jediterm` (`TERMINAL_EMULATOR=JetBrains-JediTerm`, with the
        // `ATOMCODE_JEDITERM` override for launchers that drop the env var).
        let kbd_enhanced = should_enable_kitty_keyboard(&caps)
            && execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(kitty_keyboard_flags())
            )
            .is_ok();
        if kbd_enhanced {
            g.kbd_flags_pushed = true;
        }
        // FIXED-FOOTER via DECSTBM. Scroll region `[1, H - footer_rows]`
        // is set by `AnsiRenderer` the first time it paints the footer;
        // body writes stream into that region while the footer stays
        // pinned at `[H - footer_rows + 1, H]`. This guard only clears
        // the screen on entry — the renderer owns scroll-region lifecycle
        // during normal operation, and this guard's Drop is the
        // belt-and-suspenders reset for panic / abrupt-exit paths where
        // the renderer worker didn't get to run `shutdown()`.
        if caps.tty {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            // Per-row CUP+EL instead of `\x1b[2J` — iTerm2 3.5+ ignores
            // ED under some states; the renderer paths (reset / resize
            // / resume) all now use EL, so keep startup consistent.
            // Fall back to 24 rows if crossterm can't query size (very
            // rare; a wrong guess just under-clears a few trailing rows
            // at startup — the renderer will paint over anything below
            // that anyway).
            let (_, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            use std::fmt::Write as _;
            let mut seq = String::with_capacity((rows as usize) * 8 + 4);
            for row in 1..=(rows as usize) {
                let _ = write!(seq, "\x1b[{};1H\x1b[K", row);
            }
            seq.push_str("\x1b[H");
            let _ = out.write_all(seq.as_bytes());
            let _ = out.flush();
        }
        Ok((g, kbd_enhanced))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use std::io::Write as _;
        // Panic-safe final reset: `\x1b[?7h` re-enables autowrap (in
        // case a footer paint was interrupted mid-`\x1b[?7l/h` bracket),
        // `\x1b[r` releases any DECSTBM scroll region we set during
        // normal operation, then a CRLF parks the cursor on a fresh
        // line for the user's shell prompt. This runs even when the
        // renderer worker crashed before `shutdown` could clean up,
        // which is why it exists alongside the renderer's own
        // `clear_scroll_region` in `shutdown`.
        let stdout = io::stdout();
        let mut out = stdout.lock();
        let _ = write!(out, "\x1b[?7h\x1b[r\r\n");
        let _ = out.flush();
        if self.kbd_flags_pushed {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        if self.paste_enabled {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
        }
        if self.raw_enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Byte sequence that fully restores the terminal the TUI armed. Pure
/// (returns the bytes; no I/O) so the restore *contract* is unit
/// testable without a real stdout.
///
/// This is the same cleanup `TerminalGuard::Drop` and
/// `RetainedRenderer::Drop` perform on the graceful path — but the
/// release profile sets `panic = "abort"`, so on a crash NO destructor
/// unwinds and neither Drop ever runs. The panic hook
/// (`atomcode-cli::restore_terminal_if_tui`) is then the only code with
/// a chance to undo the mutations, and it previously only disabled raw
/// mode — leaving the Kitty keyboard protocol armed, so the parent
/// shell echoed every post-crash keypress as a literal `[27u` / `[99;5u`
/// CSI-u report (the reported crash artefact).
///
/// Mirrors `RetainedRenderer::Drop` (mouse-mode off, Kitty pop, cursor
/// show, autowrap on, DECSTBM release), then disables focus reporting
/// (`?1004l`) and bracketed paste (`?2004l`) before appending a CRLF so the
/// panic backtrace prints on a fresh line below the last painted TUI row
/// instead of on top of it. Every sequence is idempotent, so emitting it
/// after a graceful shutdown that already sent the same bytes is harmless.
///
/// Bracketed paste is armed by `TerminalGuard`, not the renderer, so it is
/// NOT in `RetainedRenderer::Drop` — but the abrupt-exit paths that lean on
/// this sequence (panic hook, signal-restore handler) DO need it off, else
/// the shell wraps every paste in literal `200~`/`201~`. It is the single
/// source of truth for "undo everything the TUI armed".
///
/// The Kitty pop uses the `<` introducer (`\x1b[<1u`), never `>`: `>`
/// would *arm* the protocol on the way out — the very bug this fixes.
/// Popping a level we never pushed is a harmless no-op (empty kitty
/// stack), so this stays unconditional and we needn't thread the
/// `kbd_flags_pushed` state out of `TerminalGuard`.
pub(crate) fn panic_restore_sequence() -> &'static [u8] {
    b"\x1b[?1006l\x1b[?1002l\x1b[?1004l\x1b[<1u\x1b[?25h\x1b[?7h\x1b[r\x1b[?2004l\r\n"
}

/// Emit [`panic_restore_sequence`] to stdout and flush. Best-effort
/// (errors swallowed) so it is safe to call from a panic hook. Invoked
/// by `atomcode-cli`'s panic hook under `panic = "abort"`, where no Drop
/// runs; see [`panic_restore_sequence`] for the full rationale.
pub fn panic_restore_terminal() {
    use std::io::Write as _;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(panic_restore_sequence());
    let _ = out.flush();
}

pub(crate) fn resolve_history_replay_max_rows(
    config: &atomcode_config::Config,
    caps: &terminal::TerminalCaps,
) -> Option<usize> {
    match config.ui.history_replay_max_rows {
        Some(0) => None,
        Some(rows) => Some(rows),
        None => {
            let term_program = std::env::var("TERM_PROGRAM")
                .unwrap_or_default()
                .to_ascii_lowercase();
            if term_program.contains("vscode") || caps.legacy_conhost || caps.jediterm {
                Some(1_000)
            } else if term_program.contains("wezterm") {
                Some(3_000)
            } else {
                // Keep every automatic cap below RetainedRenderer's 5,000-row
                // memory ceiling; otherwise the "bounded" replay is identical
                // to the legacy full replay and provides no latency benefit.
                Some(2_500)
            }
        }
    }
}

pub(crate) fn sync_history_replay_config(
    renderer: &mut dyn Renderer,
    config: &atomcode_config::Config,
    caps: &terminal::TerminalCaps,
) {
    renderer.set_history_replay_max_rows(resolve_history_replay_max_rows(config, caps));
}

pub async fn run(
    config: Config,
    provider_selection: String,
    model_name: String,
    provider_selection_mode: ProviderSelectionMode,
    config_store: atomcode_config::ConfigStore,
    spawned_runtime: SpawnedRuntime,
    runtime_spawn_override: RuntimeSpawnOverride,
    working_dir: std::path::PathBuf,
    session_to_continue: Option<crate::session::Session>,
    startup_notice: Option<String>,
    telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
    dangerously_skip_permissions: bool,
    is_admin: bool,
) -> Result<()> {
    let tui_start = std::time::Instant::now();
    let SpawnedRuntime {
        endpoint,
        event_rx,
        session_id: runtime_session_id,
    } = spawned_runtime;
    let RuntimeEndpoint { native: runtime } = endpoint;
    let mut caps = TerminalCaps::probe();

    // Decide force_plain BEFORE activating TerminalGuard. Plain mode
    // is incompatible with raw-mode setup: PlainRenderer emits `\n`
    // (LF only) via `writeln!`, but raw mode disables the kernel's
    // ONLCR translation, so LF moves down without returning to col 1.
    // Result: every printed line stair-steps diagonally to the right,
    // exactly matching the bug observed in JediTerm where the welcome
    // banner ends near col 68 and subsequent MCP status lines start
    // there instead of at col 0.
    //
    // `ATOMCODE_PLAIN=1` (or any non-empty value) is the user-facing
    // escape hatch — forces PlainRenderer even on a TTY. Useful for
    // logging, CI capture, or any environment where the append-only
    // retained renderer's ANSI sequences are unwanted.
    //
    // The trade-off when force_plain is on: no pinned input box, no
    // live spinner, no slash-menu palette — but text + commands +
    // agent flow all work, which is the floor.
    let force_plain_env = std::env::var("ATOMCODE_PLAIN")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some();
    let force_retain_env = std::env::var("ATOMCODE_RETAIN")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some();
    // Phase 6 routing matrix (append-only retained renderer):
    //   ATOMCODE_PLAIN=1   → PlainRenderer (user opt-in, CI-style baseline)
    //   ATOMCODE_RETAIN=1  → RetainedRenderer (override sticky non-TTY probe)
    //   tty                → RetainedRenderer (append-only; no DECSTBM)
    //   non-tty            → PlainRenderer
    //
    // `ATOMCODE_RETAIN` exists for hosts where `is_terminal()` lies — the
    // best-known case is pwsh7 on native Win10 conhost: pwsh wraps stdout
    // in a ConPTY pipe even when the parent host is plain conhost, so
    // `std::io::stdout().is_terminal()` returns false. Users see the TUI
    // collapse into PlainRenderer (no input footer) and raw SGR bytes
    // leak as `[31m...[0m` (raw-mode → VT processing was never enabled).
    // Setting ATOMCODE_RETAIN=1 forces the retained path and lets the
    // raw-mode init call SetConsoleMode(ENABLE_VIRTUAL_TERMINAL_PROCESSING)
    // via crossterm, which fixes both symptoms.
    //
    // PLAIN beats RETAIN — if both are set the user explicitly asked for
    // the cooked-mode baseline.
    let force_plain = force_plain_env;
    let force_retain = force_retain_env && !force_plain;

    // Capture whether stdout was a real TTY BEFORE we mutate caps.
    // PlainRenderer needs this to know whether the kernel will echo
    // user input (cooked-mode, real TTY) or not (pipe / CI). Used
    // below when constructing PlainRenderer so the User-line render
    // doesn't duplicate cooked-mode echoes on the ATOMCODE_PLAIN=1
    // force_plain path.
    let was_real_tty = caps.tty;

    // When force_plain wins, strip raw-mode-related capabilities so
    // every downstream branch (TerminalGuard activate, reader spawn,
    // renderer choice) consistently picks the cooked-mode / Plain
    // path. `tty=false` also skips Kitty enhancement push and the
    // startup screen clear (both emit CSI sequences PlainRenderer
    // doesn't need).
    if force_plain {
        caps.raw_mode = false;
        caps.bracketed_paste = false;
        caps.tty = false;
    } else if force_retain {
        // Flip caps positive so TerminalGuard enables raw mode (→
        // VT processing on Win), the reader thread spawns, and the
        // renderer-choice branch picks Retained. If the probe lied
        // about TTY this corrects it; if it didn't lie, this is a
        // no-op assignment.
        caps.tty = true;
        caps.raw_mode = true;
        caps.bracketed_paste = true;
        caps.scroll_region = true;
    }

    let (_guard, kbd_enhanced) = TerminalGuard::activate(caps)?;

    // Pick the colour palette now that raw mode is on (OSC 11 detection
    // requires it — otherwise the response is line-buffered and never
    // reaches us before timeout).
    //
    // - `Light` / `Dark`: explicit, skip detection.
    // - `Auto`: query the terminal background, falling back to `dark`.
    //   `detect_light` pairs the OSC 11 query with a DA1 query and drains
    //   until the DA1 reply, so the wait is one round-trip on any
    //   responsive terminal regardless of latency (SSH / tmux included) —
    //   the `60ms` here is just the first-byte floor (raised internally to
    //   ~400ms so an SSH RTT can't beat it) and never leaks the OSC reply
    //   into the input box. Terminals that answer neither query default to
    //   dark after the internal cap.
    let theme_light = match config.ui.theme {
        atomcode_config::config::UiTheme::Light => true,
        atomcode_config::config::UiTheme::Dark => false,
        atomcode_config::config::UiTheme::Auto => {
            if caps.colors && !force_plain {
                crate::terminal_bg::detect_light(std::time::Duration::from_millis(60))
                    .unwrap_or(false)
            } else {
                false
            }
        }
    };
    crate::tuix_trace!(
        "START",
        "stage=terminal_probe elapsed_ms={}",
        tui_start.elapsed().as_millis()
    );
    crate::highlight::theme::set_theme_mode(theme_light);

    // If the terminal doesn't support Kitty keyboard protocol (CSI u),
    // set an env var so the event loop can show a hint on startup.
    // Shift+Enter won't work for newline insertion; users should use
    // Alt+Enter or Ctrl+Enter instead.
    if !kbd_enhanced {
        std::env::set_var("ATOMCODE_KBD_NOT_ENHANCED", "1");
    }

    // Pick the inner renderer by terminal capability, then wrap it in
    // a `TaskRenderer` so all ANSI I/O happens on a dedicated OS thread.
    // Slow terminals (Mac Terminal.app processing a 4KB footer payload)
    // no longer block the event loop — the event loop sends `UiLine`s
    // through a channel and moves on.
    //
    // TTY    → RetainedRenderer (append-only Ink-style cell-diff renderer).
    // Non-TTY → PlainRenderer (pipe, CI, dumb terminal, ATOMCODE_PLAIN=1).
    //
    // Since Phase 5 the retained renderer is fully append-only and no
    // longer relies on DECSTBM scroll regions, so JediTerm and legacy
    // Windows conhost — the two terminals that previously needed an
    // alt-screen path — can run retained too. There is no more
    // alt-screen branch here.
    //
    // `is_plain_renderer` is threaded into LoopCtx so non-interactive
    // sessions (CI, pipe, dumb TERM) can skip the OnboardingWizard
    // auto-trigger; the modal would otherwise try to draw a
    // Cyan-bordered box into a stdout that no human is watching.
    let is_plain_renderer = !caps.tty;
    let mut inner: Box<dyn Renderer> = if caps.tty {
        Box::new(RetainedRenderer::new(caps))
    } else {
        // Pass caps + the ORIGINAL tty value so PlainRenderer can:
        // (a) gate colours / unicode / spinner on caps.{colors,
        //     unicode_symbols, spinner} (these survive the force_plain
        //     mutation; CI / pipe don't have them);
        // (b) decide whether to suppress UiLine::User echo based on
        //     `was_real_tty` — true means the kernel does cooked-mode
        //     echo for us (so re-rendering would duplicate the line),
        //     false means we're piping and need to render it ourselves.
        Box::new(PlainRenderer::with_writer_caps_and_interactive(
            std::io::BufWriter::new(std::io::stdout()),
            caps,
            was_real_tty,
        ))
    };
    // Code-block auto-copy (issue #699): OFF by default (it silently overwrote the
    // user's clipboard on every reply). Opt-in via `config.ui.auto_copy_code_blocks`;
    // `ATOMCODE_AUTO_COPY` overrides the config when set (back-compat with the old env
    // knob). Set on the concrete renderer BEFORE it's moved onto the worker thread, so
    // no per-command plumbing is needed. `/copy` stays available regardless.
    let auto_copy = match std::env::var("ATOMCODE_AUTO_COPY") {
        Ok(v) => v != "0" && !v.eq_ignore_ascii_case("false"),
        Err(_) => config.ui.auto_copy_code_blocks,
    };
    inner.set_auto_copy_enabled(auto_copy);
    let history_replay_max_rows = resolve_history_replay_max_rows(&config, &caps);
    inner.set_history_replay_max_rows(history_replay_max_rows);
    let mut renderer: Box<dyn Renderer> = Box::new(TaskRenderer::new(inner));

    // Input thread (only spawn when raw-mode/TTY available; pipe mode
    // reads stdin directly). `reader_handle` exposes Pause / Resume so
    // the OAuth login flow (and any future child-process handoff) can
    // stop us from racing the child for stdin bytes. Pipe mode doesn't
    // need that — no browser handoff there — so it stays as a plain
    // JoinHandle held separately.
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let mut reader_handle: Option<reader::ReaderHandle> = None;
    let mut pipe_reader: Option<std::thread::JoinHandle<()>> = None;
    if caps.raw_mode {
        reader_handle = Some(reader::spawn(input_tx.clone()));
    } else {
        // For pipe mode, spawn a line-based reader on a blocking thread.
        pipe_reader = Some(std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            let lock = stdin.lock();
            for line in lock.lines().map_while(Result::ok) {
                // Synthesize a key-by-key paste so the loop handles it uniformly.
                if input_tx.send(input::InputEvent::Paste(line)).is_err() {
                    return;
                }
                // Then an Enter key to commit.
                let enter = crossterm::event::KeyEvent {
                    code: crossterm::event::KeyCode::Enter,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                    kind: crossterm::event::KeyEventKind::Press,
                    state: crossterm::event::KeyEventState::NONE,
                };
                if input_tx.send(input::InputEvent::Key(enter)).is_err() {
                    return;
                }
            }
            let _ = input_tx.send(input::InputEvent::Eof);
        }));
    };

    // `default_path()` now always returns Some (tempdir fallback lives
    // inside `platform::history_path`), so the explicit else-branch
    // with a hardcoded Unix path is gone — Windows used to fall here
    // and then fail to write to `/tmp`.
    let history_start = std::time::Instant::now();
    let history = {
        let path = History::default_path().unwrap_or_else(crate::platform::history_path);
        let cache = crate::platform::image_cache_dir();
        crate::input::history::History::load_with_cache(path, cache)
    };
    crate::tuix_trace!(
        "START",
        "stage=history_load elapsed_ms={} total_ms={}",
        history_start.elapsed().as_millis(),
        tui_start.elapsed().as_millis()
    );

    // Fresh session by default; `/resume` replaces this on load.
    let mut current_session = crate::session::Session::default_session(working_dir.clone());
    if let Some(session_id) = runtime_session_id {
        current_session.id = session_id;
    }

    // Passive "new version available" check. Detached — never blocks
    // startup; on any error returns None silently. On a positive hit
    // the task (a) stores the version in the shared mutex and (b) sends
    // a wake pulse so the event loop redraws the status row immediately
    // instead of waiting for the user's next keystroke.
    let update_hint = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let (wake_tx, wake_rx) = tokio::sync::mpsc::channel::<()>(1);
    // Background OAuth poll → event-loop channel. Unbounded so the
    // poll thread never blocks waiting for the consumer (poll thread
    // is std::thread, can't `await`). One event per spawned task,
    // capacity is irrelevant — even an unbounded channel is essentially
    // empty here.
    let (oauth_event_tx, oauth_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::event_loop::oauth_poll::OauthEvent>();

    // Seed the hint from any prior-session staged upgrade so the user
    // sees the pending status on the very first frame rather than
    // waiting for the next poll to rediscover it.
    if let Ok(Some(pending)) = atomcode_updater::read_pending() {
        if let Ok(mut g) = update_hint.lock() {
            *g = Some(pending.version);
        }
    }

    {
        let slot = update_hint.clone();
        let wake = wake_tx.clone();
        tokio::spawn(async move {
            let current = format!("v{}", env!("CARGO_PKG_VERSION"));
            if let Some(latest) = crate::version_check::check_latest(&current).await {
                if let Ok(mut g) = slot.lock() {
                    *g = Some(latest);
                }
                let _ = wake.try_send(());
            }
        });
    }

    // NOTE: the in-process deferred-upgrade poll used to live here. It
    // was moved out into a detached setsid'd subprocess spawned from
    // `main.rs` (see `spawn_detached_upgrade_prep`). Rationale: the old
    // task was tied to this tokio runtime, so any Ctrl+C / quick exit
    // cancelled the download mid-flight and `pending.json` was never
    // written — making "exit and restart to auto-upgrade" silently do
    // nothing. The detached subprocess survives parent exits. Running
    // both would race on `staged_path` (no temp-rename in
    // `download_and_verify`), so the in-process copy is gone entirely.
    //
    // Trade-off: a session that runs through a whole release cycle
    // (>1 h) won't re-stage the newer version mid-session. We accept
    // that — `/upgrade` still works manually, and the update hint from
    // the one-shot `version_check` above still surfaces the availability.

    // Long-lived progress channel for /upgrade. The sender is cloned
    // into each spawned upgrade task; the receiver stays in the event
    // loop's select!. Unbounded because progress events are tiny and
    // we never want the upgrade task to block on UI backpressure.
    let (upgrade_tx, upgrade_rx) =
        tokio::sync::mpsc::unbounded_channel::<atomcode_updater::UpgradeEvent>();
    // Mirror channel for /plugin add|update|install so git latency never
    // stalls the input loop. See LoopCtx::plugin_job_tx for the rationale.
    let (plugin_job_tx, plugin_job_rx) =
        tokio::sync::mpsc::unbounded_channel::<atomcode_capabilities::plugin::PluginJobEvent>();

    // Seed the recent-project-dirs ring from disk and guarantee the
    // current working dir sits at index 0 so the `/cd` picker always
    // has at least one entry (the dir the user just launched into).
    let recent_dirs = {
        let mut dirs = event_loop::commands::load_recent_dirs();
        event_loop::commands::push_recent_dir(&mut dirs, working_dir.clone());
        event_loop::commands::save_recent_dirs(&dirs);
        dirs
    };

    let capability_scan_start = std::time::Instant::now();
    let custom_commands = crate::custom_commands::CustomCommandRegistry::load(&working_dir);
    // Same Arc the agent loop holds — reload() calls there propagate
    // here automatically, so the slash menu reflects newly-installed
    // skills without re-plumbing.
    let foreground_runtime_id = event_loop::bg_runtime::RuntimeId::new(1);
    let skill_registry = std::sync::Arc::new(std::sync::RwLock::new(
        atomcode_capabilities::skills::SkillRegistry::new(),
    ));
    if let Ok(mut registry) = skill_registry.write() {
        let _ = event_loop::reload_skill_registry(&mut registry, &working_dir);
    }
    crate::tuix_trace!(
        "START",
        "stage=tui_capability_scan elapsed_ms={} total_ms={}",
        capability_scan_start.elapsed().as_millis(),
        tui_start.elapsed().as_millis()
    );

    // ── Plugin marketplace bootstrap (detached) ──
    //
    // Auto-install of the default skills marketplace + post-self-upgrade
    // `git pull` of every installed marketplace. Both run git subprocesses
    // inline (1–3s warm, 5–10s on first clone) — keeping them on the
    // critical path used to delay the input box by the same amount. Now
    // we hand the work to `spawn_blocking`, refresh the shared
    // `SkillRegistry` once the disk side-effects land, then forward each
    // resulting `PluginJobEvent` through `plugin_job_tx` so the event
    // loop's existing `handle_plugin_job_event` renders the same toast
    // the synchronous `/plugin install` path would emit (e.g.
    // "marketplace `atomcode` added at abc1234 (3 plugins)" ).
    // The user sees the install land as a regular body row instead of
    // a silent file-system mutation. Worst case the user types `/`
    // before the install settles — they see an empty / partial menu
    // and the toast arrives a beat later; acceptable trade-off vs.
    // burning 5–10s on every first launch.
    {
        let cfg = config.clone();
        let registry = skill_registry.clone();
        let work_dir = working_dir.clone();
        let wake = wake_tx.clone();
        let job_tx = plugin_job_tx.clone();
        tokio::spawn(async move {
            let events = tokio::task::spawn_blocking(move || {
                let events = atomcode_capabilities::plugin::bootstrap::run_startup_hooks(&cfg);
                // Refresh the shared SkillRegistry from disk so the
                // freshly-installed skills are visible to the slash
                // menu + agent loop without a restart.
                if let Ok(mut guard) = registry.write() {
                    let _ = event_loop::reload_skill_registry(&mut guard, &work_dir);
                }
                events
            })
            .await
            .unwrap_or_default();
            for ev in events {
                let _ = job_tx.send(ev);
            }
            let _ = wake.try_send(());
        });
    }

    let (runtime_event_tx, runtime_event_rx) =
        tokio::sync::mpsc::unbounded_channel::<event_loop::bg_runtime::RuntimeEvent>();
    event_loop::bg_runtime::spawn_event_forwarder(
        foreground_runtime_id,
        event_rx,
        runtime_event_tx.clone(),
    );
    let bg_manager = event_loop::bg_runtime::BgRuntimeManager::new(
        current_session.clone(),
        working_dir.clone(),
        foreground_runtime_id,
        event_loop::RuntimeEndpoint {
            native: runtime.clone(),
        },
    );

    // ── Askpass server startup (unix-only, TUI path) ────────────────────────
    // Start the Unix-socket askpass server so that child processes (sudo, ssh)
    // can forward password prompts to the TUI instead of reading from the tty.
    // On failure we degrade gracefully: askpass simply isn't available, sudo
    // will use its own fallback and the TUI will not crash.
    //
    // The guard MUST outlive run_loop — its Drop removes the socket file.
    // We bind it in the outer `run()` scope and explicitly reference it after
    // `run_loop` returns so the compiler does not drop it early.
    #[cfg(unix)]
    let _askpass_guard: Option<atomcode_capabilities::askpass::server::AskpassServerGuard>;
    #[cfg(unix)]
    let askpass_rx: Option<
        tokio::sync::mpsc::Receiver<atomcode_capabilities::askpass::server::AskpassPrompt>,
    >;

    #[cfg(unix)]
    {
        let cache = std::sync::Arc::new(atomcode_capabilities::askpass::cache::PasswordCache::new(
            std::time::Duration::from_secs(300),
        ));
        match atomcode_capabilities::askpass::server::start(cache) {
            Ok((mut env, rx, guard)) => {
                // Write the wrapper script next to the socket.  On failure,
                // degrade (no askpass) rather than crashing the TUI.
                let script_result = std::env::current_exe().ok().and_then(|exe| {
                    env.sock_path.parent().and_then(|dir| {
                        atomcode_capabilities::askpass::wrapper::write_askpass_script(&exe, dir)
                            .ok()
                    })
                });
                if let Some(script) = script_result {
                    env.askpass_script = script;
                    atomcode_capabilities::askpass::set_env(env);
                }
                askpass_rx = Some(rx);
                _askpass_guard = Some(guard);
            }
            Err(_) => {
                askpass_rx = None;
                _askpass_guard = None;
            }
        }
    }

    let file_index_root = working_dir.clone();
    let ctx = LoopCtx {
        config,
        provider_selection,
        model_name,
        provider_selection_mode,
        config_store,
        // Reconcile once inside the event loop instead of marking a fresh disk
        // revision as observed here. The config passed into `run` was loaded
        // earlier, so another process may have committed between those reads.
        observed_config_revision: None,
        pending_provider_reload: None,
        pending_provider_projection: None,
        // Reconcile authentication against the runtime after startup. Keeping
        // this unobserved closes the race where auth.toml changes while the
        // runtime is still being assembled.
        observed_auth: None,
        pending_provider_deactivation: false,
        runtime,
        pending_runtime_request_id: None,
        allowed_always: std::sync::Arc::new(
            std::sync::Mutex::new(std::collections::HashSet::new()),
        ),
        native_tools: std::collections::HashMap::new(),
        shutdown_deadline: None,
        runtime_spawn_override,
        bg_manager,
        foreground_runtime_id,
        runtime_event_tx,
        runtime_event_rx,
        foreground_replay_events: std::collections::VecDeque::new(),
        working_dir,
        previous_dir: None,
        recent_dirs,
        history,
        input_rx,
        commands: CommandRegistry::builtin(),
        current_session,
        update_hint,
        monitor_warning: std::sync::Arc::new(std::sync::Mutex::new(None)),
        hook_warning_hint: std::sync::Arc::new(std::sync::Mutex::new(None)),
        monitor_last_check_at: None,
        usage_slot: std::sync::Arc::new(std::sync::Mutex::new(None)),
        usage_last_check_at: None,
        // Seed with whatever's on disk now — any NEWER mtime observed
        // later means another atomcode process resynced and our drift
        // warning (if any) is stale.
        monitor_last_sync_seen: atomcode_codingplan::read_last_sync(),
        wake_rx,
        wake_tx: wake_tx.clone(),
        oauth_event_rx,
        oauth_event_tx,
        reader: reader_handle,
        upgrade_tx,
        upgrade_rx,
        plugin_job_tx,
        plugin_job_rx,
        pending_run_login_setup: false,
        pending_open_provider_wizard: false,
        telemetry,
        worktree_original_dir: None,
        custom_commands,
        skill_registry,
        caps,
        replay_on_start: session_to_continue,
        startup_notice,
        file_index: crate::event_loop::file_index::FileIndex::new(file_index_root),
        current_session_id: None,
        current_session_project_bucket: None,
        pending_session_resume: None,
        pending_session_resume_preparation: None,
        pending_session_picker: None,
        pending_rewind_catalog: None,
        pending_session_transition: None,
        pending_external_session_projection: None,
        pending_capability_reload: false,
        pending_mcp_reload_server_count: None,
        pending_capability_projection: None,
        clipboard_check: std::sync::Arc::new(std::sync::Mutex::new(
            crate::event_loop::ClipboardCheckState::default(),
        )),
        is_plain_renderer,
        dangerously_skip_permissions,
        is_admin,
        pending_guide_topic: None,
        live_binding: None,
        live_observation_task: None,
        app_relay_child: None,
        reasoning_effort: None,
        transient_hint: std::sync::Arc::new(std::sync::Mutex::new(None)),
        #[cfg(unix)]
        askpass_rx,
        loop_ctrl: None,
    };

    // CodingPlan drift monitor — kick off a startup check if the current
    // default provider is CodingPlan-managed. Non-CodingPlan users skip
    // this entirely (no HTTP, no state touched). Check runs in the
    // background via tokio::spawn → the warning shows up on the next
    // footer repaint once it resolves.
    if event_loop::monitor::is_codingplan_provider(&ctx.config.default_provider) {
        event_loop::monitor::spawn_check(
            ctx.config.clone(),
            ctx.model_name.clone(),
            ctx.monitor_warning.clone(),
            ctx.wake_tx.clone(),
        );
    }

    crate::tuix_trace!(
        "START",
        "stage=event_loop_ready total_ms={}",
        tui_start.elapsed().as_millis()
    );
    let result = run_loop(ctx, renderer.as_mut()).await;

    // Must shut down the renderer BEFORE re-exec: the alternate screen is
    // still active and raw mode is on — if we spawn a child while the
    // terminal is in that state, the new process inherits a garbled TTY.
    renderer.shutdown();
    drop(pipe_reader); // pipe-mode thread exits on next channel send failure

    // If /upgrade succeeded, the live binary has been replaced on disk.
    // Re-exec into the new version so the user gets a seamless upgrade
    // without manually restarting. This mirrors the startup-time upgrade
    // path in main.rs (apply_pending_upgrade → re_exec_self).
    //
    // The exe path comes from `ExitReason::UpgradeRestart { exe }`, which
    // was captured *before* `replace_binary` renamed the running binary.
    // On Windows, `std::env::current_exe()` would return the renamed
    // `.atomcode.rolling` path after the swap, so we MUST use this saved
    // value instead.
    if let Ok(event_loop::ExitReason::UpgradeRestart { exe }) = &result {
        // Set env var so the new process can show a one-time "upgraded" banner
        // on the welcome screen.
        std::env::set_var(
            "ATOMCODE_UPGRADED_FROM",
            format!("v{}", env!("CARGO_PKG_VERSION")),
        );
        match atomcode_updater::re_exec_self(Some(exe)) {
            Ok(_infallible) => unreachable!("re_exec_self returned Ok"),
            Err(e) => {
                // Re-exec failed. The upgrade is on disk, so the user just
                // needs to start atomcode again — don't treat this as fatal.
                eprintln!(
                    "Upgrade applied but re-exec failed ({}). The new version will be used on the next launch.",
                    e
                );
                std::env::remove_var("ATOMCODE_UPGRADED_FROM");
            }
        }
    }

    result.map(|_| ())
}

#[cfg(test)]
mod panic_restore_tests {
    use super::{
        kitty_keyboard_flags, panic_restore_sequence, resolve_history_replay_max_rows,
    };
    use crossterm::event::KeyboardEnhancementFlags;

    fn test_caps() -> crate::terminal::TerminalCaps {
        crate::terminal::TerminalCaps {
            tty: true,
            colors: true,
            spinner: true,
            bracketed_paste: true,
            raw_mode: true,
            scroll_region: true,
            unicode_symbols: true,
            legacy_conhost: false,
            jediterm: false,
            modern_emulator: true,
            kitty_keyboard: true,
        }
    }

    #[test]
    fn automatic_history_replay_cap_is_below_retained_memory_limit() {
        let cap = resolve_history_replay_max_rows(&atomcode_config::Config::default(), &test_caps())
            .expect("automatic replay must stay bounded");
        assert!(cap < crate::render::retained::MAX_SCROLLBACK_ROWS);
    }

    #[test]
    fn configured_history_replay_cap_preserves_zero_and_custom_values() {
        let mut config = atomcode_config::Config::default();
        config.ui.history_replay_max_rows = Some(0);
        assert_eq!(resolve_history_replay_max_rows(&config, &test_caps()), None);
        config.ui.history_replay_max_rows = Some(777);
        assert_eq!(
            resolve_history_replay_max_rows(&config, &test_caps()),
            Some(777)
        );
    }

    #[test]
    fn kitty_keyboard_flags_do_not_request_release_events() {
        assert_eq!(
            kitty_keyboard_flags(),
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        );
    }

    /// The panic hook is the ONLY terminal-cleanup that runs under
    /// `panic = "abort"` — no Drop (TerminalGuard, RetainedRenderer)
    /// unwinds. So the bytes it emits must single-handedly undo every
    /// terminal mutation the TUI armed, or the parent shell is left
    /// echoing CSI-u keypresses as literal `[27u` / `[99;5u` gibberish
    /// (the reported crash). This pins the exact restore contract.
    #[test]
    fn panic_restore_sequence_pops_kitty_keyboard_protocol() {
        let s = panic_restore_sequence();
        let text = String::from_utf8_lossy(s);
        // Kitty keyboard pop — `<` introducer (pop), the fix for the
        // literal CSI-u echo. Must be present.
        assert!(
            text.contains("\x1b[<1u"),
            "must pop Kitty keyboard flags (CSI < u): {text:?}"
        );
        // And must NEVER push (`>` introducer) — re-arming on the way
        // out leaves the shell in CSI-u mode (the exact bug).
        assert!(
            !text.contains("\x1b[>"),
            "must not re-arm Kitty protocol on exit: {text:?}"
        );
    }

    #[test]
    fn panic_restore_sequence_restores_cursor_autowrap_scroll_and_mouse() {
        let text = String::from_utf8_lossy(panic_restore_sequence());
        assert!(
            text.contains("\x1b[?25h"),
            "must show cursor (DECTCEM): {text:?}"
        );
        assert!(
            text.contains("\x1b[?7h"),
            "must re-enable autowrap (DECAWM): {text:?}"
        );
        assert!(
            text.contains("\x1b[r"),
            "must release DECSTBM scroll region: {text:?}"
        );
        assert!(
            text.contains("\x1b[?1002l"),
            "must disable button-event mouse: {text:?}"
        );
        assert!(
            text.contains("\x1b[?1006l"),
            "must disable SGR mouse coords: {text:?}"
        );
        assert!(
            text.contains("\x1b[?2004l"),
            "must disable bracketed paste: {text:?}"
        );
        assert!(
            text.contains("\x1b[?1004l"),
            "must disable focus reporting: {text:?}"
        );
    }
}
