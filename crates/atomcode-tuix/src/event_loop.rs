// crates/atomcode-tuix/src/event_loop.rs
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use atomcode_core::agent::{AgentCommand, AgentEvent, AgentHandle, AgentPhase};
use atomcode_core::config::Config;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use tokio::sync::mpsc;

use crate::commands::{parse_slash_line, CommandRegistry};
use crate::input::history::History;
use crate::input::key_action::{classify, Action};
use crate::input::InputEvent;
use crate::render::{Renderer, UiLine};
use crate::state::{UiPhase, UiState};
use crate::think::ThinkStripper;

/// Bag of handles passed into the loop.
pub struct LoopCtx {
    pub config: Config,
    pub model_name: String,
    pub agent: AgentHandle,
    pub working_dir: PathBuf,
    pub previous_dir: Option<PathBuf>,
    pub history: History,
    pub input_rx: mpsc::UnboundedReceiver<InputEvent>,
    pub commands: CommandRegistry,
}

/// Line-edit buffer for input composition. Byte-indexed cursor.
struct Buffer {
    text: String,
    cursor: usize,
    history_idx: Option<usize>,
    stash: String,
}

impl Buffer {
    fn new() -> Self {
        Self { text: String::new(), cursor: 0, history_idx: None, stash: String::new() }
    }

    fn apply(&mut self, action: Action, history: &[String], commands: &CommandRegistry) -> BufferResult {
        match action {
            Action::Insert(c) => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                self.history_idx = None;
                BufferResult::Redraw
            }
            Action::Submit => {
                let line = self.text.trim().to_string();
                if line.is_empty() {
                    return BufferResult::Redraw;
                }
                BufferResult::Commit(line)
            }
            Action::InsertNewline => {
                self.text.insert(self.cursor, '\n');
                self.cursor += 1;
                BufferResult::Redraw
            }
            Action::Cancel => {
                if self.text.is_empty() {
                    BufferResult::Exit
                } else {
                    self.text.clear();
                    self.cursor = 0;
                    self.history_idx = None;
                    BufferResult::Redraw
                }
            }
            Action::ClearLine => {
                self.text.clear();
                self.cursor = 0;
                BufferResult::Redraw
            }
            Action::DeleteWordBackward => {
                let before = &self.text[..self.cursor];
                let trimmed = before.trim_end_matches(' ');
                let word_start = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                self.text.drain(word_start..self.cursor);
                self.cursor = word_start;
                BufferResult::Redraw
            }
            Action::DeleteToEnd => {
                let end = self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len());
                self.text.drain(self.cursor..end);
                BufferResult::Redraw
            }
            Action::Backspace => {
                if self.cursor > 0 {
                    let p = prev_boundary(&self.text, self.cursor);
                    self.text.drain(p..self.cursor);
                    self.cursor = p;
                }
                BufferResult::Redraw
            }
            Action::DeleteForward => {
                if self.cursor < self.text.len() {
                    let n = next_boundary(&self.text, self.cursor);
                    self.text.drain(self.cursor..n);
                }
                BufferResult::Redraw
            }
            Action::CursorLeft => {
                if self.cursor > 0 {
                    self.cursor = prev_boundary(&self.text, self.cursor);
                }
                BufferResult::Redraw
            }
            Action::CursorRight => {
                if self.cursor < self.text.len() {
                    self.cursor = next_boundary(&self.text, self.cursor);
                }
                BufferResult::Redraw
            }
            Action::LineStart => {
                let start = self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
                self.cursor = start;
                BufferResult::Redraw
            }
            Action::LineEnd => {
                let end = self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len());
                self.cursor = end;
                BufferResult::Redraw
            }
            Action::HistoryPrev => {
                if self.text.contains('\n') || history.is_empty() {
                    return BufferResult::Redraw;
                }
                let new_idx = match self.history_idx {
                    None => {
                        self.stash = self.text.clone();
                        Some(history.len() - 1)
                    }
                    Some(i) if i > 0 => Some(i - 1),
                    Some(i) => Some(i),
                };
                self.history_idx = new_idx;
                if let Some(i) = new_idx {
                    self.text = history[i].clone();
                    self.cursor = self.text.len();
                }
                BufferResult::Redraw
            }
            Action::HistoryNext => {
                if let Some(i) = self.history_idx {
                    if i + 1 < history.len() {
                        self.history_idx = Some(i + 1);
                        self.text = history[i + 1].clone();
                    } else {
                        self.history_idx = None;
                        self.text = self.stash.clone();
                    }
                    self.cursor = self.text.len();
                }
                BufferResult::Redraw
            }
            Action::Complete => {
                if self.text.starts_with('/') {
                    let prefix = &self.text[1..];
                    let matches = commands.matching_prefix(prefix);
                    if matches.len() == 1 {
                        self.text = format!("/{} ", matches[0].name);
                        self.cursor = self.text.len();
                    }
                    // Could also show a list for multiple matches; omit for v1.
                }
                BufferResult::Redraw
            }
            Action::NoOp => BufferResult::NoOp,
        }
    }

    fn cursor_cols(&self) -> usize {
        // display width of buf[..cursor]
        crate::width::display_width(&self.text[..self.cursor])
    }
}

enum BufferResult {
    NoOp,
    Redraw,
    Commit(String),
    Exit,
}

fn prev_boundary(s: &str, mut p: usize) -> usize {
    p -= 1;
    while !s.is_char_boundary(p) { p -= 1; }
    p
}

fn next_boundary(s: &str, mut p: usize) -> usize {
    p += 1;
    while p < s.len() && !s.is_char_boundary(p) { p += 1; }
    p
}

pub async fn run_loop(
    mut ctx: LoopCtx,
    renderer: &mut dyn Renderer,
) -> Result<()> {
    let mut state = UiState::new();
    let mut buf = Buffer::new();
    let mut think = ThinkStripper::new();

    // Draw welcome + initial prompt
    let dir_display = ctx.working_dir.to_string_lossy().to_string();
    let dir_display = if let Ok(home) = std::env::var("HOME") {
        dir_display.replacen(&home, "~", 1)
    } else { dir_display };
    renderer.render(UiLine::Welcome { model: ctx.model_name.clone(), working_dir: dir_display.clone() });
    renderer.render(UiLine::InputPrompt { buf: String::new(), cursor_cols: 0 });
    renderer.flush();

    let mut spinner_tick = tokio::time::interval(Duration::from_millis(100));
    spinner_tick.tick().await; // consume immediate tick

    // DEVIATION from plan:
    // 1. plan uses `SignalKind::terminal_stop()` which does not exist in tokio 1.x.
    //    Using `SignalKind::from_raw(libc::SIGTSTP)` instead.
    // 2. tokio::select! does not support #[cfg(...)] on individual arms, so signal
    //    handling is split into a cfg-gated loop variant below.
    #[cfg(unix)]
    let mut sigtstp = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP)
    )?;
    #[cfg(unix)]
    let mut sigcont = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::from_raw(libc::SIGCONT)
    )?;

    loop {
        #[cfg(unix)]
        tokio::select! {
            // ── Terminal input ──
            maybe = ctx.input_rx.recv() => {
                let Some(ev) = maybe else { break };
                handle_input(
                    ev, &mut state, &mut buf, &mut ctx, renderer,
                )?;
                // DEVIATION from plan: removed `ctx.input_rx.is_closed()` check —
                // UnboundedReceiver has no is_closed(); recv()->None already handles closure.
            }

            // ── Agent events ──
            maybe = ctx.agent.event_rx.recv(), if matches!(state.phase, UiPhase::Streaming) => {
                let Some(ev) = maybe else { break };
                handle_agent_event(ev, &mut state, &mut think, renderer);
                // if back to IDLE, redraw prompt
                if matches!(state.phase, UiPhase::Idle) {
                    renderer.render(UiLine::InputPrompt { buf: buf.text.clone(), cursor_cols: buf.cursor_cols() });
                    renderer.flush();
                }
            }

            // ── Spinner tick ──
            _ = spinner_tick.tick(), if matches!(state.phase, UiPhase::Streaming) => {
                let frame = state.tick_spinner();
                renderer.render(UiLine::Spinner { frame, label: state.spinner_label.clone() });
                renderer.flush();
            }

            // ── Suspend ──
            _ = sigtstp.recv() => {
                renderer.render(UiLine::ClearTransient);
                renderer.shutdown();
                state.on_suspend();
                // Disable raw mode before SIGSTOP so shell gets a sane terminal.
                let _ = crossterm::terminal::disable_raw_mode();
                unsafe { libc::raise(libc::SIGSTOP); }
            }

            // ── Resume ──
            _ = sigcont.recv() => {
                let _ = crossterm::terminal::enable_raw_mode();
                state.on_resume();
                match state.phase {
                    UiPhase::Streaming => {
                        renderer.render(UiLine::Spinner {
                            frame: state.tick_spinner(),
                            label: state.spinner_label.clone(),
                        });
                    }
                    _ => {
                        renderer.render(UiLine::InputPrompt {
                            buf: buf.text.clone(),
                            cursor_cols: buf.cursor_cols(),
                        });
                    }
                }
                renderer.flush();
            }
        }

        #[cfg(not(unix))]
        tokio::select! {
            // ── Terminal input ──
            maybe = ctx.input_rx.recv() => {
                let Some(ev) = maybe else { break };
                handle_input(
                    ev, &mut state, &mut buf, &mut ctx, renderer,
                )?;
            }

            // ── Agent events ──
            maybe = ctx.agent.event_rx.recv(), if matches!(state.phase, UiPhase::Streaming) => {
                let Some(ev) = maybe else { break };
                handle_agent_event(ev, &mut state, &mut think, renderer);
                if matches!(state.phase, UiPhase::Idle) {
                    renderer.render(UiLine::InputPrompt { buf: buf.text.clone(), cursor_cols: buf.cursor_cols() });
                    renderer.flush();
                }
            }

            // ── Spinner tick ──
            _ = spinner_tick.tick(), if matches!(state.phase, UiPhase::Streaming) => {
                let frame = state.tick_spinner();
                renderer.render(UiLine::Spinner { frame, label: state.spinner_label.clone() });
                renderer.flush();
            }
        }

        if matches!(state.phase, UiPhase::Idle) && ctx.agent.cmd_tx.is_closed() {
            break;
        }
    }

    let _ = ctx.history.save();
    Ok(())
}

fn handle_input(
    ev: InputEvent,
    state: &mut UiState,
    buf: &mut Buffer,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
) -> Result<()> {
    match ev {
        InputEvent::Paste(text) => {
            if matches!(state.phase, UiPhase::Idle) {
                buf.text.insert_str(buf.cursor, &text);
                buf.cursor += text.len();
                renderer.render(UiLine::InputPrompt { buf: buf.text.clone(), cursor_cols: buf.cursor_cols() });
                renderer.flush();
            }
        }
        InputEvent::Eof => {
            // Treat like Ctrl+C on empty buf
        }
        InputEvent::Key(KeyEvent { kind: KeyEventKind::Release, .. }) => {}
        InputEvent::Key(KeyEvent { code, modifiers, .. }) => {
            match state.phase {
                UiPhase::Idle => handle_idle_key(code, modifiers, state, buf, ctx, renderer)?,
                UiPhase::Streaming => handle_streaming_key(code, modifiers, ctx, renderer)?,
                UiPhase::Approval => handle_approval_key(code, state, ctx, renderer)?,
                UiPhase::Suspended => {} // ignored
            }
        }
    }
    Ok(())
}

fn handle_idle_key(
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    state: &mut UiState,
    buf: &mut Buffer,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
) -> Result<()> {
    let action = classify(code, modifiers);
    match buf.apply(action, ctx.history.entries(), &ctx.commands) {
        BufferResult::NoOp => {}
        BufferResult::Redraw => {
            renderer.render(UiLine::InputPrompt { buf: buf.text.clone(), cursor_cols: buf.cursor_cols() });
            renderer.flush();
        }
        BufferResult::Commit(line) => {
            renderer.render(UiLine::InputCommit);
            // Slash command?
            if let Some((cmd, arg)) = parse_slash_line(&line) {
                execute_slash_command(cmd, arg, state, ctx, renderer)?;
                buf.text.clear();
                buf.cursor = 0;
                if matches!(state.phase, UiPhase::Idle) {
                    renderer.render(UiLine::InputPrompt { buf: String::new(), cursor_cols: 0 });
                    renderer.flush();
                }
            } else {
                ctx.history.push(line.clone());
                buf.text.clear();
                buf.cursor = 0;
                renderer.render(UiLine::User(line.clone()));
                ctx.agent.cmd_tx.send(AgentCommand::SendMessage(line)).ok();
                state.on_submit();
            }
        }
        BufferResult::Exit => {
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
            // trigger break on next loop via closed channel
        }
    }
    Ok(())
}

fn handle_streaming_key(
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    ctx: &mut LoopCtx,
    _renderer: &mut dyn Renderer,
) -> Result<()> {
    if code == KeyCode::Char('c') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        ctx.agent.cmd_tx.send(AgentCommand::Cancel).ok();
    }
    Ok(())
}

fn handle_approval_key(
    code: KeyCode,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    _renderer: &mut dyn Renderer,
) -> Result<()> {
    let cmd = match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => AgentCommand::ApproveTool,
        KeyCode::Char('a') | KeyCode::Char('A') => AgentCommand::ApproveToolAlways,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => AgentCommand::DenyTool,
        _ => return Ok(()),
    };
    ctx.agent.cmd_tx.send(cmd).ok();
    state.on_approval_resolved();
    Ok(())
}

fn handle_agent_event(
    ev: AgentEvent,
    state: &mut UiState,
    think: &mut ThinkStripper,
    renderer: &mut dyn Renderer,
) {
    match ev {
        AgentEvent::TextDelta(text) => {
            let visible = think.feed(&text);
            if !visible.is_empty() {
                renderer.render(UiLine::AssistantText(visible));
                renderer.flush();
            }
        }
        AgentEvent::ToolCallStreaming { name, .. } => {
            state.on_tool_call_streaming(&name);
        }
        AgentEvent::ToolCallStarted { name, arguments, .. } => {
            // Close any in-flight assistant line
            renderer.render(UiLine::AssistantLineBreak);
            let detail = format_tool_detail(&name, &arguments);
            renderer.render(UiLine::ToolCall { name: name.clone(), detail });
            renderer.flush();
            state.on_tool_call_started(&name);
        }
        AgentEvent::ToolCallResult { output, success, .. } => {
            let summary = summarise(&output);
            renderer.render(UiLine::ToolResult { success, summary });
            // Diff lines
            for line in output.lines().take(120) {
                if let Some(rest) = line.strip_prefix("+ ") {
                    renderer.render(UiLine::DiffLine { added: true, text: rest.to_string() });
                } else if let Some(rest) = line.strip_prefix("- ") {
                    renderer.render(UiLine::DiffLine { added: false, text: rest.to_string() });
                }
            }
            renderer.flush();
        }
        AgentEvent::ApprovalNeeded { tool_name, call, .. } => {
            let detail = format_tool_detail(&tool_name, &call.arguments);
            renderer.render(UiLine::ApprovalPrompt { tool: tool_name.clone(), detail });
            renderer.flush();
            state.on_approval_needed(&tool_name);
        }
        AgentEvent::PhaseChange(AgentPhase::Thinking) => state.on_thinking(),
        AgentEvent::PhaseChange(AgentPhase::CallingTool(name)) => state.on_tool_call_streaming(&name),
        AgentEvent::PhaseChange(_) => {}
        AgentEvent::TurnComplete { .. } => {
            renderer.render(UiLine::AssistantLineBreak);
            renderer.render(UiLine::TurnComplete);
            renderer.flush();
            state.on_turn_complete();
        }
        AgentEvent::TurnCancelled { .. } => {
            renderer.render(UiLine::TurnCancelled);
            renderer.flush();
            state.on_turn_cancelled();
        }
        AgentEvent::Error(e) => {
            renderer.render(UiLine::Error(e));
            renderer.flush();
            state.on_error();
        }
        AgentEvent::TokenUsage(u) => {
            state.total_tokens += u.completion_tokens;
        }
        AgentEvent::ContextStats { .. }
        | AgentEvent::SubAgentProgress { .. }
        | AgentEvent::WorkingDirChanged(_) => {}
    }
}

fn execute_slash_command(
    cmd: &str,
    arg: &str,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
) -> Result<()> {
    match cmd {
        "quit" | "exit" => {
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
        }
        "help" => {
            renderer.render(UiLine::CommandOutput(ctx.commands.help_text()));
            renderer.flush();
        }
        "config" => {
            let txt = format!("  Provider: {}\n  Config: {}\n",
                ctx.config.default_provider,
                Config::default_path().display());
            renderer.render(UiLine::CommandOutput(txt));
            renderer.flush();
        }
        "clear" => {
            // Pure-append clear: use terminal's own clear sequence. OK because
            // scrollback is preserved by most terminals with \x1b[3J being optional.
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[2J\x1b[H");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let dir_display = ctx.working_dir.to_string_lossy().to_string();
            renderer.render(UiLine::Welcome { model: ctx.model_name.clone(), working_dir: dir_display });
            renderer.flush();
        }
        "model" => {
            renderer.render(UiLine::CommandOutput(format!("  Model: {}\n", ctx.model_name)));
            renderer.flush();
        }
        "status" => {
            let txt = format!(
                "  Model:  {}\n  Dir:    {}\n  Config: {}\n  Tokens: {}\n",
                ctx.model_name,
                ctx.working_dir.display(),
                Config::default_path().display(),
                state.total_tokens,
            );
            renderer.render(UiLine::CommandOutput(txt));
            renderer.flush();
        }
        "diff" => {
            let out = std::process::Command::new("git")
                .args(["diff", "--stat"])
                .current_dir(&ctx.working_dir)
                .output();
            match out {
                Ok(o) => {
                    let s = String::from_utf8_lossy(&o.stdout).to_string();
                    renderer.render(UiLine::CommandOutput(if s.is_empty() { "  (no changes)\n".into() } else { s }));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(format!("git diff failed: {}", e)));
                }
            }
            renderer.flush();
        }
        "undo" => {
            renderer.render(UiLine::CommandOutput("  Undo is not yet supported.\n".into()));
            renderer.flush();
        }
        "cost" => {
            renderer.render(UiLine::CommandOutput(format!("  Session tokens: {}\n", state.total_tokens)));
            renderer.flush();
        }
        "cd" => {
            let new_dir = resolve_cd(arg, &ctx.working_dir, ctx.previous_dir.as_deref());
            match new_dir {
                Ok(path) => {
                    ctx.previous_dir = Some(ctx.working_dir.clone());
                    ctx.working_dir = path.clone();
                    ctx.agent.cmd_tx.send(AgentCommand::ChangeDir(path.to_string_lossy().to_string())).ok();
                    renderer.render(UiLine::CommandOutput(format!("  Changed to: {}\n", path.display())));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(e));
                }
            }
            renderer.flush();
        }
        other => {
            renderer.render(UiLine::Error(format!("Unknown command: /{}", other)));
            renderer.flush();
        }
    }
    Ok(())
}

fn resolve_cd(arg: &str, cwd: &std::path::Path, prev: Option<&std::path::Path>) -> std::result::Result<PathBuf, String> {
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    let target = if arg.is_empty() {
        home.ok_or_else(|| "HOME not set".to_string())?
    } else if arg == "-" {
        prev.map(|p| p.to_path_buf()).ok_or_else(|| "No previous directory".to_string())?
    } else if let Some(rest) = arg.strip_prefix('~') {
        let home = home.ok_or_else(|| "HOME not set".to_string())?;
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if rest.is_empty() { home } else { home.join(rest) }
    } else {
        let p = PathBuf::from(arg);
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    let canon = target.canonicalize().map_err(|e| format!("{}: {}", target.display(), e))?;
    if !canon.is_dir() {
        return Err(format!("Not a directory: {}", canon.display()));
    }
    Ok(canon)
}

fn format_tool_detail(name: &str, args_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return String::new();
    };
    match name {
        "read_file" | "edit_file" | "write_file" | "create_file" => {
            v.get("file_path").and_then(|x| x.as_str())
                .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
                .unwrap_or_default()
        }
        "grep" => v.get("pattern").and_then(|x| x.as_str())
            .map(|p| crate::width::truncate_to_width(p, 30))
            .unwrap_or_default(),
        "bash" => v.get("command").and_then(|x| x.as_str())
            .map(|c| crate::width::truncate_to_width(c, 40))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn summarise(output: &str) -> String {
    let first = output.lines().next().unwrap_or("(no output)");
    let n = output.lines().count();
    let trimmed = crate::width::truncate_to_width(first, 80);
    if n > 1 {
        format!("{} ({} lines)", trimmed, n)
    } else {
        trimmed
    }
}
