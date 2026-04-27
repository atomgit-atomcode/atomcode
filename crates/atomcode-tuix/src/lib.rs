// crates/atomcode-tuix/src/lib.rs

pub mod commands;
pub mod event_loop;
pub mod input;
pub mod markdown;
pub mod modals;
pub mod platform;
pub mod render;
pub mod sanitize;
pub mod state;
pub mod terminal;
#[cfg(test)]
pub mod test_term;
pub mod think;
pub mod trace;
pub mod width;

use anyhow::Result;
use atomcode_core::agent::AgentHandle;
use atomcode_core::config::Config;
use atomcode_core::tool::ToolContext;
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
use crate::event_loop::{run_loop, LoopCtx};
use crate::input::history::History;
use crate::input::reader;
use crate::render::{
    plain::PlainRenderer, retained::RetainedRenderer, worker::TaskRenderer, Renderer,
};
use crate::terminal::TerminalCaps;

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
        // `REPORT_EVENT_TYPES` is the second bit of the protocol and is what
        // actually makes OS key autorepeat distinguishable from fresh presses:
        // without it, every 30ms autorepeat tick reports as `KeyEventKind::Press`,
        // so holding Shift+Enter for a normal 150ms press-down inserts 5-10
        // newlines instead of one. With it enabled, autorepeats report as
        // `KeyEventKind::Repeat` and are filtered out in `event_loop/mod.rs`.
        //
        // `execute!` is best-effort — terminals that don't support CSI u
        // (notably Apple Terminal.app, some Linux terminals) ignore the
        // sequence; we just don't set `kbd_flags_pushed` and Drop won't try
        // to pop. Terminals that support DISAMBIGUATE but not
        // REPORT_EVENT_TYPES ignore the extra bit silently — this never
        // makes things worse than before.
        let kbd_enhanced = caps.tty
            && execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                )
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

pub async fn run(
    config: Config,
    model_name: String,
    agent_handle: AgentHandle,
    _tool_context: ToolContext,
    working_dir: std::path::PathBuf,
    _session_to_continue: Option<atomcode_core::session::Session>,
    mcp_registry: Option<std::sync::Arc<atomcode_core::mcp::McpRegistry>>,
    mcp_connect_rx: Option<tokio::sync::mpsc::UnboundedReceiver<atomcode_core::mcp::McpConnectEvent>>,
    telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
) -> Result<()> {
    let caps = TerminalCaps::probe();
    let (_guard, kbd_enhanced) = TerminalGuard::activate(caps)?;

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
    // TTY → retained-mode Ink-style cell-diff renderer.
    // Non-TTY (pipe, CI, dumb terminal) → PlainRenderer, which
    // just writes plain text without ANSI cursor positioning.
    //
    // `ATOMCODE_PLAIN=1` (or any non-empty value) forces PlainRenderer
    // even on a TTY — escape hatch for terminals where the retained
    // path's DECSTBM scroll region / cursor positioning misbehaves
    // (notably reported on legacy Windows conhost: the fixed footer
    // scrolls off-screen, content appears duplicated, viewport drifts
    // upward on each redraw). PlainRenderer does no cursor positioning
    // and no scroll-region tricks, so it works on any terminal that
    // can print bytes — at the cost of losing the pinned input box,
    // live spinner, and slash-menu palette. All commands and agent
    // functionality are unchanged.
    let force_plain = std::env::var("ATOMCODE_PLAIN")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some();
    let inner: Box<dyn Renderer> = if caps.tty && !force_plain {
        Box::new(RetainedRenderer::new(caps))
    } else {
        Box::new(PlainRenderer::new())
    };
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
    let history = History::default_path()
        .map(History::load)
        .unwrap_or_else(|| History::load(crate::platform::history_path()));

    let session_manager = atomcode_core::session::SessionManager::new(&working_dir);
    // Fresh session by default; `/resume` replaces this on load.
    let current_session = atomcode_core::session::Session::default_session(working_dir.clone());

    // Passive "new version available" check. Detached — never blocks
    // startup; on any error returns None silently. On a positive hit
    // the task (a) stores the version in the shared mutex and (b) sends
    // a wake pulse so the event loop redraws the status row immediately
    // instead of waiting for the user's next keystroke.
    let update_hint = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let (wake_tx, wake_rx) = tokio::sync::mpsc::channel::<()>(1);

    // Seed the hint from any prior-session staged upgrade so the user
    // sees the pending status on the very first frame rather than
    // waiting for the next poll to rediscover it.
    if let Ok(Some(pending)) = atomcode_core::self_update::read_pending() {
        if let Ok(mut g) = update_hint.lock() {
            *g = Some(pending.version);
        }
    }

    {
        let slot = update_hint.clone();
        let wake = wake_tx.clone();
        tokio::spawn(async move {
            let current = format!("v{}", env!("CARGO_PKG_VERSION"));
            if let Some(latest) = atomcode_core::version_check::check_latest(&current).await {
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
        tokio::sync::mpsc::unbounded_channel::<atomcode_core::self_update::UpgradeEvent>();

    // Seed the recent-project-dirs ring from disk and guarantee the
    // current working dir sits at index 0 so the `/cd` picker always
    // has at least one entry (the dir the user just launched into).
    let recent_dirs = {
        let mut dirs = event_loop::commands::load_recent_dirs();
        event_loop::commands::push_recent_dir(&mut dirs, working_dir.clone());
        event_loop::commands::save_recent_dirs(&dirs);
        dirs
    };

    let ctx = LoopCtx {
        config,
        model_name,
        agent: agent_handle,
        working_dir,
        previous_dir: None,
        recent_dirs,
        history,
        input_rx,
        commands: CommandRegistry::builtin(),
        session_manager,
        current_session,
        update_hint,
        monitor_warning: std::sync::Arc::new(std::sync::Mutex::new(None)),
        monitor_last_check_at: None,
        // Seed with whatever's on disk now — any NEWER mtime observed
        // later means another atomcode process resynced and our drift
        // warning (if any) is stale.
        monitor_last_sync_seen: atomcode_core::coding_plan::read_last_sync(),
        wake_rx,
        wake_tx: wake_tx.clone(),
        reader: reader_handle,
        upgrade_tx,
        upgrade_rx,
        pending_new_issue: None,
        pending_run_codingplan: false,
        pending_open_provider_wizard: false,
        mcp_registry,
        mcp_connect_rx,
        mcp_reload: None,
        telemetry,
        caps,
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

    let result = run_loop(ctx, renderer.as_mut()).await;

    renderer.shutdown();
    drop(pipe_reader); // pipe-mode thread exits on next channel send failure

    result
}
