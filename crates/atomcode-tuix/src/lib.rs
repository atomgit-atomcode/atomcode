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
use crossterm::{execute, event::{EnableBracketedPaste, DisableBracketedPaste}};
use std::io;
use tokio::sync::mpsc;

use crate::commands::CommandRegistry;
use crate::event_loop::{run_loop, LoopCtx};
use crate::input::history::History;
use crate::input::reader;
use crate::render::{plain::PlainRenderer, retained::RetainedRenderer, worker::TaskRenderer, Renderer};
use crate::terminal::TerminalCaps;

/// RAII guard: enables raw mode + bracketed paste on construction,
/// unconditionally restores both on drop (even during panic).
struct TerminalGuard {
    raw_enabled: bool,
    paste_enabled: bool,
}

impl TerminalGuard {
    fn activate(caps: TerminalCaps) -> Result<Self> {
        use std::io::Write as _;
        let mut g = Self {
            raw_enabled: false,
            paste_enabled: false,
        };
        if caps.raw_mode {
            crossterm::terminal::enable_raw_mode()?;
            g.raw_enabled = true;
        }
        if caps.bracketed_paste {
            execute!(io::stdout(), EnableBracketedPaste)?;
            g.paste_enabled = true;
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
            let _ = write!(out, "\x1b[2J\x1b[H");
            let _ = out.flush();
        }
        Ok(g)
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
) -> Result<()> {
    let caps = TerminalCaps::probe();
    let _guard = TerminalGuard::activate(caps)?;

    // Pick the inner renderer by terminal capability, then wrap it in
    // a `TaskRenderer` so all ANSI I/O happens on a dedicated OS thread.
    // Slow terminals (Mac Terminal.app processing a 4KB footer payload)
    // no longer block the event loop — the event loop sends `UiLine`s
    // through a channel and moves on.
    // TTY → retained-mode Ink-style cell-diff renderer.
    // Non-TTY (pipe, CI, dumb terminal) → PlainRenderer, which
    // just writes plain text without ANSI cursor positioning.
    let inner: Box<dyn Renderer> = if caps.tty {
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

    // Passive "new version available" check. Detached — never blocks
    // startup; on any error returns None silently. On a positive hit
    // the task (a) stores the version in the shared mutex and (b) sends
    // a wake pulse so the event loop redraws the status row immediately
    // instead of waiting for the user's next keystroke.
    let update_hint = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let (wake_tx, wake_rx) = tokio::sync::mpsc::channel::<()>(1);
    {
        let slot = update_hint.clone();
        tokio::spawn(async move {
            let current = format!("v{}", env!("CARGO_PKG_VERSION"));
            if let Some(latest) = atomcode_core::version_check::check_latest(&current).await {
                if let Ok(mut g) = slot.lock() {
                    *g = Some(latest);
                }
                let _ = wake_tx.try_send(());
            }
        });
    }

    let ctx = LoopCtx {
        config,
        model_name,
        agent: agent_handle,
        working_dir,
        previous_dir: None,
        history,
        input_rx,
        commands: CommandRegistry::builtin(),
        session_manager,
        update_hint,
        wake_rx,
        reader: reader_handle,
    };

    let result = run_loop(ctx, renderer.as_mut()).await;

    renderer.shutdown();
    drop(pipe_reader); // pipe-mode thread exits on next channel send failure

    result
}
