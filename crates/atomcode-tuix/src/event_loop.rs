// crates/atomcode-tuix/src/event_loop.rs
use std::collections::VecDeque;
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
///
/// Large pasted blocks are folded into `[Pasted #N +M lines]` placeholders
/// stored in `text`; the original contents live in `pastes` and are
/// spliced back in when the line is submitted. This keeps the visible
/// input short (matching CC's paste UX) without truncating what the
/// agent actually sees.
struct Buffer {
    text: String,
    cursor: usize,
    history_idx: Option<usize>,
    stash: String,
    /// Placeholder index → original pasted text. Index 0 = paste #1.
    pastes: Vec<String>,
}

/// Minimum line count or char count for a paste to fold into a
/// placeholder. Smaller pastes are inserted inline — no point hiding
/// 3 lines behind a `[Pasted ...]` token.
const PASTE_FOLD_LINES: usize = 5;
const PASTE_FOLD_CHARS: usize = 400;

/// Fold `\r\n` and lone `\r` line endings to `\n`. Bracketed-paste
/// payloads from macOS Terminal / iTerm2 / Windows clipboard frequently
/// carry CR separators; leaving them in place makes `str::lines()` miss
/// line breaks and can confuse downstream JSON/prompt serialisation.
fn normalize_newlines(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\r', "\n")
}

impl Buffer {
    fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history_idx: None,
            stash: String::new(),
            pastes: Vec::new(),
        }
    }

    /// Insert a pasted block. Folds into a `[Pasted …]` placeholder if
    /// the block exceeds the fold threshold, keeping the visible input
    /// terse. Returns the placeholder that was inserted (or the raw
    /// text for small pastes) so callers can advance the cursor.
    ///
    /// Single-line long pastes (e.g. a 600-char URL) use a `{N} chars`
    /// summary — `+1 lines` would be misleading. Multi-line pastes use
    /// `+{M} lines` which is what people expect for code blocks / diffs.
    ///
    /// **Line-ending normalisation:** most terminals in bracketed paste
    /// mode emit `\r` (or `\r\n`) between lines rather than `\n`. Without
    /// normalising, a 20-line paste looks like one gigantic line to
    /// `str::lines()` (returning count 1), and downstream agents may
    /// mis-handle payloads that mix CR-only separators. We fold `\r\n`
    /// and lone `\r` to `\n` at ingress so both the placeholder summary
    /// and the expanded agent payload are in canonical form.
    fn insert_paste(&mut self, text: String) -> String {
        let text = normalize_newlines(&text);
        let line_count = text.lines().count().max(1);
        let char_count = text.chars().count();
        if line_count >= PASTE_FOLD_LINES || char_count >= PASTE_FOLD_CHARS {
            let id = self.pastes.len() + 1;
            let placeholder = if line_count <= 1 {
                format!("[Pasted #{} {} chars]", id, char_count)
            } else {
                format!("[Pasted #{} +{} lines]", id, line_count)
            };
            self.pastes.push(text);
            self.text.insert_str(self.cursor, &placeholder);
            self.cursor += placeholder.len();
            placeholder
        } else {
            let n = text.len();
            self.text.insert_str(self.cursor, &text);
            self.cursor += n;
            text
        }
    }

    /// Expand every `[Pasted #N +M lines]` token in `line` back to the
    /// original paste contents. Called at submit time — the agent gets
    /// the full pasted payload, while history/display keeps the compact
    /// form.
    fn expand_pastes(&self, line: &str) -> String {
        if self.pastes.is_empty() {
            return line.to_string();
        }
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(start) = rest.find("[Pasted #") {
            out.push_str(&rest[..start]);
            let tail = &rest[start..];
            if let Some(end) = tail.find(']') {
                // Parse id from "[Pasted #N +M lines]"
                let header = &tail[..=end];
                let id_part = header
                    .strip_prefix("[Pasted #")
                    .and_then(|s| s.split_whitespace().next());
                if let Some(id_str) = id_part {
                    if let Ok(id) = id_str.parse::<usize>() {
                        if id >= 1 && id <= self.pastes.len() {
                            out.push_str(&self.pastes[id - 1]);
                            rest = &tail[end + 1..];
                            continue;
                        }
                    }
                }
                // Malformed or out-of-range token — leave as-is.
                out.push_str(header);
                rest = &tail[end + 1..];
            } else {
                out.push_str(tail);
                rest = "";
                break;
            }
        }
        out.push_str(rest);
        out
    }

    fn clear_pastes(&mut self) {
        self.pastes.clear();
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
                    self.pastes.clear();
                    BufferResult::Redraw
                }
            }
            Action::ClearLine => {
                self.text.clear();
                self.cursor = 0;
                self.pastes.clear();
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

}

#[cfg(test)]
mod buffer_tests {
    use super::*;

    #[test]
    fn small_paste_inserts_inline() {
        let mut b = Buffer::new();
        b.insert_paste("hi\n".to_string());
        assert_eq!(b.text, "hi\n");
        assert!(b.pastes.is_empty(), "small paste should not fold");
    }

    #[test]
    fn large_paste_folds_into_placeholder() {
        let mut b = Buffer::new();
        let big = "line\n".repeat(10);
        b.insert_paste(big.clone());
        assert!(b.text.contains("[Pasted #1 +10 lines]"));
        assert_eq!(b.pastes, vec![big]);
    }

    #[test]
    fn expand_pastes_restores_original() {
        let mut b = Buffer::new();
        let big = "line\n".repeat(10);
        b.insert_paste(big.clone());
        let committed = b.text.clone();
        let expanded = b.expand_pastes(&committed);
        assert_eq!(expanded, big);
    }

    #[test]
    fn expand_pastes_is_noop_without_placeholders() {
        let b = Buffer::new();
        assert_eq!(b.expand_pastes("plain text"), "plain text");
    }

    #[test]
    fn paste_with_cr_separators_folds_correctly() {
        // Bracketed-paste often uses \r between lines (esp. macOS
        // Terminal.app). Without normalising, str::lines() sees one
        // gigantic line and the placeholder misreports "+1 lines".
        let mut b = Buffer::new();
        let cr_paste: String = (1..=20).map(|i| format!("line{}\r", i)).collect();
        b.insert_paste(cr_paste.clone());
        assert!(
            b.text.contains("+20 lines"),
            "expected 20-line placeholder, got: {}",
            b.text
        );
        // Original stored in pastes[0] is normalised (no \r).
        assert!(!b.pastes[0].contains('\r'));
        // Expanded body round-trips with \n separators.
        let expanded = b.expand_pastes(&b.text);
        assert_eq!(expanded.lines().count(), 20);
    }

    #[test]
    fn expand_handles_multiple_pastes_interleaved() {
        let mut b = Buffer::new();
        b.insert_paste("A\n".repeat(6));
        b.text.insert_str(b.cursor, " then ");
        b.cursor += 6;
        b.insert_paste("B\n".repeat(6));
        let line = b.text.clone();
        let out = b.expand_pastes(&line);
        assert!(out.contains("A\n"));
        assert!(out.contains(" then "));
        assert!(out.contains("B\n"));
        assert!(!out.contains("[Pasted"));
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
    let mut menu = MenuState::new();
    let mut model_picker: Option<ModelPicker> = None;
    let mut provider_wizard: Option<ProviderWizard> = None;
    // Messages the user submitted while a turn was already running.
    // Drained one-at-a-time from the head whenever the current turn
    // finishes (TurnComplete / TurnCancelled / error → Idle). Matches
    // CC's "type-ahead" behavior — you can queue the next prompt while
    // the model is still thinking and it fires automatically.
    let mut message_queue: VecDeque<String> = VecDeque::new();

    // Draw welcome + initial prompt
    let dir_display = ctx.working_dir.to_string_lossy().to_string();
    let dir_display = if let Ok(home) = std::env::var("HOME") {
        dir_display.replacen(&home, "~", 1)
    } else { dir_display };
    renderer.render(UiLine::Welcome { model: ctx.model_name.clone(), working_dir: dir_display.clone() });
    renderer.render(UiLine::InputPrompt {
        buf: String::new(),
        cursor_byte: 0,
        menu: None,
        status: build_status(&state, &ctx),
    });
    renderer.flush();

    // Spinner tick channel — a background task fires a tick every 100ms
    // into a bounded (cap 1) mpsc. The main loop recv's this in the
    // `tokio::select!` alongside the agent-event channel, so spinner
    // ticks compete fairly with agent events (both are channel reads
    // rather than a time-interval future that the runtime can skip
    // over when other branches are always ready).
    //
    // Cap 1 + try_send means if the main loop is mid-event and a tick
    // can't land in the channel, we silently drop it — no burst of
    // queued frames when control eventually returns. The post-event
    // pump (below) complements this by advancing the spinner as soon
    // as a slow handler finishes, even if the next scheduled tick is
    // still 50ms away.
    let (spin_tx, mut spin_rx) = tokio::sync::mpsc::channel::<()>(1);
    let spin_task = {
        let spin_tx = spin_tx.clone();
        tokio::spawn(async move {
            use tokio::sync::mpsc::error::TrySendError;
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await; // discard the immediate tick
            loop {
                interval.tick().await;
                match spin_tx.try_send(()) {
                    Ok(_) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Closed(_)) => break,
                }
            }
        })
    };
    drop(spin_tx); // only the task needs the sender

    // Last-draw timestamp — consulted by the post-event pump so we
    // don't redraw more often than every 100ms even when handlers
    // fire back-to-back.
    let mut last_spinner_draw = std::time::Instant::now();

    // call_id → (tool_name, detail). Populated on ToolCallStarted, consumed
    // on ToolCallResult so the result line can show "name(detail) — summary"
    // instead of just a bare "✓ summary" detached from its originating call.
    let mut pending_tools: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();

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
            // Biased ordering: spinner first so whenever a tick is
            // pending in spin_rx we draw it before racing with agent
            // events. Without `biased` tokio picks a ready branch
            // randomly, so under heavy agent traffic the spinner gets
            // chosen ~50% of the time its tick is ready, dropping the
            // effective frame rate to ~5 fps and looking like "frozen
            // then jumps".
            biased;

            // ── Spinner tick (from background task) ──
            Some(()) = spin_rx.recv(), if matches!(state.phase, UiPhase::Streaming) => {
                draw_spinner_now(&mut state, &buf, &ctx, renderer, message_queue.len());
                last_spinner_draw = std::time::Instant::now();
            }

            // ── Terminal input ──
            maybe = ctx.input_rx.recv() => {
                let Some(ev) = maybe else { break };
                handle_input(
                    ev, &mut state, &mut buf, &mut ctx, renderer, &mut menu,
                    &mut model_picker, &mut provider_wizard, &mut message_queue,
                )?;
            }

            // ── Agent events ──
            maybe = ctx.agent.event_rx.recv(), if matches!(state.phase, UiPhase::Streaming) => {
                let Some(ev) = maybe else { break };
                handle_agent_event(ev, &mut state, &mut think, renderer, &mut pending_tools);
                if matches!(state.phase, UiPhase::Streaming)
                    && last_spinner_draw.elapsed() >= Duration::from_millis(100)
                {
                    draw_spinner_now(&mut state, &buf, &ctx, renderer, message_queue.len());
                    last_spinner_draw = std::time::Instant::now();
                }
                if matches!(state.phase, UiPhase::Idle) {
                    // Turn just ended — drain the type-ahead queue.
                    // Pop the oldest queued message, echo as a User
                    // line, dispatch to the agent, and transition
                    // back to Streaming. Remaining queue entries
                    // fire in order on subsequent completions.
                    if let Some(queued) = message_queue.pop_front() {
                        renderer.render(UiLine::User(queued.clone()));
                        renderer.flush();
                        ctx.agent.cmd_tx.send(AgentCommand::SendMessage(queued)).ok();
                        state.on_submit();
                        draw_spinner_now(&mut state, &buf, &ctx, renderer, message_queue.len());
                    } else {
                        redraw_idle_plain(&buf, &state, &ctx, renderer);
                    }
                }
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
                        draw_spinner_now(&mut state, &buf, &ctx, renderer, message_queue.len());
                        last_spinner_draw = std::time::Instant::now();
                    }
                    _ => {
                        redraw_idle_plain(&buf, &state, &ctx, renderer);
                    }
                }
            }
        }

        #[cfg(not(unix))]
        tokio::select! {
            biased;

            // ── Spinner tick (from background task) ──
            Some(()) = spin_rx.recv(), if matches!(state.phase, UiPhase::Streaming) => {
                draw_spinner_now(&mut state, &buf, &ctx, renderer, message_queue.len());
                last_spinner_draw = std::time::Instant::now();
            }

            // ── Terminal input ──
            maybe = ctx.input_rx.recv() => {
                let Some(ev) = maybe else { break };
                handle_input(
                    ev, &mut state, &mut buf, &mut ctx, renderer, &mut menu,
                    &mut model_picker, &mut provider_wizard, &mut message_queue,
                )?;
            }

            // ── Agent events ──
            maybe = ctx.agent.event_rx.recv(), if matches!(state.phase, UiPhase::Streaming) => {
                let Some(ev) = maybe else { break };
                handle_agent_event(ev, &mut state, &mut think, renderer, &mut pending_tools);
                if matches!(state.phase, UiPhase::Streaming)
                    && last_spinner_draw.elapsed() >= Duration::from_millis(100)
                {
                    draw_spinner_now(&mut state, &buf, &ctx, renderer, message_queue.len());
                    last_spinner_draw = std::time::Instant::now();
                }
                if matches!(state.phase, UiPhase::Idle) {
                    // Turn just ended — drain the type-ahead queue.
                    // Pop the oldest queued message, echo as a User
                    // line, dispatch to the agent, and transition
                    // back to Streaming. Remaining queue entries
                    // fire in order on subsequent completions.
                    if let Some(queued) = message_queue.pop_front() {
                        renderer.render(UiLine::User(queued.clone()));
                        renderer.flush();
                        ctx.agent.cmd_tx.send(AgentCommand::SendMessage(queued)).ok();
                        state.on_submit();
                        draw_spinner_now(&mut state, &buf, &ctx, renderer, message_queue.len());
                    } else {
                        redraw_idle_plain(&buf, &state, &ctx, renderer);
                    }
                }
            }
        }

        if matches!(state.phase, UiPhase::Idle) && ctx.agent.cmd_tx.is_closed() {
            break;
        }
    }

    // Stop the background spinner task. Dropping `spin_rx` at scope
    // exit would let it self-terminate on the next try_send, but abort
    // is immediate and has no downside — the task holds no resources
    // beyond the interval timer.
    spin_task.abort();
    let _ = ctx.history.save();
    Ok(())
}

fn handle_input(
    ev: InputEvent,
    state: &mut UiState,
    buf: &mut Buffer,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    menu: &mut MenuState,
    model_picker: &mut Option<ModelPicker>,
    provider_wizard: &mut Option<ProviderWizard>,
    message_queue: &mut VecDeque<String>,
) -> Result<()> {
    match ev {
        InputEvent::Paste(text) => {
            // Allow pasting during Streaming too — it goes into the
            // type-ahead buffer just like keyboard input.
            if matches!(state.phase, UiPhase::Idle | UiPhase::Streaming)
                && model_picker.is_none()
                && provider_wizard.is_none()
            {
                buf.insert_paste(text);
                if matches!(state.phase, UiPhase::Streaming) {
                    draw_spinner_now(state, buf, ctx, renderer, message_queue.len());
                } else {
                    redraw_idle_plain(&buf, &state, &ctx, renderer);
                }
            }
        }
        InputEvent::Eof => {}
        InputEvent::Key(KeyEvent { kind: KeyEventKind::Release, .. }) => {}
        InputEvent::Key(KeyEvent { code, modifiers, .. }) => {
            // Wizard takes priority over model picker, which takes
            // priority over the normal phase handler.
            if provider_wizard.is_some() && matches!(state.phase, UiPhase::Idle) {
                handle_provider_wizard_key(
                    code, modifiers, buf, state, ctx, renderer, provider_wizard,
                )?;
                return Ok(());
            }
            if model_picker.is_some() && matches!(state.phase, UiPhase::Idle) {
                handle_model_picker_key(code, modifiers, buf, state, ctx, renderer, model_picker)?;
                return Ok(());
            }
            match state.phase {
                UiPhase::Idle => handle_idle_key(
                    code, modifiers, state, buf, ctx, renderer, menu,
                    model_picker, provider_wizard,
                )?,
                UiPhase::Streaming => handle_streaming_key(
                    code, modifiers, state, buf, ctx, renderer, message_queue,
                )?,
                UiPhase::Approval => handle_approval_key(code, state, ctx, renderer)?,
                UiPhase::Suspended => {}
            }
        }
    }
    Ok(())
}

/// Slash-command palette state. Active whenever buf starts with '/'.
pub struct MenuState {
    pub selected: usize,
}

impl MenuState {
    pub fn new() -> Self {
        Self { selected: 0 }
    }
}

/// Interactive model picker: activated by `/model`. Holds the provider
/// list sorted alphabetically with the current default first.
pub struct ModelPicker {
    pub providers: Vec<String>,
    pub selected: usize,
}

impl ModelPicker {
    pub fn open(config: &Config) -> Self {
        let mut providers: Vec<String> = config.providers.keys().cloned().collect();
        providers.sort();
        // Put the current default at top for quick re-confirmation.
        let cur = config.default_provider.clone();
        if let Some(idx) = providers.iter().position(|p| *p == cur) {
            providers.swap(0, idx);
        }
        Self {
            providers,
            selected: 0,
        }
    }
}

// ── ProviderWizard: /provider interactive flow ──
//
// A multi-step Q&A wizard for managing providers without leaving the
// scrollback. Driven by `ProviderWizard` state; input goes into the
// normal buf and Enter advances to the next step (or commits). At any
// point Esc cancels and returns to Idle.
//
// Each step appends a `CommandOutput` prompt to scrollback ("Provider
// name?"), the user types + Enter, the answer is echoed back as another
// scrollback line, and the next step's prompt appears. Persistent
// menus (MainMenu / DeletePick / SetDefaultPick) re-use the existing
// `MenuPayload` footer palette.

pub enum ProviderWizard {
    /// Initial picker: Add / Edit / Delete / Set Default.
    MainMenu { selected: usize },
    /// Sequential `Add` prompts. `draft` accumulates answered fields.
    Add { step: WizardStep, draft: DraftProvider },
    /// Pick which provider to edit.
    EditPick {
        providers: Vec<String>,
        selected: usize,
    },
    /// Editing a specific provider; same flow as `Add` but prompts show
    /// the existing value as a hint and an empty Enter keeps it.
    Edit {
        target: String,
        step: WizardStep,
        draft: DraftProvider,
    },
    /// Pick which provider to delete.
    DeletePick {
        providers: Vec<String>,
        selected: usize,
    },
    /// Final y/N confirmation before a delete actually lands.
    DeleteConfirm { target: String },
    /// Pick which provider to make default.
    SetDefaultPick {
        providers: Vec<String>,
        selected: usize,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum WizardStep {
    Name,
    ProviderType,
    ApiKey,
    Model,
}

#[derive(Clone, Debug, Default)]
pub struct DraftProvider {
    pub name: String,
    pub provider_type: String,
    pub api_key: String,
    pub model: String,
}

impl DraftProvider {
    /// Merge this draft onto `base` — empty fields leave `base` untouched.
    /// Used by Edit so an empty Enter at a prompt keeps the existing value.
    fn apply_onto(&self, base: &mut atomcode_core::config::provider::ProviderConfig) {
        if !self.provider_type.is_empty() {
            base.provider_type = self.provider_type.clone();
        }
        if !self.api_key.is_empty() {
            base.api_key = Some(self.api_key.clone());
        }
        if !self.model.is_empty() {
            base.model = self.model.clone();
        }
    }

    fn into_config(self) -> atomcode_core::config::provider::ProviderConfig {
        use atomcode_core::config::provider::{default_context_window_for, ProviderConfig};
        let provider_type = self.provider_type.clone();
        ProviderConfig {
            provider_type: provider_type.clone(),
            api_key: if self.api_key.is_empty() {
                None
            } else {
                Some(self.api_key)
            },
            model: self.model,
            base_url: None,
            system_prompt: None,
            user_agent: None,
            context_window: default_context_window_for(&provider_type),
            max_tokens: None,
            ephemeral: false,
        }
    }
}

/// Filter the command registry by the buf's prefix after '/'. Returns the
/// (name, desc) pairs matching, or None if menu shouldn't show (buf doesn't
/// start with '/' or has whitespace, meaning the user has moved on to args).
fn build_menu_items(buf: &str, commands: &CommandRegistry) -> Option<Vec<(String, String)>> {
    if !buf.starts_with('/') {
        return None;
    }
    let rest = &buf[1..];
    // Once a space appears (user is typing args), stop showing menu.
    if rest.contains(char::is_whitespace) {
        return None;
    }
    let matches: Vec<(String, String)> = commands
        .matching_prefix(rest)
        .into_iter()
        .map(|c| (c.name.to_string(), c.desc.to_string()))
        .collect();
    if matches.is_empty() {
        None
    } else {
        Some(matches)
    }
}

fn handle_idle_key(
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    state: &mut UiState,
    buf: &mut Buffer,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    menu: &mut MenuState,
    model_picker: &mut Option<ModelPicker>,
    provider_wizard: &mut Option<ProviderWizard>,
) -> Result<()> {
    // If the menu is active (buf starts with '/'), intercept navigation keys.
    let menu_items = build_menu_items(&buf.text, &ctx.commands);
    if let Some(items) = &menu_items {
        // Clamp selection in range.
        if menu.selected >= items.len() {
            menu.selected = items.len() - 1;
        }
        match (code, modifiers) {
            (KeyCode::Up, _) => {
                menu.selected = menu.selected.saturating_sub(1);
                redraw_with_menu(buf, items, menu.selected, state, ctx, renderer);
                return Ok(());
            }
            (KeyCode::Down, _) => {
                if menu.selected + 1 < items.len() {
                    menu.selected += 1;
                }
                redraw_with_menu(buf, items, menu.selected, state, ctx, renderer);
                return Ok(());
            }
            (KeyCode::Enter, m) if !m.contains(crossterm::event::KeyModifiers::SHIFT) => {
                // Accept the highlighted command as the committed line.
                let name = items[menu.selected].0.clone();
                let committed = format!("/{}", name);
                menu.selected = 0;
                // Simulate a commit path.
                renderer.render(UiLine::ClearTransient);
                renderer.render(UiLine::User(committed.clone()));
                buf.text.clear();
                buf.cursor = 0;
                if let Some((cmd, arg)) = parse_slash_line(&committed) {
                    execute_slash_command(cmd, arg, state, ctx, renderer, model_picker, provider_wizard)?;
                    if matches!(state.phase, UiPhase::Idle) {
                        redraw_idle(buf, state, ctx, model_picker, renderer);
                    }
                }
                return Ok(());
            }
            (KeyCode::Esc, _) => {
                // Close menu by clearing buffer.
                buf.text.clear();
                buf.cursor = 0;
                menu.selected = 0;
                redraw_idle_plain(buf, state, ctx, renderer);
                return Ok(());
            }
            _ => {} // fall through to buffer edits
        }
    }

    let action = classify(code, modifiers);
    match buf.apply(action, ctx.history.entries(), &ctx.commands) {
        BufferResult::NoOp => {}
        BufferResult::Redraw => {
            // Rebuild menu after buf change.
            let items = build_menu_items(&buf.text, &ctx.commands);
            if let Some(items) = items {
                if menu.selected >= items.len() {
                    menu.selected = 0;
                }
                redraw_with_menu(buf, &items, menu.selected, state, ctx, renderer);
            } else {
                menu.selected = 0;
                redraw_idle_plain(buf, state, ctx, renderer);
            }
        }
        BufferResult::Commit(line) => {
            // Expand paste placeholders so the agent sees full content
            // while the echoed user line and history stay compact.
            let expanded = buf.expand_pastes(&line);
            renderer.render(UiLine::ClearTransient);
            renderer.render(UiLine::User(line.clone()));
            buf.text.clear();
            buf.cursor = 0;
            buf.clear_pastes();
            menu.selected = 0;
            if let Some((cmd, arg)) = parse_slash_line(&line) {
                execute_slash_command(cmd, arg, state, ctx, renderer, model_picker, provider_wizard)?;
                if matches!(state.phase, UiPhase::Idle) {
                    redraw_idle(buf, state, ctx, model_picker, renderer);
                }
            } else {
                ctx.history.push(line.clone());
                ctx.agent.cmd_tx.send(AgentCommand::SendMessage(expanded)).ok();
                state.on_submit();
            }
        }
        BufferResult::Exit => {
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
        }
    }
    Ok(())
}

fn redraw_with_menu(
    buf: &Buffer,
    items: &[(String, String)],
    selected: usize,
    state: &UiState,
    ctx: &LoopCtx,
    renderer: &mut dyn Renderer,
) {
    let payload = crate::render::MenuPayload {
        items: items.to_vec(),
        selected,
    };
    renderer.render(UiLine::InputPrompt {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        menu: Some(payload),
        status: build_status(state, ctx),
    });
    renderer.flush();
}

/// Idle prompt without any menu/picker — used by the common
/// "Redraw" path and the post-event-loop fallback after an agent
/// event returns the UI to Idle.
fn redraw_idle_plain(
    buf: &Buffer,
    state: &UiState,
    ctx: &LoopCtx,
    renderer: &mut dyn Renderer,
) {
    renderer.render(UiLine::InputPrompt {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        menu: None,
        status: build_status(state, ctx),
    });
    renderer.flush();
}

/// Redraw the idle footer, showing the model picker if active.
fn redraw_idle(
    buf: &Buffer,
    state: &UiState,
    ctx: &LoopCtx,
    model_picker: &Option<ModelPicker>,
    renderer: &mut dyn Renderer,
) {
    let payload = model_picker.as_ref().map(|p| {
        let items: Vec<(String, String)> = p
            .providers
            .iter()
            .map(|name| {
                let desc = ctx
                    .config
                    .providers
                    .get(name)
                    .map(|c| format!("{} · {}", c.provider_type, c.model))
                    .unwrap_or_default();
                (name.clone(), desc)
            })
            .collect();
        crate::render::MenuPayload {
            items,
            selected: p.selected,
        }
    });
    renderer.render(UiLine::InputPrompt {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        menu: payload,
        status: build_status(state, ctx),
    });
    renderer.flush();
}

/// Redraw the footer with the wizard's current menu/prompt. Text-input
/// steps show the normal input box; picker steps show an overlay menu
/// built from wizard state.
fn redraw_wizard(
    buf: &Buffer,
    state: &UiState,
    ctx: &LoopCtx,
    wizard: &ProviderWizard,
    renderer: &mut dyn Renderer,
) {
    let menu = match wizard {
        ProviderWizard::MainMenu { selected } => Some(crate::render::MenuPayload {
            items: vec![
                ("add".into(), "Add a new provider".into()),
                ("edit".into(), "Edit an existing provider".into()),
                ("delete".into(), "Remove a provider".into()),
                ("set-default".into(), "Switch the default provider".into()),
            ],
            selected: *selected,
        }),
        ProviderWizard::EditPick { providers, selected }
        | ProviderWizard::DeletePick { providers, selected }
        | ProviderWizard::SetDefaultPick { providers, selected } => {
            let items: Vec<(String, String)> = providers
                .iter()
                .map(|name| {
                    let desc = ctx
                        .config
                        .providers
                        .get(name)
                        .map(|c| format!("{} · {}", c.provider_type, c.model))
                        .unwrap_or_default();
                    (name.clone(), desc)
                })
                .collect();
            Some(crate::render::MenuPayload {
                items,
                selected: *selected,
            })
        }
        // Q&A steps: plain input box, no overlay menu.
        ProviderWizard::Add { .. }
        | ProviderWizard::Edit { .. }
        | ProviderWizard::DeleteConfirm { .. } => None,
    };
    renderer.render(UiLine::InputPrompt {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        menu,
        status: build_status(state, ctx),
    });
    renderer.flush();
}

/// Push a prompt line into scrollback. Steps share the same "tool-line"
/// styling — a muted line with two-space indent — so the Q&A reads like
/// the rest of the conversation rather than a modal popup.
fn wizard_push(renderer: &mut dyn Renderer, text: &str) {
    renderer.render(UiLine::CommandOutput(format!("  {}\n", text)));
    renderer.flush();
}

/// Prompt string for the given wizard step; includes the existing value
/// as a hint in Edit mode so the user sees what empty-Enter will keep.
fn wizard_step_prompt(
    step: WizardStep,
    existing: Option<&atomcode_core::config::provider::ProviderConfig>,
) -> String {
    match (step, existing) {
        (WizardStep::Name, _) => "Provider name?".into(),
        (WizardStep::ProviderType, None) => "Type? (openai / claude / ollama)".into(),
        (WizardStep::ProviderType, Some(p)) => {
            format!("Type? [{}] (openai / claude / ollama, blank to keep)", p.provider_type)
        }
        (WizardStep::ApiKey, None) => "API key? (blank to leave unset)".into(),
        (WizardStep::ApiKey, Some(p)) => {
            let hint = if p.api_key.is_some() { "set — blank to keep" } else { "unset" };
            format!("API key? [{}]", hint)
        }
        (WizardStep::Model, None) => "Model?".into(),
        (WizardStep::Model, Some(p)) => format!("Model? [{}] (blank to keep)", p.model),
    }
}

/// Push the prompt for this step into scrollback + redraw footer.
fn show_step_prompt(
    step: WizardStep,
    existing: Option<&atomcode_core::config::provider::ProviderConfig>,
    buf: &Buffer,
    state: &UiState,
    ctx: &LoopCtx,
    wizard: &ProviderWizard,
    renderer: &mut dyn Renderer,
) {
    wizard_push(renderer, &wizard_step_prompt(step, existing));
    redraw_wizard(buf, state, ctx, wizard, renderer);
}

/// Validate and advance the "Add" sub-flow. Returns the next state, or
/// None when the wizard has committed / cancelled (caller clears).
fn advance_add(
    draft: &mut DraftProvider,
    step: WizardStep,
    answer: &str,
    renderer: &mut dyn Renderer,
) -> Option<WizardStep> {
    let ans = answer.trim();
    match step {
        WizardStep::Name => {
            if ans.is_empty() {
                wizard_push(renderer, "Name cannot be empty.");
                return Some(WizardStep::Name);
            }
            draft.name = ans.to_string();
            Some(WizardStep::ProviderType)
        }
        WizardStep::ProviderType => {
            if !["openai", "claude", "ollama"].contains(&ans) {
                wizard_push(renderer, "Unknown type. Choose openai / claude / ollama.");
                return Some(WizardStep::ProviderType);
            }
            draft.provider_type = ans.to_string();
            Some(WizardStep::ApiKey)
        }
        WizardStep::ApiKey => {
            draft.api_key = ans.to_string();
            Some(WizardStep::Model)
        }
        WizardStep::Model => {
            if ans.is_empty() {
                wizard_push(renderer, "Model cannot be empty.");
                return Some(WizardStep::Model);
            }
            draft.model = ans.to_string();
            None // signal: ready to commit
        }
    }
}

/// Validate and advance the "Edit" sub-flow. Empty answers preserve
/// the existing value, so the caller needs `existing` to know what
/// that value is.
fn advance_edit(
    draft: &mut DraftProvider,
    step: WizardStep,
    answer: &str,
    renderer: &mut dyn Renderer,
) -> Option<WizardStep> {
    let ans = answer.trim();
    match step {
        WizardStep::Name => {
            // Name isn't editable (it's the key into the provider map).
            Some(WizardStep::ProviderType)
        }
        WizardStep::ProviderType => {
            if !ans.is_empty() && !["openai", "claude", "ollama"].contains(&ans) {
                wizard_push(renderer, "Unknown type. Choose openai / claude / ollama or leave blank.");
                return Some(WizardStep::ProviderType);
            }
            draft.provider_type = ans.to_string();
            Some(WizardStep::ApiKey)
        }
        WizardStep::ApiKey => {
            draft.api_key = ans.to_string();
            Some(WizardStep::Model)
        }
        WizardStep::Model => {
            draft.model = ans.to_string();
            None
        }
    }
}

/// Persist config changes and notify the daemon to pick them up.
fn save_and_reload(ctx: &mut LoopCtx, renderer: &mut dyn Renderer) {
    let path = Config::default_path();
    match ctx.config.save(&path) {
        Ok(()) => {
            let _ = ctx
                .agent
                .cmd_tx
                .send(AgentCommand::ReloadConfig(ctx.config.clone()));
        }
        Err(e) => {
            renderer.render(UiLine::Error(format!("config save failed: {}", e)));
            renderer.flush();
        }
    }
}

fn handle_provider_wizard_key(
    code: KeyCode,
    _modifiers: crossterm::event::KeyModifiers,
    buf: &mut Buffer,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    wizard: &mut Option<ProviderWizard>,
) -> Result<()> {
    // Esc always cancels at any point.
    if matches!(code, KeyCode::Esc) {
        *wizard = None;
        buf.text.clear();
        buf.cursor = 0;
        wizard_push(renderer, "(cancelled)");
        redraw_idle_plain(buf, state, ctx, renderer);
        return Ok(());
    }

    let current = wizard.take().expect("guarded by caller");
    match current {
        // ── Menu states: Up / Down / Enter navigate; others ignored. ──
        ProviderWizard::MainMenu { mut selected } => {
            const ITEMS: [&str; 4] = ["add", "edit", "delete", "set-default"];
            match code {
                KeyCode::Up => {
                    selected = selected.saturating_sub(1);
                    *wizard = Some(ProviderWizard::MainMenu { selected });
                }
                KeyCode::Down => {
                    if selected + 1 < ITEMS.len() {
                        selected += 1;
                    }
                    *wizard = Some(ProviderWizard::MainMenu { selected });
                }
                KeyCode::Enter => {
                    let providers: Vec<String> = {
                        let mut v: Vec<String> = ctx.config.providers.keys().cloned().collect();
                        v.sort();
                        v
                    };
                    match ITEMS[selected] {
                        "add" => {
                            let new = ProviderWizard::Add {
                                step: WizardStep::Name,
                                draft: DraftProvider::default(),
                            };
                            show_step_prompt(WizardStep::Name, None, buf, state, ctx, &new, renderer);
                            *wizard = Some(new);
                        }
                        "edit" | "delete" | "set-default" if providers.is_empty() => {
                            wizard_push(renderer, "No providers configured yet.");
                            redraw_idle_plain(buf, state, ctx, renderer);
                            // wizard stays None → back to normal Idle.
                        }
                        "edit" => {
                            let new = ProviderWizard::EditPick { providers, selected: 0 };
                            redraw_wizard(buf, state, ctx, &new, renderer);
                            *wizard = Some(new);
                        }
                        "delete" => {
                            let new = ProviderWizard::DeletePick { providers, selected: 0 };
                            redraw_wizard(buf, state, ctx, &new, renderer);
                            *wizard = Some(new);
                        }
                        "set-default" => {
                            let new = ProviderWizard::SetDefaultPick { providers, selected: 0 };
                            redraw_wizard(buf, state, ctx, &new, renderer);
                            *wizard = Some(new);
                        }
                        _ => {}
                    }
                }
                _ => {
                    *wizard = Some(ProviderWizard::MainMenu { selected });
                }
            }
            if let Some(w) = wizard.as_ref() {
                redraw_wizard(buf, state, ctx, w, renderer);
            }
        }

        // ── Picker states share Up/Down/Enter logic. ──
        ProviderWizard::EditPick { providers, mut selected } => {
            match code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < providers.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let target = providers[selected].clone();
                    let existing = ctx.config.providers.get(&target).cloned();
                    let new = ProviderWizard::Edit {
                        target: target.clone(),
                        step: WizardStep::ProviderType, // skip Name (immutable)
                        draft: DraftProvider::default(),
                    };
                    show_step_prompt(
                        WizardStep::ProviderType,
                        existing.as_ref(),
                        buf,
                        state,
                        ctx,
                        &new,
                        renderer,
                    );
                    *wizard = Some(new);
                    return Ok(());
                }
                _ => {}
            }
            let new = ProviderWizard::EditPick { providers, selected };
            redraw_wizard(buf, state, ctx, &new, renderer);
            *wizard = Some(new);
        }

        ProviderWizard::DeletePick { providers, mut selected } => {
            match code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < providers.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let target = providers[selected].clone();
                    wizard_push(renderer, &format!("Delete \"{}\"? [y/N]", target));
                    let new = ProviderWizard::DeleteConfirm { target };
                    redraw_wizard(buf, state, ctx, &new, renderer);
                    *wizard = Some(new);
                    return Ok(());
                }
                _ => {}
            }
            let new = ProviderWizard::DeletePick { providers, selected };
            redraw_wizard(buf, state, ctx, &new, renderer);
            *wizard = Some(new);
        }

        ProviderWizard::SetDefaultPick { providers, mut selected } => {
            match code {
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < providers.len() {
                        selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let chosen = providers[selected].clone();
                    ctx.config.default_provider = chosen.clone();
                    if let Some(p) = ctx.config.providers.get(&chosen) {
                        ctx.model_name = p.model.clone();
                    }
                    save_and_reload(ctx, renderer);
                    wizard_push(renderer, &format!("Default set to {}.", chosen));
                    redraw_idle_plain(buf, state, ctx, renderer);
                    return Ok(());
                }
                _ => {}
            }
            let new = ProviderWizard::SetDefaultPick { providers, selected };
            redraw_wizard(buf, state, ctx, &new, renderer);
            *wizard = Some(new);
        }

        ProviderWizard::DeleteConfirm { target } => {
            match code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    ctx.config.providers.remove(&target);
                    // If we just dropped the default, fall back to any
                    // remaining provider or blank.
                    if ctx.config.default_provider == target {
                        ctx.config.default_provider = ctx
                            .config
                            .providers
                            .keys()
                            .next()
                            .cloned()
                            .unwrap_or_default();
                    }
                    save_and_reload(ctx, renderer);
                    wizard_push(renderer, &format!("Removed \"{}\".", target));
                }
                _ => {
                    wizard_push(renderer, "(kept)");
                }
            }
            redraw_idle_plain(buf, state, ctx, renderer);
        }

        // ── Text-input states: Enter submits, chars edit buf, others pass through Buffer. ──
        ProviderWizard::Add { step, mut draft } => {
            if matches!(code, KeyCode::Enter) {
                let answer = buf.text.clone();
                wizard_push(renderer, &format!("  ↳ {}", answer));
                buf.text.clear();
                buf.cursor = 0;
                match advance_add(&mut draft, step, &answer, renderer) {
                    Some(next) => {
                        let new = ProviderWizard::Add { step: next, draft };
                        show_step_prompt(next, None, buf, state, ctx, &new, renderer);
                        *wizard = Some(new);
                    }
                    None => {
                        // All fields gathered — commit.
                        let name = draft.name.clone();
                        let cfg = draft.into_config();
                        ctx.config.providers.insert(name.clone(), cfg);
                        // If nothing was default, promote the newcomer.
                        if ctx.config.default_provider.is_empty() {
                            ctx.config.default_provider = name.clone();
                        }
                        save_and_reload(ctx, renderer);
                        wizard_push(renderer, &format!("Added provider \"{}\".", name));
                        redraw_idle_plain(buf, state, ctx, renderer);
                    }
                }
                return Ok(());
            }
            // Forward other keys to the buffer so typing / editing works.
            forward_to_buffer(code, _modifiers, buf, ctx);
            let restored = ProviderWizard::Add { step, draft };
            redraw_wizard(buf, state, ctx, &restored, renderer);
            *wizard = Some(restored);
        }

        ProviderWizard::Edit { target, step, mut draft } => {
            if matches!(code, KeyCode::Enter) {
                let answer = buf.text.clone();
                wizard_push(renderer, &format!("  ↳ {}", if answer.is_empty() { "(keep)" } else { answer.as_str() }));
                buf.text.clear();
                buf.cursor = 0;
                match advance_edit(&mut draft, step, &answer, renderer) {
                    Some(next) => {
                        let existing = ctx.config.providers.get(&target).cloned();
                        let new = ProviderWizard::Edit {
                            target: target.clone(),
                            step: next,
                            draft,
                        };
                        show_step_prompt(next, existing.as_ref(), buf, state, ctx, &new, renderer);
                        *wizard = Some(new);
                    }
                    None => {
                        // Commit edit: merge draft onto existing provider.
                        if let Some(existing) = ctx.config.providers.get_mut(&target) {
                            draft.apply_onto(existing);
                        }
                        save_and_reload(ctx, renderer);
                        wizard_push(renderer, &format!("Updated \"{}\".", target));
                        redraw_idle_plain(buf, state, ctx, renderer);
                    }
                }
                return Ok(());
            }
            forward_to_buffer(code, _modifiers, buf, ctx);
            let restored = ProviderWizard::Edit { target, step, draft };
            redraw_wizard(buf, state, ctx, &restored, renderer);
            *wizard = Some(restored);
        }
    }

    Ok(())
}

/// Route a keystroke into `Buffer::apply` so text-input wizard steps
/// support the usual editing shortcuts (Backspace / Left / Right / etc).
fn forward_to_buffer(
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    buf: &mut Buffer,
    ctx: &LoopCtx,
) {
    let action = classify(code, modifiers);
    let _ = buf.apply(action, ctx.history.entries(), &ctx.commands);
}

/// Modal key handler while ModelPicker is active.
fn handle_model_picker_key(
    code: KeyCode,
    _modifiers: crossterm::event::KeyModifiers,
    buf: &mut Buffer,
    state: &UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    model_picker: &mut Option<ModelPicker>,
) -> Result<()> {
    let Some(picker) = model_picker.as_mut() else { return Ok(()); };
    match code {
        KeyCode::Up => {
            picker.selected = picker.selected.saturating_sub(1);
            redraw_idle(buf, state, ctx, model_picker, renderer);
        }
        KeyCode::Down => {
            let max = model_picker.as_ref().unwrap().providers.len().saturating_sub(1);
            let picker = model_picker.as_mut().unwrap();
            if picker.selected < max {
                picker.selected += 1;
            }
            redraw_idle(buf, state, ctx, model_picker, renderer);
        }
        KeyCode::Enter => {
            let chosen = model_picker.as_ref().unwrap().providers[model_picker.as_ref().unwrap().selected].clone();
            // Build new config with updated default_provider.
            let mut new_config = ctx.config.clone();
            new_config.default_provider = chosen.clone();
            // Tell the agent to switch.
            ctx.agent
                .cmd_tx
                .send(AgentCommand::ReloadConfig(new_config.clone()))
                .ok();
            // Update local state for the display.
            let display = new_config
                .providers
                .get(&chosen)
                .map(|p| p.model.clone())
                .unwrap_or_else(|| chosen.clone());
            ctx.config = new_config;
            ctx.model_name = display.clone();
            *model_picker = None;
            renderer.render(UiLine::CommandOutput(format!(
                "  Switched to {} · {}\n",
                chosen, display
            )));
            renderer.flush();
            redraw_idle(buf, state, ctx, model_picker, renderer);
        }
        KeyCode::Esc => {
            *model_picker = None;
            redraw_idle(buf, state, ctx, model_picker, renderer);
        }
        _ => {}
    }
    Ok(())
}

fn handle_streaming_key(
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    state: &mut UiState,
    buf: &mut Buffer,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    message_queue: &mut VecDeque<String>,
) -> Result<()> {
    // Ctrl+C always cancels the running turn — highest priority so
    // users have a reliable escape hatch even mid-edit.
    if code == KeyCode::Char('c') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        ctx.agent.cmd_tx.send(AgentCommand::Cancel).ok();
        return Ok(());
    }

    let action = classify(code, modifiers);
    match buf.apply(action, ctx.history.entries(), &ctx.commands) {
        BufferResult::NoOp => {}
        BufferResult::Redraw => {
            draw_spinner_now(state, buf, ctx, renderer, message_queue.len());
        }
        BufferResult::Commit(line) => {
            // Slash commands are not queued — they need ctx access
            // that only makes sense between turns. Show a hint and
            // leave the buf alone.
            if line.starts_with('/') {
                renderer.render(UiLine::CommandOutput(
                    "  (slash commands are disabled while a turn is running)\n".into(),
                ));
                renderer.flush();
                buf.text.clear();
                buf.cursor = 0;
                draw_spinner_now(state, buf, ctx, renderer, message_queue.len());
                return Ok(());
            }
            // Expand any paste placeholders — agent sees full payload,
            // scrollback echo stays compact.
            let expanded = buf.expand_pastes(&line);
            ctx.history.push(line.clone());
            message_queue.push_back(expanded);
            buf.text.clear();
            buf.cursor = 0;
            buf.clear_pastes();
            // Echo as a queued entry so the user sees it landed.
            renderer.render(UiLine::CommandOutput(format!("  ↳ queued: {}\n", line)));
            renderer.flush();
            draw_spinner_now(state, buf, ctx, renderer, message_queue.len());
        }
        BufferResult::Exit => {
            // Ctrl+C on empty buf during streaming — treat as cancel
            // (consistent with the explicit Ctrl+C branch above).
            ctx.agent.cmd_tx.send(AgentCommand::Cancel).ok();
        }
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
    pending_tools: &mut std::collections::HashMap<String, (String, String)>,
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
            state.on_tool_call_streaming(&display_tool_name(&name));
        }
        AgentEvent::ToolCallStarted { id, name, arguments } => {
            // Don't emit the ▸ line yet; hold it in pending_tools until the
            // matching ToolCallResult arrives. This preserves CC-style
            // visual pairing even when the agent runs tools in parallel
            // (all Starts then all Results in the event stream).
            let detail = format_tool_detail(&name, &arguments);
            let display = display_tool_name(&name);
            pending_tools.insert(id, (display.clone(), detail));
            state.on_tool_call_started(&display);
        }
        AgentEvent::ToolCallResult { call_id, name, output, success, .. } => {
            // Close any in-flight assistant line before emitting the pair.
            renderer.render(UiLine::AssistantLineBreak);

            // Prefer the display-name we stored at ToolCallStarted time;
            // fall back to converting the raw name if we missed the Start
            // (e.g. protocol surfaced a Result without a matching Start).
            let (display_name, detail) = pending_tools
                .remove(&call_id)
                .unwrap_or_else(|| (display_tool_name(&name), String::new()));

            // Filter empty tool names (model occasionally emits malformed
            // tool calls with "" as the name; agent surfaces the error via
            // a ToolCallResult but there's no useful ▸ line to render).
            let safe_name = if display_name.is_empty() {
                "(invalid)".to_string()
            } else {
                display_name
            };

            renderer.render(UiLine::ToolCall {
                name: safe_name.clone(),
                detail: detail.clone(),
            });
            let summary = summarise(&output);
            renderer.render(UiLine::ToolResult { success, summary });
            // Collect diff lines into a single batch — N individual
            // DiffLine renders each trigger a full footer redraw and
            // tens of KB of ANSI, which blocks the event loop long
            // enough to stall the spinner during edit tool results.
            let diff_entries: Vec<crate::render::DiffEntry> = output
                .lines()
                .take(120)
                .filter_map(|line| {
                    if let Some(rest) = line.strip_prefix("+ ") {
                        Some(crate::render::DiffEntry {
                            added: true,
                            text: rest.to_string(),
                        })
                    } else if let Some(rest) = line.strip_prefix("- ") {
                        Some(crate::render::DiffEntry {
                            added: false,
                            text: rest.to_string(),
                        })
                    } else {
                        None
                    }
                })
                .collect();
            if !diff_entries.is_empty() {
                renderer.render(UiLine::DiffBlock(diff_entries));
            }
            renderer.flush();
            let _ = name;
        }
        AgentEvent::ApprovalNeeded { tool_name, call, .. } => {
            let detail = format_tool_detail(&tool_name, &call.arguments);
            renderer.render(UiLine::ApprovalPrompt {
                tool: display_tool_name(&tool_name),
                detail,
            });
            renderer.flush();
            state.on_approval_needed(&tool_name);
        }
        AgentEvent::PhaseChange(AgentPhase::Thinking) => state.on_thinking(),
        AgentEvent::PhaseChange(AgentPhase::CallingTool(name)) => {
            state.on_tool_call_streaming(&display_tool_name(&name));
        }
        AgentEvent::PhaseChange(_) => {}
        AgentEvent::TurnComplete { duration, total_tokens, turn_count, tool_call_count, .. } => {
            renderer.render(UiLine::AssistantLineBreak);
            pending_tools.clear();
            let done = state.next_done_label();
            let dur = crate::render::fmt_dur(duration);
            let label = format!(
                "✓ {} · {} rounds · {} tools · {} · {} tok",
                done, turn_count, tool_call_count, dur, total_tokens
            );
            renderer.render(UiLine::TurnSeparator { label });
            renderer.flush();
            state.on_turn_complete();
        }
        AgentEvent::TurnCancelled { .. } => {
            // Render any in-flight tool calls that never got a result
            // as "(cancelled)" so the user sees what was mid-flight.
            for (_id, (name, detail)) in pending_tools.drain() {
                let safe_name = if name.is_empty() { "(invalid)".into() } else { name };
                renderer.render(UiLine::ToolCall { name: safe_name, detail });
                renderer.render(UiLine::ToolResult {
                    success: false,
                    summary: "(cancelled)".into(),
                });
            }
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
    model_picker: &mut Option<ModelPicker>,
    provider_wizard: &mut Option<ProviderWizard>,
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
        "session" => {
            // Start fresh: tell the agent to drop conversation history,
            // clear the scrollback + type-ahead queue + UI state, and
            // redraw the welcome screen so the user sees they're in a
            // brand-new session. Ports `/session` from the legacy TUI.
            ctx.agent.cmd_tx.send(AgentCommand::ClearConversation).ok();
            state.total_tokens = 0;
            state.thinking_idx = 0;
            state.on_turn_complete();
            // Wipe screen + reset scrollback view, like a fresh launch.
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[2J\x1b[H");
            let _ = std::io::Write::flush(&mut std::io::stdout());
            let dir_display = ctx.working_dir.to_string_lossy().to_string();
            let dir_display = if let Ok(home) = std::env::var("HOME") {
                dir_display.replacen(&home, "~", 1)
            } else {
                dir_display
            };
            renderer.render(UiLine::Welcome {
                model: ctx.model_name.clone(),
                working_dir: dir_display,
            });
            renderer.render(UiLine::CommandOutput("  New session started.\n".into()));
            renderer.flush();
        }
        "model" => {
            if ctx.config.providers.is_empty() {
                renderer.render(UiLine::CommandOutput(
                    "  No providers configured.\n".into(),
                ));
                renderer.flush();
            } else {
                *model_picker = Some(ModelPicker::open(&ctx.config));
            }
        }
        "provider" => {
            *provider_wizard = Some(ProviderWizard::MainMenu { selected: 0 });
            renderer.render(UiLine::CommandOutput(
                "  Provider management — Add / Edit / Delete / Set default. Esc to cancel.\n"
                    .into(),
            ));
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
        "login" => {
            run_login_flow(renderer, ctx)?;
        }
        "logout" => {
            match atomcode_core::auth::logout() {
                Ok(()) => {
                    renderer.render(UiLine::CommandOutput(
                        "  Signed out of AtomGit.\n".into(),
                    ));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(format!("logout failed: {}", e)));
                }
            }
            renderer.flush();
        }
        "whoami" => {
            let txt = if let Some(auth) = atomcode_core::auth::get_stored_auth() {
                let email = auth.user.email.as_deref().unwrap_or("—");
                let name = auth.user.name.as_deref().unwrap_or(&auth.user.username);
                format!(
                    "  {} ({})\n  {}\n  auth: {}\n",
                    name,
                    auth.user.username,
                    email,
                    atomcode_core::auth::auth_file_path().display(),
                )
            } else {
                "  Not signed in. Use /login to authenticate.\n".into()
            };
            renderer.render(UiLine::CommandOutput(txt));
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

/// Drop out of raw mode, run the (blocking) OAuth login flow so the user
/// can interact with the browser callback in a normal terminal, then
/// re-enter raw mode and redraw the welcome screen. OAuth uses stdout
/// prints + opens a browser — mixing that with our footer-managing
/// raw-mode renderer would collide on stdin/stdout, so we suspend.
fn run_login_flow(
    renderer: &mut dyn Renderer,
    ctx: &mut LoopCtx,
) -> Result<()> {
    // Suspend our UI so the OAuth flow owns the terminal.
    renderer.shutdown();
    let _ = crossterm::terminal::disable_raw_mode();

    let result = atomcode_core::auth::login()
        .and_then(|auth| atomcode_core::auth::save_auth(&auth).map(|()| auth));

    // Re-enter raw mode regardless of success/failure.
    let _ = crossterm::terminal::enable_raw_mode();

    match result {
        Ok(auth) => {
            let dir_display = ctx.working_dir.to_string_lossy().to_string();
            let dir_display = if let Ok(home) = std::env::var("HOME") {
                dir_display.replacen(&home, "~", 1)
            } else {
                dir_display
            };
            renderer.render(UiLine::Welcome {
                model: ctx.model_name.clone(),
                working_dir: dir_display,
            });
            let name = auth
                .user
                .name
                .as_deref()
                .unwrap_or(&auth.user.username)
                .to_string();
            renderer.render(UiLine::CommandOutput(format!(
                "  Signed in as {} ({}).\n",
                name, auth.user.username
            )));
            renderer.flush();
        }
        Err(e) => {
            renderer.render(UiLine::Error(format!("login failed: {}", e)));
            renderer.flush();
        }
    }
    Ok(())
}

/// Build the persistent status line shown directly below the input box.
/// Pulls model name from ctx, cwd from ctx.working_dir (with $HOME
/// collapsed to `~`), and running token count from state.
fn build_status(state: &UiState, ctx: &LoopCtx) -> crate::render::StatusLine {
    let cwd = ctx.working_dir.to_string_lossy().to_string();
    let cwd = if let Ok(home) = std::env::var("HOME") {
        cwd.replacen(&home, "~", 1)
    } else {
        cwd
    };
    crate::render::StatusLine {
        model: ctx.model_name.clone(),
        cwd,
        total_tokens: state.total_tokens,
    }
}

/// Render one spinner frame. Used from both the interval-driven tick
/// path and the opportunistic "post-event" pump path that guards
/// against agent-event floods starving the interval tick.
fn draw_spinner_now(
    state: &mut UiState,
    buf: &Buffer,
    ctx: &LoopCtx,
    renderer: &mut dyn Renderer,
    queue_len: usize,
) {
    let frame = state.tick_spinner();
    let label = format_spinner_label(state, queue_len);
    let status = build_status(state, ctx);
    renderer.render(UiLine::StreamingBox {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        frame,
        label,
        status,
    });
    renderer.flush();
}

/// Build the spinner line shown in the footer —
/// `"{label}… · {elapsed} · {N} queued"`. State stores only the bare
/// word (e.g. `Pondering`, `Running ReadFile`); ellipsis + elapsed +
/// queued suffixes are appended here so format is consistent across
/// every call site.
fn format_spinner_label(state: &UiState, queue_len: usize) -> String {
    let base = &state.spinner_label;
    let mut out = format!("{}…", base);
    if let Some(d) = state.turn_elapsed() {
        out.push_str(&format!(" · {}", crate::render::fmt_dur(d)));
    }
    if queue_len > 0 {
        out.push_str(&format!(" · {} queued", queue_len));
    }
    out
}

/// Convert a snake_case tool name to PascalCase for display. The agent
/// protocol uses `read_file`, `edit_file`, `web_fetch` etc.; the UI shows
/// `ReadFile`, `EditFile`, `WebFetch` — a CC-style convention that reads
/// more cleanly at a glance.
pub fn display_tool_name(snake: &str) -> String {
    let mut out = String::with_capacity(snake.len());
    for word in snake.split('_') {
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    out
}

fn format_tool_detail(name: &str, args_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return String::new();
    };
    let get_str = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let basename = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();

    match name {
        "read_file" | "edit_file" | "write_file" | "create_file" | "list_symbols" => {
            get_str("file_path").map(|p| basename(&p)).unwrap_or_default()
        }
        "read_symbol" => {
            let sym = get_str("symbol").unwrap_or_default();
            let file = get_str("file_path").map(|p| basename(&p)).unwrap_or_default();
            if sym.is_empty() { file } else if file.is_empty() { sym } else { format!("{} in {}", sym, file) }
        }
        "glob" => get_str("pattern")
            .map(|p| crate::width::truncate_to_width(&p, 40))
            .unwrap_or_default(),
        "grep" => get_str("pattern")
            .map(|p| crate::width::truncate_to_width(&p, 40))
            .unwrap_or_default(),
        "bash" => get_str("command")
            .map(|c| crate::width::truncate_to_width(&c, 60))
            .unwrap_or_default(),
        "list_directory" | "change_dir" => {
            get_str("path").unwrap_or_else(|| ".".into())
        }
        "web_fetch" => get_str("url")
            .map(|u| crate::width::truncate_to_width(&u, 60))
            .unwrap_or_default(),
        "web_search" => get_str("query")
            .map(|q| crate::width::truncate_to_width(&q, 50))
            .unwrap_or_default(),
        "find_references" | "trace_callees" | "trace_callers" | "trace_chain" => {
            get_str("symbol").unwrap_or_default()
        }
        "blast_radius" | "file_dependencies" => {
            get_str("file").map(|p| basename(&p)).unwrap_or_default()
        }
        "search_replace" => {
            let file = get_str("file_path").or_else(|| get_str("file"));
            let pat = get_str("pattern").or_else(|| get_str("old"));
            match (file, pat) {
                (Some(f), Some(p)) => format!("{}: {}", basename(&f), crate::width::truncate_to_width(&p, 25)),
                (Some(f), None) => basename(&f),
                (None, Some(p)) => crate::width::truncate_to_width(&p, 40),
                _ => String::new(),
            }
        }
        "use_skill" => get_str("name").unwrap_or_default(),
        _ => {
            // Fallback: try common single-key args that make sense as detail.
            for key in ["file_path", "path", "file", "pattern", "query", "url", "name", "symbol", "command"] {
                if let Some(s) = get_str(key) {
                    return crate::width::truncate_to_width(&s, 40);
                }
            }
            String::new()
        }
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
