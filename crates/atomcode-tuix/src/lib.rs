// crates/atomcode-tuix/src/lib.rs

pub mod commands;
pub mod event_loop;
pub mod input;
pub mod markdown;
pub mod render;
pub mod sanitize;
pub mod state;
pub mod terminal;
pub mod think;
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
use crate::render::{ansi::AnsiRenderer, plain::PlainRenderer, Renderer};
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
        // PURE APPEND ARCHITECTURE — no scroll region, no DECSTBM.
        // Footer is drawn at the current cursor position; content writes
        // erase and redraw the footer; terminal scrolls naturally when
        // cursor reaches the bottom row. No region boundaries means no
        // transition bugs.
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
        // Ensure scroll region and autowrap reset defensively in case any
        // renderer code emitted DECSTBM. Cursor to a fresh row for shell.
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

    let mut renderer: Box<dyn Renderer> = if caps.tty {
        Box::new(AnsiRenderer::new(caps))
    } else {
        Box::new(PlainRenderer::new())
    };

    // Input thread (only spawn when raw-mode/TTY available; pipe mode reads stdin directly)
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let reader_handle = if caps.raw_mode {
        Some(reader::spawn(input_tx.clone()))
    } else {
        // For pipe mode, spawn a line-based reader on a blocking thread.
        Some(std::thread::spawn(move || {
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
        }))
    };

    let history = History::default_path()
        .map(History::load)
        .unwrap_or_else(|| History::load(std::path::PathBuf::from("/tmp/atomcode-history")));

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

    // Long-lived progress channel for /upgrade. The sender is cloned
    // into each spawned upgrade task; the receiver stays in the event
    // loop's select!. Unbounded because progress events are tiny and
    // we never want the upgrade task to block on UI backpressure.
    let (upgrade_tx, upgrade_rx) =
        tokio::sync::mpsc::unbounded_channel::<atomcode_core::self_update::UpgradeEvent>();

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
        upgrade_tx,
        upgrade_rx,
    };

    let result = run_loop(ctx, renderer.as_mut()).await;

    renderer.shutdown();
    drop(reader_handle); // thread exits on next channel send failure

    result
}
