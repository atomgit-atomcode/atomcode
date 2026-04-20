// crates/atomcode-tuix/src/event_loop/mod.rs
//
// Main event-loop crate root. `run_loop` is the entry point from
// `atomcode-tuix::run`; everything else in this module tree supports it.
//
// Layout:
//   mod.rs       — App struct + LoopCtx + run_loop + input plumbing
//                  (handle_input / handle_idle_key / handle_streaming_key /
//                  handle_approval_key / redraw helpers), plus Buffer +
//                  BufferResult + agent-event handler + spinner draw.
//   commands.rs  — slash-command dispatcher + /login (OAuth child handoff)
//
// Over time more subfiles should split out (agent_events, redraw helpers,
// Buffer); modal overlays already live in `crate::modals`.

mod commands;
use commands::execute_slash_command;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use atomcode_core::agent::{AgentCommand, AgentEvent, AgentHandle, AgentPhase};
use atomcode_core::config::Config;
use atomcode_core::session::SessionManager;
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
    pub session_manager: SessionManager,
    /// Shared "new version available" hint. Populated by the detached
    /// version-check task spawned from `run()`; read by `build_status`
    /// on each redraw. `None` = no hint (either check still pending,
    /// network failed silently, or already up to date).
    pub update_hint: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Wake signal from the version-check task — one `()` sent when the
    /// task resolves with a positive result. The event loop selects on
    /// `wake_rx` and triggers an idle redraw so the hint appears without
    /// waiting for the user's next keystroke.
    pub wake_rx: mpsc::Receiver<()>,
    /// Control handle for the crossterm reader thread — `Some` in raw-mode
    /// TTY sessions, `None` in pipe mode. Used by child-process handoffs
    /// (OAuth login, future `/shell`) to pause+resume event consumption
    /// so our reader doesn't race the child for stdin bytes.
    pub reader: Option<crate::input::reader::ReaderHandle>,
    /// Sender used by `/upgrade` to report streaming progress/failure
    /// events from the detached upgrade task. Cloned into the task at
    /// spawn time; kept here so the receiver in the loop outlives any
    /// number of upgrades (no reconstructing on each invocation).
    pub upgrade_tx: mpsc::UnboundedSender<atomcode_core::self_update::UpgradeEvent>,
    /// Consumed in the main `select!` so upgrade progress is rendered
    /// alongside agent events.
    pub upgrade_rx: mpsc::UnboundedReceiver<atomcode_core::self_update::UpgradeEvent>,
}

/// Line-edit buffer for input composition. Byte-indexed cursor.
///
/// Large pasted blocks are folded into `[Pasted #N +M lines]` placeholders
/// stored in `text`; the original contents live in `pastes` and are
/// spliced back in when the line is submitted. This keeps the visible
/// input short (matching CC's paste UX) without truncating what the
/// agent actually sees.
pub struct Buffer {
    pub text: String,
    pub cursor: usize,
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

    pub(crate) fn apply(&mut self, action: Action, history: &[String], commands: &CommandRegistry) -> BufferResult {
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

#[cfg(test)]
mod menu_tests {
    use super::*;

    #[test]
    fn non_slash_input_returns_no_menu() {
        let reg = CommandRegistry::builtin();
        assert!(build_menu_items("hello world", &reg).is_none());
    }

    #[test]
    fn slash_prefix_returns_all_commands() {
        let reg = CommandRegistry::builtin();
        let items = build_menu_items("/", &reg).expect("menu should show for '/'");
        assert!(!items.is_empty(), "builtin registry should have commands");
    }

    #[test]
    fn slash_with_filter_narrows_list() {
        let reg = CommandRegistry::builtin();
        let all = build_menu_items("/", &reg).unwrap();
        let filtered = build_menu_items("/he", &reg).unwrap_or_default();
        assert!(
            filtered.len() < all.len(),
            "prefix '/he' should filter builtin commands"
        );
        // Every filtered entry must start with "he".
        for (name, _) in &filtered {
            assert!(
                name.starts_with("he"),
                "prefix filter leaked non-matching '{}'",
                name
            );
        }
    }

    #[test]
    fn whitespace_after_slash_closes_menu() {
        // Once the user types args, menu goes away so arrow keys don't
        // start navigating a stale palette.
        let reg = CommandRegistry::builtin();
        assert!(build_menu_items("/cd ", &reg).is_none());
        assert!(build_menu_items("/cd /tmp", &reg).is_none());
    }

    #[test]
    fn slash_with_no_matches_returns_none() {
        let reg = CommandRegistry::builtin();
        assert!(build_menu_items("/zzznomatch", &reg).is_none());
    }
}

#[cfg(test)]
mod tool_format_tests {
    use super::*;

    #[test]
    fn display_tool_name_snake_to_pascal() {
        assert_eq!(display_tool_name("read_file"), "ReadFile");
        assert_eq!(display_tool_name("search_replace"), "SearchReplace");
        assert_eq!(display_tool_name("bash"), "Bash");
    }

    #[test]
    fn display_tool_name_handles_edge_cases() {
        assert_eq!(display_tool_name(""), "");
        assert_eq!(display_tool_name("x"), "X");
        assert_eq!(display_tool_name("x_"), "X");
        assert_eq!(display_tool_name("_x"), "X");
    }

    #[test]
    fn format_tool_detail_read_file_basename() {
        let args = r#"{"file_path":"/abs/path/to/foo.rs"}"#;
        assert_eq!(format_tool_detail("read_file", args), "foo.rs");
    }

    #[test]
    fn format_tool_detail_read_symbol_combines_symbol_and_file() {
        let args = r#"{"symbol":"parse","file_path":"src/lexer.rs"}"#;
        assert_eq!(format_tool_detail("read_symbol", args), "parse in lexer.rs");
    }

    #[test]
    fn format_tool_detail_bash_truncates_long_commands() {
        let args = format!(
            r#"{{"command":"{}"}}"#,
            "a".repeat(500)
        );
        let out = format_tool_detail("bash", &args);
        // 200-col budget with a 1-col trailing '…' (3 UTF-8 bytes).
        assert!(
            crate::width::display_width(&out) <= 200,
            "bash detail should truncate to <=200 cols, got {} cols `{}`",
            crate::width::display_width(&out),
            out
        );
        assert!(
            out.ends_with('…'),
            "truncated bash detail should end with ellipsis: `{}`",
            out
        );
    }

    #[test]
    fn format_tool_detail_unknown_tool_falls_back_to_common_keys() {
        // Unknown tool but args carry `file_path` — fallback uses it.
        let args = r#"{"file_path":"/tmp/a.txt","extra":"x"}"#;
        let out = format_tool_detail("my_custom_tool", args);
        assert!(!out.is_empty(), "fallback should find file_path");
    }

    #[test]
    fn format_tool_detail_invalid_json_returns_empty() {
        let out = format_tool_detail("read_file", "not json");
        assert_eq!(out, "");
    }

    #[test]
    fn summarise_single_line_returned_as_is() {
        assert_eq!(summarise("ok"), "ok");
    }

    #[test]
    fn summarise_multi_line_adds_line_count() {
        let out = summarise("first line\nsecond line\nthird line");
        assert!(out.starts_with("first line"));
        assert!(out.contains("(3 lines)"));
    }

    #[test]
    fn summarise_empty_string_has_fallback() {
        let out = summarise("");
        // Empty input: `lines()` yields nothing, so first falls back
        // to "(no output)" and n==0 means no " (N lines)" suffix.
        assert!(out.contains("(no output)"), "got: {}", out);
    }
}

pub(crate) enum BufferResult {
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

/// All the per-session UI state that flows through key/event handlers.
///
/// Before this aggregation, handlers took 7–9 `&mut` parameters each
/// and the call sites filled a paragraph. Now the handlers take
/// `(&mut App, &mut LoopCtx, &mut dyn Renderer, …event)` — the LoopCtx
/// stays separate because the tokio `select!` in `run_loop` needs to
/// borrow `ctx.input_rx`, `ctx.agent.event_rx`, `ctx.wake_rx`
/// independently, and bundling them into App would fight the borrow
/// checker on every arm.
pub struct App {
    pub state: UiState,
    pub buf: Buffer,
    pub menu: MenuState,
    /// Exactly one overlay at a time — /model, /provider, /resume all
    /// push into the same slot. The Modal trait owns draw + key handling
    /// so adding a fourth overlay is `Some(Box::new(X))`, not a new
    /// field + new dispatch branch.
    pub active_modal: Option<Box<dyn crate::modals::Modal>>,
    /// Messages the user submitted while a turn was already running.
    /// Drained one-at-a-time from the head whenever the current turn
    /// finishes. Matches CC's "type-ahead" UX — queue the next prompt
    /// while the model is still thinking and it fires automatically.
    pub message_queue: VecDeque<String>,
    /// Streaming-state `<think>…</think>` stripper. Kept on App (not
    /// a local in the streaming arm) because it carries state across
    /// agent events — a tag straddling two chunks would break if the
    /// stripper were re-constructed each event.
    pub think: ThinkStripper,
    /// call_id → (tool_name, detail). Populated on ToolCallStarted,
    /// consumed on ToolCallResult so the result line reads
    /// "name(detail) — summary" instead of a bare "✓ summary" detached
    /// from its originating call.
    pub pending_tools: std::collections::HashMap<String, (String, String)>,
}

impl App {
    fn new() -> Self {
        Self {
            state: UiState::new(),
            buf: Buffer::new(),
            menu: MenuState::new(),
            active_modal: None,
            message_queue: VecDeque::new(),
            think: ThinkStripper::new(),
            pending_tools: std::collections::HashMap::new(),
        }
    }
}

pub async fn run_loop(
    mut ctx: LoopCtx,
    renderer: &mut dyn Renderer,
) -> Result<()> {
    let mut app = App::new();

    crate::tuix_trace!(
        "SES",
        "run_loop start model={} cwd={}",
        ctx.model_name,
        ctx.working_dir.display()
    );

    // Draw welcome + initial prompt
    let dir_display = crate::platform::collapse_home(
        &ctx.working_dir.to_string_lossy(),
    );
    renderer.render(UiLine::Welcome { model: ctx.model_name.clone(), working_dir: dir_display.clone() });
    renderer.render(UiLine::InputPrompt {
        buf: String::new(),
        cursor_byte: 0,
        menu: None,
        status: build_status(&app.state, &ctx),
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

    // Deferred-render tick: 50fps. The renderer throttles InputPrompt /
    // StreamingBox redraws to 20ms windows so Mac Terminal.app doesn't
    // choke on back-to-back full footer payloads, but the trailing
    // edge of a burst needs someone to paint it — that someone is this
    // tick. No-op when nothing is pending.
    // 5ms matches the InputThrottle window (see render::throttle) —
    // tick == window means the max visible lag from "burst ended" to
    // "parked paint landed" is ~10ms, imperceptible. Previously 20ms
    // which compounded with the 20ms throttle window to ~40ms lag,
    // visible for IME commit bursts.
    let mut deferred_render_tick = tokio::time::interval(Duration::from_millis(5));
    deferred_render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    deferred_render_tick.tick().await; // consume the immediate fire

    // Last-draw timestamp — consulted by the post-event pump so we
    // don't redraw more often than every 100ms even when handlers
    // fire back-to-back.
    let mut last_spinner_draw = std::time::Instant::now();

    // Last emitted integer percent for the /upgrade download line.
    // Gate on change so we don't spam the renderer with a progress
    // line for every chunk (a 10 MB binary at 64 KiB chunks would be
    // 160 redraws). `-1` means "no download active yet".
    let mut upgrade_last_pct: i32 = -1;
    // True once Done fired successfully — the loop exits after the
    // current pending message finishes so the user sees the success
    // line before the TUI shuts down.
    let mut upgrade_done = false;

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

            // ── Deferred-render trailing edge ──
            // Drains any InputPrompt / StreamingBox payload the
            // renderer parked during its 20ms throttle window. No-op
            // when nothing is pending.
            _ = deferred_render_tick.tick() => {
                renderer.flush_deferred();
            }

            // ── Spinner tick (from background task) ──
            Some(()) = spin_rx.recv(), if matches!(app.state.phase, UiPhase::Streaming) => {
                draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                last_spinner_draw = std::time::Instant::now();
            }

            // ── Terminal input ──
            maybe = ctx.input_rx.recv() => {
                let Some(ev) = maybe else { break };
                handle_input(&mut app, &mut ctx, renderer, ev)?;
            }

            // ── Version-check wake ──
            // Fires once when the detached startup check resolves with a
            // positive result. Idle-only: in Streaming the spinner tick
            // redraws frequently enough that the hint picks up naturally.
            Some(()) = ctx.wake_rx.recv(), if matches!(app.state.phase, UiPhase::Idle) => {
                redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
            }

            // ── /upgrade progress ──
            Some(ev) = ctx.upgrade_rx.recv() => {
                handle_upgrade_event(ev, &mut upgrade_last_pct, &mut upgrade_done, &mut ctx, renderer);
                if upgrade_done { break; }
                if matches!(app.state.phase, UiPhase::Idle) {
                    redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                }
            }

            // ── Agent events ──
            maybe = ctx.agent.event_rx.recv(), if matches!(app.state.phase, UiPhase::Streaming) => {
                let Some(ev) = maybe else { break };
                let pre_phase = app.state.phase;
                handle_agent_event(ev, &mut app.state, &mut app.think, renderer, &mut app.pending_tools);
                if pre_phase != app.state.phase {
                    crate::tuix_trace!("PH", "{:?} -> {:?}", pre_phase, app.state.phase);
                }
                if matches!(app.state.phase, UiPhase::Streaming)
                    && last_spinner_draw.elapsed() >= Duration::from_millis(100)
                {
                    draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                    last_spinner_draw = std::time::Instant::now();
                }
                if matches!(app.state.phase, UiPhase::Idle) {
                    // Turn just ended — drain the type-ahead queue.
                    // Pop the oldest queued message, echo as a User
                    // line, dispatch to the agent, and transition
                    // back to Streaming. Remaining queue entries
                    // fire in order on subsequent completions.
                    if let Some(queued) = app.message_queue.pop_front() {
                        crate::tuix_trace!("QUE", "pop_front remaining={}", app.message_queue.len());
                        renderer.render(UiLine::User(queued.clone()));
                        renderer.flush();
                        ctx.agent.cmd_tx.send(AgentCommand::SendMessage(queued)).ok();
                        app.state.on_submit();
                        draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                    } else {
                        crate::tuix_trace!("PH", "turn_end -> Idle, queue empty, redraw_idle");
                        redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                    }
                }
            }

            // ── Suspend ──
            _ = sigtstp.recv() => {
                renderer.render(UiLine::ClearTransient);
                renderer.shutdown();
                app.state.on_suspend();
                // Disable raw mode before SIGSTOP so shell gets a sane terminal.
                let _ = crossterm::terminal::disable_raw_mode();
                unsafe { libc::raise(libc::SIGSTOP); }
            }

            // ── Resume ──
            _ = sigcont.recv() => {
                let _ = crossterm::terminal::enable_raw_mode();
                app.state.on_resume();
                match app.state.phase {
                    UiPhase::Streaming => {
                        draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                        last_spinner_draw = std::time::Instant::now();
                    }
                    _ => {
                        redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                    }
                }
            }
        }

        #[cfg(not(unix))]
        tokio::select! {
            biased;

            // ── Deferred-render trailing edge ──
            // Drains any InputPrompt / StreamingBox payload the
            // renderer parked during its 20ms throttle window. No-op
            // when nothing is pending.
            _ = deferred_render_tick.tick() => {
                renderer.flush_deferred();
            }

            // ── Spinner tick (from background task) ──
            Some(()) = spin_rx.recv(), if matches!(app.state.phase, UiPhase::Streaming) => {
                draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                last_spinner_draw = std::time::Instant::now();
            }

            // ── Terminal input ──
            maybe = ctx.input_rx.recv() => {
                let Some(ev) = maybe else { break };
                handle_input(&mut app, &mut ctx, renderer, ev)?;
            }

            // ── Version-check wake ──
            Some(()) = ctx.wake_rx.recv(), if matches!(app.state.phase, UiPhase::Idle) => {
                redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
            }

            // ── /upgrade progress ──
            Some(ev) = ctx.upgrade_rx.recv() => {
                handle_upgrade_event(ev, &mut upgrade_last_pct, &mut upgrade_done, &mut ctx, renderer);
                if upgrade_done { break; }
                if matches!(app.state.phase, UiPhase::Idle) {
                    redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                }
            }

            // ── Agent events ──
            maybe = ctx.agent.event_rx.recv(), if matches!(app.state.phase, UiPhase::Streaming) => {
                let Some(ev) = maybe else { break };
                let pre_phase = app.state.phase;
                handle_agent_event(ev, &mut app.state, &mut app.think, renderer, &mut app.pending_tools);
                if pre_phase != app.state.phase {
                    crate::tuix_trace!("PH", "{:?} -> {:?}", pre_phase, app.state.phase);
                }
                if matches!(app.state.phase, UiPhase::Streaming)
                    && last_spinner_draw.elapsed() >= Duration::from_millis(100)
                {
                    draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                    last_spinner_draw = std::time::Instant::now();
                }
                if matches!(app.state.phase, UiPhase::Idle) {
                    if let Some(queued) = app.message_queue.pop_front() {
                        crate::tuix_trace!("QUE", "pop_front remaining={}", app.message_queue.len());
                        renderer.render(UiLine::User(queued.clone()));
                        renderer.flush();
                        ctx.agent.cmd_tx.send(AgentCommand::SendMessage(queued)).ok();
                        app.state.on_submit();
                        draw_spinner_now(&mut app.state, &app.buf, &ctx, renderer, app.message_queue.len(), app.menu.selected);
                    } else {
                        crate::tuix_trace!("PH", "turn_end -> Idle, queue empty, redraw_idle");
                        redraw_idle_plain(&app.buf, &app.state, &ctx, renderer);
                    }
                }
            }
        }

        if matches!(app.state.phase, UiPhase::Idle) && ctx.agent.cmd_tx.is_closed() {
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
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    ev: InputEvent,
) -> Result<()> {
    use crate::modals::ModalAction;

    crate::tuix_trace!(
        "IN",
        "phase={:?} modal={} qlen={} ev={}",
        app.state.phase,
        app.active_modal.is_some(),
        app.message_queue.len(),
        match &ev {
            InputEvent::Paste(t) => format!("paste({})", t.len()),
            InputEvent::Eof => "eof".into(),
            InputEvent::Key(k) => format!("key({:?},{:?})", k.kind, k.code),
            InputEvent::Resize(w, h) => format!("resize({}x{})", w, h),
        }
    );

    match ev {
        InputEvent::Resize(cols, rows) => {
            // Forward to the renderer so DECSTBM-based backends can
            // re-issue their scroll region and repaint the footer at
            // the new geometry. Fire-and-forget; the render worker
            // serialises this against in-flight content writes.
            renderer.on_resize(cols, rows);
        }
        InputEvent::Paste(text) => {
            // Allow pasting during Streaming too — it goes into the
            // type-ahead buffer just like keyboard input. Modals have
            // their own key handling and ignore paste events.
            if matches!(app.state.phase, UiPhase::Idle | UiPhase::Streaming)
                && app.active_modal.is_none()
            {
                app.buf.insert_paste(text);
                if matches!(app.state.phase, UiPhase::Streaming) {
                    draw_spinner_now(&mut app.state, &app.buf, ctx, renderer, app.message_queue.len(), app.menu.selected);
                } else {
                    redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                }
            }
        }
        InputEvent::Eof => {}
        // Only act on Press events. On Unix tty crossterm only emits Press
        // so this guard is a no-op there; on Windows crossterm emits all
        // three kinds (Press / Repeat / Release). Without filtering to
        // Press we double-fired on every keystroke (Press + Release both
        // ran the handler) and a held-down key fired again on every
        // Repeat tick, producing "ghost characters" / runaway backspace
        // the moment the OS autorepeat kicked in.
        InputEvent::Key(KeyEvent { kind: KeyEventKind::Press, code, modifiers, .. }) => {
            // Modal trumps phase handlers when it's installed — /model,
            // /provider, /resume all install a modal and the event loop
            // funnels every keystroke through it until it reports Close.
            if matches!(app.state.phase, UiPhase::Idle) {
                if let Some(modal) = app.active_modal.as_mut() {
                    let action = modal.handle_key(
                        code, modifiers, &mut app.buf, &mut app.state, ctx, renderer,
                    )?;
                    if matches!(action, ModalAction::Close) {
                        app.active_modal = None;
                        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                    }
                    return Ok(());
                }
            }
            match app.state.phase {
                UiPhase::Idle => handle_idle_key(app, ctx, renderer, code, modifiers)?,
                UiPhase::Streaming => handle_streaming_key(app, ctx, renderer, code, modifiers)?,
                UiPhase::Approval => handle_approval_key(code, &mut app.state, ctx, renderer)?,
                UiPhase::Suspended => {}
            }
        }
        // Release / Repeat key events: drop on the floor. Press is handled
        // above; everything else is noise on Windows.
        InputEvent::Key(_) => {}
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

// `ModelPicker` moved to `crate::modals::model_picker`; re-exported at
// `crate::modals::ModelPicker` for existing call sites (execute_slash_command).
pub use crate::modals::ModelPicker;

// `SessionPicker` moved to `crate::modals::session_picker`; re-exported
// at `crate::modals::SessionPicker` for existing call sites.
pub use crate::modals::SessionPicker;

// `ProviderWizard` + `WizardStep` + `DraftProvider` moved to
// `crate::modals::provider_wizard`; re-exported at `crate::modals` for
// existing call sites (execute_slash_command).
pub use crate::modals::ProviderWizard;

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
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Result<()> {
    // If the menu is active (buf starts with '/'), intercept navigation keys.
    let menu_items = build_menu_items(&app.buf.text, &ctx.commands);
    if let Some(items) = &menu_items {
        // Clamp selection in range.
        if app.menu.selected >= items.len() {
            app.menu.selected = items.len() - 1;
        }
        match (code, modifiers) {
            (KeyCode::Up, _) => {
                app.menu.selected = app.menu.selected.saturating_sub(1);
                redraw_with_menu(&app.buf, items, app.menu.selected, &app.state, ctx, renderer);
                return Ok(());
            }
            (KeyCode::Down, _) => {
                if app.menu.selected + 1 < items.len() {
                    app.menu.selected += 1;
                }
                redraw_with_menu(&app.buf, items, app.menu.selected, &app.state, ctx, renderer);
                return Ok(());
            }
            (KeyCode::Enter, m) if !m.contains(crossterm::event::KeyModifiers::SHIFT) => {
                // Accept the highlighted command as the committed line.
                let name = items[app.menu.selected].0.clone();
                let committed = format!("/{}", name);
                app.menu.selected = 0;
                // Simulate a commit path.
                renderer.render(UiLine::ClearTransient);
                renderer.render(UiLine::User(committed.clone()));
                app.buf.text.clear();
                app.buf.cursor = 0;
                if let Some((cmd, arg)) = parse_slash_line(&committed) {
                    execute_slash_command(cmd, arg, &mut app.state, ctx, renderer, &mut app.active_modal)?;
                    if matches!(app.state.phase, UiPhase::Idle) {
                        redraw_after_slash(&app.buf, &app.state, ctx, &app.active_modal, renderer);
                    }
                }
                return Ok(());
            }
            (KeyCode::Esc, _) => {
                // Close menu by clearing buffer.
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                return Ok(());
            }
            _ => {} // fall through to buffer edits
        }
    }

    let action = classify(code, modifiers);
    let result = app.buf.apply(action, ctx.history.entries(), &ctx.commands);
    crate::tuix_trace!(
        "KEY",
        "idle result={} buf_len={} cursor={}",
        match &result {
            BufferResult::NoOp => "NoOp",
            BufferResult::Redraw => "Redraw",
            BufferResult::Commit(_) => "Commit",
            BufferResult::Exit => "Exit",
        },
        app.buf.text.len(),
        app.buf.cursor
    );
    match result {
        BufferResult::NoOp => {}
        BufferResult::Redraw => {
            // Rebuild menu after buf change.
            let items = build_menu_items(&app.buf.text, &ctx.commands);
            if let Some(items) = items {
                if app.menu.selected >= items.len() {
                    app.menu.selected = 0;
                }
                redraw_with_menu(&app.buf, &items, app.menu.selected, &app.state, ctx, renderer);
            } else {
                app.menu.selected = 0;
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
            }
        }
        BufferResult::Commit(line) => {
            // Expand paste placeholders so the agent sees full content
            // while the echoed user line and history stay compact.
            let expanded = app.buf.expand_pastes(&line);
            renderer.render(UiLine::ClearTransient);
            renderer.render(UiLine::User(line.clone()));
            app.buf.text.clear();
            app.buf.cursor = 0;
            app.buf.clear_pastes();
            app.menu.selected = 0;
            if let Some((cmd, arg)) = parse_slash_line(&line) {
                execute_slash_command(cmd, arg, &mut app.state, ctx, renderer, &mut app.active_modal)?;
                if matches!(app.state.phase, UiPhase::Idle) {
                    redraw_after_slash(&app.buf, &app.state, ctx, &app.active_modal, renderer);
                }
            } else {
                ctx.history.push(line.clone());
                ctx.agent.cmd_tx.send(AgentCommand::SendMessage(expanded)).ok();
                app.state.on_submit();
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

/// Redraw after running a slash command. If the command installed a
/// modal, delegate the draw to it so the modal's menu appears; otherwise
/// fall through to the plain idle prompt.
///
/// Replaces the old per-picker `redraw_idle` that hard-coded payload
/// construction for model/session. New modals just implement `draw`.
fn redraw_after_slash(
    buf: &Buffer,
    state: &UiState,
    ctx: &LoopCtx,
    active_modal: &Option<Box<dyn crate::modals::Modal>>,
    renderer: &mut dyn Renderer,
) {
    if let Some(modal) = active_modal.as_ref() {
        modal.draw(buf, state, ctx, renderer);
    } else {
        redraw_idle_plain(buf, state, ctx, renderer);
    }
}

/// Persist config changes and notify the daemon to pick them up.
pub(crate) fn save_and_reload(ctx: &mut LoopCtx, renderer: &mut dyn Renderer) {
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

fn handle_streaming_key(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Result<()> {
    // Ctrl+C always cancels the running turn — highest priority so
    // users have a reliable escape hatch even mid-edit.
    if code == KeyCode::Char('c') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        ctx.agent.cmd_tx.send(AgentCommand::Cancel).ok();
        return Ok(());
    }

    // Esc also cancels a running turn (CC-style). Placed before the
    // menu-nav block so Streaming + menu-open Esc still cancels the
    // stream — mid-stream the higher-value action is "stop the agent",
    // not "clear an unsubmitted slash token" (users can Ctrl+U for that).
    if code == KeyCode::Esc {
        ctx.agent.cmd_tx.send(AgentCommand::Cancel).ok();
        return Ok(());
    }

    // When the menu is active (buf starts with `/`), intercept nav keys
    // so the user can browse candidate commands mid-stream. Execution
    // is still blocked below — Enter falls through to the commit arm,
    // which emits the "disabled while a turn is running" hint.
    let menu_items = build_menu_items(&app.buf.text, &ctx.commands);
    if let Some(items) = &menu_items {
        if app.menu.selected >= items.len() {
            app.menu.selected = items.len() - 1;
        }
        match code {
            KeyCode::Up => {
                app.menu.selected = app.menu.selected.saturating_sub(1);
                draw_spinner_now(&mut app.state, &app.buf, ctx, renderer, app.message_queue.len(), app.menu.selected);
                return Ok(());
            }
            KeyCode::Down => {
                if app.menu.selected + 1 < items.len() {
                    app.menu.selected += 1;
                }
                draw_spinner_now(&mut app.state, &app.buf, ctx, renderer, app.message_queue.len(), app.menu.selected);
                return Ok(());
            }
            KeyCode::Esc => {
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                draw_spinner_now(&mut app.state, &app.buf, ctx, renderer, app.message_queue.len(), app.menu.selected);
                return Ok(());
            }
            _ => {} // fall through to buffer edits
        }
    }

    let action = classify(code, modifiers);
    match app.buf.apply(action, ctx.history.entries(), &ctx.commands) {
        BufferResult::NoOp => {}
        BufferResult::Redraw => {
            // Menu shape may have changed — reset selection if it
            // now points past the (possibly shorter) list.
            if let Some(items) = build_menu_items(&app.buf.text, &ctx.commands) {
                if app.menu.selected >= items.len() {
                    app.menu.selected = 0;
                }
            } else {
                app.menu.selected = 0;
            }
            draw_spinner_now(&mut app.state, &app.buf, ctx, renderer, app.message_queue.len(), app.menu.selected);
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
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                draw_spinner_now(&mut app.state, &app.buf, ctx, renderer, app.message_queue.len(), app.menu.selected);
                return Ok(());
            }
            // Expand any paste placeholders — agent sees full payload,
            // scrollback echo stays compact.
            let expanded = app.buf.expand_pastes(&line);
            ctx.history.push(line.clone());
            app.message_queue.push_back(expanded);
            crate::tuix_trace!("QUE", "push_back len={}", app.message_queue.len());
            app.buf.text.clear();
            app.buf.cursor = 0;
            app.buf.clear_pastes();
            // Echo as a queued entry so the user sees it landed.
            renderer.render(UiLine::CommandOutput(format!("  ↳ queued: {}\n", line)));
            renderer.flush();
            draw_spinner_now(&mut app.state, &app.buf, ctx, renderer, app.message_queue.len(), app.menu.selected);
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
    renderer: &mut dyn Renderer,
) -> Result<()> {
    let cmd = match code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => AgentCommand::ApproveTool,
        KeyCode::Char('a') | KeyCode::Char('A') => AgentCommand::ApproveToolAlways,
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => AgentCommand::DenyTool,
        _ => return Ok(()),
    };
    // Retract the "Waiting for approval" body row now that the user
    // responded — without this, the prompt stays in scrollback next to
    // the tool result, creating visual noise.
    renderer.pop_approval_prompt();
    ctx.agent.cmd_tx.send(cmd).ok();
    state.on_approval_resolved();
    Ok(())
}

/// Render one streamed upgrade event. Mutates the percent tracker so
/// Downloading lines only redraw on whole-percent changes (see caller's
/// `upgrade_last_pct` reasoning). Sets `done = true` when the upgrade
/// succeeds, so the main loop can break after rendering the success
/// line — the user must restart to load the new binary.
pub(super) fn handle_upgrade_event(
    ev: atomcode_core::self_update::UpgradeEvent,
    last_pct: &mut i32,
    done: &mut bool,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
) {
    use atomcode_core::self_update::UpgradeEvent;
    match ev {
        UpgradeEvent::ManifestFetched { version } => {
            *last_pct = -1;
            renderer.render(UiLine::CommandOutput(format!(
                "  最新版本: {}\n",
                version
            )));
        }
        UpgradeEvent::Downloading { bytes, total } => {
            let pct = if total == 0 {
                0
            } else {
                ((bytes * 100) / total) as i32
            };
            if pct != *last_pct {
                *last_pct = pct;
                // Emit at 25/50/75/100 to keep output tidy. Finer-grained
                // progress would flood the append-only renderer with lines
                // since there's no in-place update here.
                if pct == 25 || pct == 50 || pct == 75 || pct == 100 {
                    renderer.render(UiLine::CommandOutput(format!(
                        "  下载中 {}% ({} / {} bytes)\n",
                        pct, bytes, total
                    )));
                }
            }
        }
        UpgradeEvent::Verifying => {
            renderer.render(UiLine::CommandOutput("  正在校验 SHA256\n".into()));
        }
        UpgradeEvent::Replacing => {
            renderer.render(UiLine::CommandOutput("  正在替换二进制文件\n".into()));
        }
        UpgradeEvent::Done { version, backup } => {
            renderer.render(UiLine::CommandOutput(format!(
                "\n✓ 已升级到 {}（旧版本保留为 {}）\n  请退出后重新运行 `atomcode` 以加载新版本。\n",
                version,
                backup.display()
            )));
            // Push the hint in the status bar to match the new reality —
            // the little "↑ vX" arrow goes away for this session.
            if let Ok(mut g) = ctx.update_hint.lock() {
                *g = None;
            }
            *done = true;
            // Tell the agent to shut down so the loop exits cleanly.
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
        }
        UpgradeEvent::Failed(msg) => {
            renderer.render(UiLine::Error(format!("升级失败: {}", msg)));
        }
        UpgradeEvent::RolledBack { exe, backup } => {
            renderer.render(UiLine::CommandOutput(format!(
                "\n✓ 已回滚。当前二进制: {}；另一版本保存在 {}\n  请退出后重新运行 `atomcode` 加载回滚版本。\n",
                exe.display(),
                backup.display()
            )));
            *done = true;
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
        }
    }
    renderer.flush();
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


/// Build the persistent status line shown directly below the input box.
/// Pulls model name from ctx, cwd from ctx.working_dir (with $HOME
/// collapsed to `~`), and running token count from state.
pub(crate) fn build_status(state: &UiState, ctx: &LoopCtx) -> crate::render::StatusLine {
    let cwd = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
    let hint = ctx
        .update_hint
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .map(|v| format!("↑ {} 使用/upgrade升级", v));
    crate::render::StatusLine {
        model: ctx.model_name.clone(),
        cwd,
        total_tokens: state.total_tokens,
        hint,
    }
}

/// Render one spinner frame. Used from both the interval-driven tick
/// path and the opportunistic "post-event" pump path that guards
/// against agent-event floods starving the interval tick.
///
/// When the type-ahead buffer starts with `/`, the slash-command palette
/// is attached so the user can see candidate commands mid-stream (the
/// renderer then shows the menu in place of the spinner).
fn draw_spinner_now(
    state: &mut UiState,
    buf: &Buffer,
    ctx: &LoopCtx,
    renderer: &mut dyn Renderer,
    queue_len: usize,
    menu_selected: usize,
) {
    let frame = state.tick_spinner();
    let label = format_spinner_label(state, queue_len);
    let status = build_status(state, ctx);
    let menu = build_menu_items(&buf.text, &ctx.commands).map(|items| {
        let selected = menu_selected.min(items.len().saturating_sub(1));
        crate::render::MenuPayload { items, selected }
    });
    renderer.render(UiLine::StreamingBox {
        buf: buf.text.clone(),
        cursor_byte: buf.cursor,
        frame,
        label,
        status,
        menu,
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

pub(crate) fn format_tool_detail(name: &str, args_json: &str) -> String {
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
            .map(|p| crate::width::truncate_with_ellipsis(&p, 100))
            .unwrap_or_default(),
        "grep" => get_str("pattern")
            .map(|p| crate::width::truncate_with_ellipsis(&p, 100))
            .unwrap_or_default(),
        "bash" => get_str("command")
            .map(|c| crate::width::truncate_with_ellipsis(&c, 200))
            .unwrap_or_default(),
        "list_directory" | "change_dir" => {
            get_str("path").unwrap_or_else(|| ".".into())
        }
        "web_fetch" => get_str("url")
            .map(|u| crate::width::truncate_with_ellipsis(&u, 150))
            .unwrap_or_default(),
        "web_search" => get_str("query")
            .map(|q| crate::width::truncate_with_ellipsis(&q, 100))
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
                (Some(f), Some(p)) => format!("{}: {}", basename(&f), crate::width::truncate_with_ellipsis(&p, 60)),
                (Some(f), None) => basename(&f),
                (None, Some(p)) => crate::width::truncate_with_ellipsis(&p, 100),
                _ => String::new(),
            }
        }
        "use_skill" => get_str("name").unwrap_or_default(),
        _ => {
            // Fallback: try common single-key args that make sense as detail.
            for key in ["file_path", "path", "file", "pattern", "query", "url", "name", "symbol", "command"] {
                if let Some(s) = get_str(key) {
                    return crate::width::truncate_with_ellipsis(&s, 100);
                }
            }
            String::new()
        }
    }
}

pub(crate) fn summarise(output: &str) -> String {
    let first = output.lines().next().unwrap_or("(no output)");
    let n = output.lines().count();
    let trimmed = crate::width::truncate_to_width(first, 80);
    if n > 1 {
        format!("{} ({} lines)", trimmed, n)
    } else {
        trimmed
    }
}

// SessionPicker tests moved alongside the struct in
// `crate::modals::session_picker::tests`.
