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

/// RAII guard: enables raw mode + bracketed paste + scroll region on
/// construction, unconditionally restores all three on drop (even during panic).
struct TerminalGuard {
    raw_enabled: bool,
    paste_enabled: bool,
    scroll_region_set: bool,
}

impl TerminalGuard {
    fn activate(caps: TerminalCaps) -> Result<Self> {
        use std::io::Write as _;
        let mut g = Self {
            raw_enabled: false,
            paste_enabled: false,
            scroll_region_set: false,
        };
        if caps.raw_mode {
            crossterm::terminal::enable_raw_mode()?;
            g.raw_enabled = true;
        }
        if caps.bracketed_paste {
            execute!(io::stdout(), EnableBracketedPaste)?;
            g.paste_enabled = true;
        }
        // Reserve the bottom 4 rows for the input footer (spinner + 3-line
        // input box). Everything else scrolls within the top region.
        if caps.tty {
            let (_, h) = crossterm::terminal::size().unwrap_or((80, 24));
            let bottom = (h as usize).saturating_sub(4);
            if bottom >= 5 {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                // Clear screen + home, set scroll region, disable autowrap.
                // Autowrap off prevents exact-fit box borders (width-w last
                // char) from triggering a scroll at the bottom-right cell.
                let _ = write!(out, "\x1b[2J\x1b[H\x1b[1;{}r\x1b[?7l", bottom);
                let _ = out.flush();
                g.scroll_region_set = true;
            }
        }
        Ok(g)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        use std::io::Write as _;
        if self.scroll_region_set {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            // Reset scroll region + re-enable autowrap + move cursor below
            // the footer so the next shell prompt doesn't stomp leftover chrome.
            let (_, h) = crossterm::terminal::size().unwrap_or((80, 24));
            let _ = write!(out, "\x1b[?7h\x1b[r\x1b[{};1H\r\n", h);
            let _ = out.flush();
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

    let ctx = LoopCtx {
        config,
        model_name,
        agent: agent_handle,
        working_dir,
        previous_dir: None,
        history,
        input_rx,
        commands: CommandRegistry::builtin(),
    };

    let result = run_loop(ctx, renderer.as_mut()).await;

    renderer.shutdown();
    drop(reader_handle); // thread exits on next channel send failure

    result
}
