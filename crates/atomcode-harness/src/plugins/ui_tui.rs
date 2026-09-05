//! A full-screen terminal front end.
//!
//! Deliberately hand-drawn rather than built on a widget framework: what is
//! being demonstrated is that a front end owning the whole screen, its own input
//! loop and its own redraw cycle needs nothing from the runtime that the
//! one-shot printer did not — the same registry, the same log, the same events.
//!
//! Input is delivered straight to the inbox, so typing during a turn steers it.
//! The transcript is rendered from the session log, so a redraw after a resize
//! shows exactly what the model saw, not a replay of what was printed.

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentStatus};
use crate::events::{AgentCreated, AgentInfo, SessionEventCommitted};
use crate::seams::{
    AgentLoopSvc, AgentsSvc, SessionSvc, SessionTitleSvc, UiSvc, UserInterface, UserQuestions,
    UserQuestionsSvc,
};
use crate::session::{Committed, SessionEvent};

/// One rendered line of the transcript.
#[derive(Clone)]
struct Line {
    text: String,
    style: Style,
}

#[derive(Clone, Copy, PartialEq)]
enum Style {
    User,
    Assistant,
    Tool,
    Error,
    Meta,
}

impl Style {
    fn ansi(self) -> &'static str {
        match self {
            Style::User => "\x1b[34m",
            Style::Assistant => "\x1b[0m",
            Style::Tool => "\x1b[36m",
            Style::Error => "\x1b[31m",
            Style::Meta => "\x1b[2m",
        }
    }
}

/// Project the session log into displayable lines.
///
/// Rendering from the log rather than accumulating print calls is what makes a
/// resize, a scroll or a reattach show the real conversation instead of a
/// transcript of side effects.
fn render(log: &crate::session::SessionLog) -> Vec<Line> {
    let mut lines = Vec::new();
    for logged in log.events() {
        match logged.event {
            SessionEvent::UserMessage { text, .. } => lines.push(Line {
                text: format!("› {text}"),
                style: Style::User,
            }),
            SessionEvent::Injected { text, origin, .. } => lines.push(Line {
                text: format!("[{origin:?}] {}", first_line(&text)),
                style: Style::Meta,
            }),
            SessionEvent::AssistantMessage {
                text, tool_calls, ..
            } => {
                if !text.is_empty() {
                    for line in text.lines() {
                        lines.push(Line {
                            text: line.to_string(),
                            style: Style::Assistant,
                        });
                    }
                }
                for call in tool_calls {
                    lines.push(Line {
                        text: format!("⚒ {} {}", call.name, truncate(&call.arguments, 90)),
                        style: Style::Tool,
                    });
                }
            }
            SessionEvent::ToolResultLogged {
                content, is_error, ..
            } => lines.push(Line {
                text: format!(
                    "  {} {}",
                    if is_error { "✗" } else { "✓" },
                    truncate(first_line(&content), 90)
                ),
                style: if is_error { Style::Error } else { Style::Tool },
            }),
            SessionEvent::TurnEnd { stop, error, .. } => lines.push(Line {
                text: match error {
                    Some(e) => format!("— {stop:?}: {e}"),
                    None => format!("— {stop:?}"),
                },
                style: Style::Meta,
            }),
            _ => {}
        }
    }
    lines
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// Restores the terminal even if the front end panics or errors out. A UI that
/// leaves a shell in raw mode on the way out is worse than no UI.
struct Screen;

impl Screen {
    fn enter() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            terminal::EnterAlternateScreen,
            cursor::Hide
        )?;
        Ok(Self)
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            std::io::stdout(),
            cursor::Show,
            terminal::LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

struct Tui;

/// What the input task tells the render loop.
enum Signal {
    Redraw,
    Delivered,
    Quit,
    Cancel,
}

#[async_trait]
impl UserInterface for Tui {
    fn describe(&self) -> String {
        "full-screen terminal".into()
    }

    async fn run(&self, ctx: &Context, initial: Option<String>) -> Result<(), String> {
        let driver = ctx.require::<AgentLoopSvc>().map_err(|e| e.to_string())?;
        let agents = ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
        let log = ctx.require::<SessionSvc>().map_err(|e| e.to_string())?;
        let agent = agents.create(ctx);
        ctx.emit::<AgentCreated>(&AgentInfo { id: agent.id() });

        let (signals_tx, mut signals) = mpsc::unbounded_channel::<Signal>();

        // Every committed event asks for a repaint. The loop coalesces them by
        // draining the channel, so a burst of stream chunks is one redraw.
        let redraw = signals_tx.clone();
        let _watch = ctx.on_emit::<SessionEventCommitted>(move |_: &Committed| {
            let _ = redraw.send(Signal::Redraw);
        });

        let _screen = Screen::enter().map_err(|e| format!("cannot take the terminal: {e}"))?;
        let input_agent = agent.clone();
        let input_tx = signals_tx.clone();
        let input = tokio::spawn(async move { read_keys(input_agent, input_tx).await });

        if let Some(text) = initial {
            agent.send(text);
        }

        let mut quit = false;
        while !quit {
            draw(&render(&log), &agent, ctx).await;

            if agent.inbox().has_waking_input() {
                driver.drive(&agent).await;
                continue;
            }

            match signals.recv().await {
                Some(Signal::Quit) | None => quit = true,
                Some(Signal::Cancel) => agent.cancel(),
                Some(Signal::Redraw) | Some(Signal::Delivered) => {}
            }
        }
        input.abort();
        Ok(())
    }
}

/// Owns the keyboard. Text goes to the inbox as it is entered, which is what
/// makes typing during a turn steer it rather than queue behind it.
async fn read_keys(agent: Arc<Agent>, signals: mpsc::UnboundedSender<Signal>) {
    let mut buffer = String::new();
    let mut events = EventStream::new();
    while let Some(Ok(event)) = events.next().await {
        let TermEvent::Key(key) = event else {
            let _ = signals.send(Signal::Redraw);
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let signal = match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => Signal::Cancel,
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => Signal::Quit,
            (KeyCode::Esc, _) => Signal::Cancel,
            (KeyCode::Enter, _) => {
                let text = buffer.trim().to_string();
                buffer.clear();
                if text.is_empty() {
                    Signal::Redraw
                } else if text == "/quit" || text == "/exit" {
                    Signal::Quit
                } else {
                    agent.send(text);
                    Signal::Delivered
                }
            }
            (KeyCode::Backspace, _) => {
                buffer.pop();
                Signal::Redraw
            }
            (KeyCode::Char(c), _) => {
                buffer.push(c);
                Signal::Redraw
            }
            _ => Signal::Redraw,
        };
        // The draw loop needs the in-progress line; hand it over by writing it
        // where the loop can see it. Kept simple: the buffer is echoed as part
        // of the signal-driven redraw below.
        CURRENT_INPUT.with_buffer(&buffer);
        if signals.send(signal).is_err() {
            break;
        }
    }
}

/// The in-progress input line, shared between the key reader and the painter.
struct CurrentInput(std::sync::Mutex<String>);

impl CurrentInput {
    fn with_buffer(&self, text: &str) {
        *self.0.lock().expect("input buffer poisoned") = text.to_string();
    }
    fn get(&self) -> String {
        self.0.lock().expect("input buffer poisoned").clone()
    }
}

static CURRENT_INPUT: std::sync::LazyLock<CurrentInput> =
    std::sync::LazyLock::new(|| CurrentInput(std::sync::Mutex::new(String::new())));

async fn draw(lines: &[Line], agent: &Agent, ctx: &Context) {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let rows = rows as usize;
    let body = rows.saturating_sub(3);

    let title = match (
        ctx.service::<SessionTitleSvc>(),
        ctx.service::<SessionSvc>(),
    ) {
        (Some(titler), Some(log)) => titler.title(&log).await,
        _ => None,
    };

    let mut out = String::new();
    out.push_str("\x1b[H\x1b[2J");
    out.push_str(&format!(
        "\x1b[7m {:<width$}\x1b[0m\r\n",
        format!(
            " atomcode harness · #{} {:?}{}",
            agent.id(),
            agent.status(),
            title.map(|t| format!(" · {t}")).unwrap_or_default()
        ),
        width = cols as usize
    ));

    // Bottom-anchored: the newest output is what a person is reading.
    let start = lines.len().saturating_sub(body);
    for line in &lines[start..] {
        out.push_str(line.style.ansi());
        out.push_str(&truncate(&line.text, cols as usize));
        out.push_str("\x1b[0m\r\n");
    }
    for _ in lines[start..].len()..body {
        out.push_str("\r\n");
    }

    out.push_str(&format!("\x1b[2m{}\x1b[0m\r\n", "─".repeat(cols as usize)));
    let hint = if agent.status() == AgentStatus::Working {
        "working — type to steer, ctrl-c to stop"
    } else {
        "ctrl-d to quit"
    };
    out.push_str(&format!(
        "› {}\x1b[2m  ({hint})\x1b[0m",
        CURRENT_INPUT.get()
    ));

    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(out.as_bytes());
    let _ = stdout.flush();
}

/// Asks in the alternate screen the front end already owns.
struct TuiQuestions;

#[async_trait]
impl UserQuestions for TuiQuestions {
    fn describe(&self) -> String {
        "the full-screen terminal".into()
    }
    async fn ask(&self, question: &str, options: &[String]) -> Option<String> {
        let mut stdout = std::io::stdout();
        let _ = write!(
            stdout,
            "\r\n\x1b[33m{question}\x1b[0m\r\n\x1b[33m[{}]\x1b[0m ",
            options.join("/")
        );
        let _ = stdout.flush();

        let mut events = EventStream::new();
        let mut answer = String::new();
        while let Some(Ok(TermEvent::Key(key))) = events.next().await {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Enter => break,
                KeyCode::Backspace => {
                    answer.pop();
                }
                KeyCode::Esc => return None,
                KeyCode::Char(c) => answer.push(c),
                _ => {}
            }
        }
        let answer = answer.trim().to_lowercase();
        options
            .iter()
            .find(|o| o.to_lowercase() == answer || o.to_lowercase().starts_with(&answer))
            .cloned()
    }
}

pub struct TuiUiPlugin;

#[async_trait]
impl Plugin for TuiUiPlugin {
    fn name(&self) -> &'static str {
        "ui-tui"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["agents", "agent-loop", "sessions"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["ui", "session-title"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["ui"]
    }
    fn description(&self) -> &'static str {
        "a full-screen terminal UI rendered from the session log"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UiSvc>(Arc::new(Tui))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The TUI's asker, as a row of its own.
///
/// Split from the front end for the same reason the plain terminal's is: one
/// row per capability. It is a *different* provider from the plain one because
/// a prompt has to be drawn by whoever owns the screen, and in an alternate
/// screen that is this.
pub struct TuiQuestionsPlugin;

#[async_trait]
impl Plugin for TuiQuestionsPlugin {
    fn name(&self) -> &'static str {
        "user-questions-tui"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["user-questions"]
    }
    fn description(&self) -> &'static str {
        "ask inside the full-screen terminal"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<UserQuestionsSvc>(Arc::new(TuiQuestions))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
