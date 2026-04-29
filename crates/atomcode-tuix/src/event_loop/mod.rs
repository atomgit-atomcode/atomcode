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

pub(crate) mod commands;
pub(crate) mod monitor;
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

#[derive(Debug, Clone)]
pub struct McpReloadProgress {
    pub total: usize,
    pub done: usize,
    pub connected: usize,
    pub failed: usize,
    pub started_at: std::time::Instant,
}

/// Bag of handles passed into the loop.
pub struct LoopCtx {
    pub config: Config,
    pub model_name: String,
    pub agent: AgentHandle,
    pub working_dir: PathBuf,
    pub previous_dir: Option<PathBuf>,
    /// Recently visited project directories, most recent first (max 5).
    /// Persisted to `~/.atomcode/recent_dirs.txt`. Drives the `/cd`
    /// picker when invoked with no argument and is updated whenever
    /// the working directory changes (via slash command or agent tool).
    pub recent_dirs: Vec<PathBuf>,
    pub history: History,
    pub input_rx: mpsc::UnboundedReceiver<InputEvent>,
    pub commands: CommandRegistry,
    pub session_manager: SessionManager,
    /// Session actively being accumulated. Updated on TurnComplete /
    /// TurnCancelled (both carry the latest `messages` slice), saved to
    /// disk via `session_manager` on the same events so `/resume` after
    /// a quit sees the conversation. Replaced wholesale when the user
    /// resumes another session via `/resume` + SessionPicker.
    pub current_session: atomcode_core::session::Session,
    /// Shared "new version available" hint. Populated by the detached
    /// version-check task spawned from `run()`; read by `build_status`
    /// on each redraw. `None` = no hint (either check still pending,
    /// network failed silently, or already up to date).
    pub update_hint: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Shared CodingPlan drift-monitor warning slot. Written by the
    /// detached check task (see `monitor::spawn_check`); read by
    /// `build_status` on each redraw. Takes precedence over `update_hint`
    /// so a drift warning isn't buried by an upgrade banner. Cleared
    /// when `/codingplan` persists a fresh config (re-sync resets the
    /// hint state).
    pub monitor_warning: std::sync::Arc<std::sync::Mutex<Option<monitor::CodingPlanWarning>>>,
    /// Last time a monitor check was fired this session. Pre-turn
    /// triggers respect `monitor::CHECK_COOLDOWN` (15 min) against this
    /// timestamp; startup + `/model` switch bypass the cooldown.
    /// `None` = no check has run yet this session.
    pub monitor_last_check_at: Option<std::time::Instant>,
    /// Last-observed timestamp from the shared CodingPlan sync marker
    /// (`~/.atomcode/codingplan_sync.json`). On every user input we
    /// re-read it; a change means ANOTHER atomcode process (e.g. a
    /// second terminal) just ran `/codingplan` and the server is now
    /// in sync with the on-disk config. We then hot-reload config
    /// from disk + clear the stale drift warning. Without this,
    /// Terminal A's "CodingPlan 模型列表更新" hint would stick forever
    /// after Terminal B ran the fix.
    pub monitor_last_sync_seen: Option<std::time::SystemTime>,
    /// Wake signal from background tasks (version check + CodingPlan
    /// drift monitor). One `()` sent when any task needs the event loop
    /// to repaint so a freshly-computed hint/warning appears without
    /// waiting for the user's next keystroke. Bounded at 1 — overlapping
    /// wakes coalesce since the redraw is idempotent.
    pub wake_rx: mpsc::Receiver<()>,
    /// Sender side of `wake_rx`. Cloned into every spawned check task
    /// so `/model` switches, pre-turn triggers, and the like can wake
    /// the event loop after updating `monitor_warning`.
    pub wake_tx: mpsc::Sender<()>,
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
    /// Signal channel from the `/issue` wizard modal back to the event
    /// loop. The wizard's Enter handler can't touch `App` directly
    /// (modals only see `LoopCtx`), so it stores the collected title +
    /// body here, returns `Close`, and the event loop's post-close
    /// branch POSTs the issue to AtomGit and echoes the URL of the
    /// newly-created issue back into the conversation.
    pub pending_new_issue: Option<NewIssueDraft>,
    /// Set by `WelcomeWizard` when the user picks option 0 (Set up
    /// CodingPlan). The event loop drains this on modal close and
    /// runs the full CodingPlan setup flow (login if needed → claim →
    /// fetch models → register providers). Needs raw-mode
    /// suspend/resume, something modals can't drive themselves. Same
    /// pattern as `pending_new_issue`.
    pub pending_run_codingplan: bool,
    /// Set by `WelcomeWizard` when the user picks option 1 (Configure
    /// manually). The event loop drains this on modal close and swaps in
    /// `ProviderWizard::MainMenu` — a Modal-to-Modal transition that
    /// needs mutable `active_modal` access only the event loop has.
    pub pending_open_provider_wizard: bool,
    /// MCP server registry for `/mcp` status display. `None` when no MCP
    /// servers are configured or all failed to connect.
    pub mcp_registry: Option<std::sync::Arc<atomcode_core::mcp::McpRegistry>>,
    /// Channel for receiving MCP connection status events (Connected/Failed).
    /// Events are rendered into scrollback as they arrive during startup.
    pub mcp_connect_rx: Option<tokio::sync::mpsc::UnboundedReceiver<atomcode_core::mcp::McpConnectEvent>>,
    /// When `/mcp reload` is invoked, we track progress until every configured
    /// server reports Connected/Failed, then emit a one-line summary.
    pub mcp_reload: Option<McpReloadProgress>,
    /// Telemetry handle — used to emit `UseCommand` at each slash dispatch.
    pub telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
    /// Original working dir before `/worktree create`, for `/worktree done`.
    pub worktree_original_dir: Option<PathBuf>,
    /// User-defined custom commands loaded from `~/.atomcode/commands/` and
    /// `<project>/.atomcode/commands/`. Queried by the slash-command
    /// dispatcher as a fallback when the entered name doesn't match a
    /// built-in command.
    pub custom_commands: atomcode_core::commands::CustomCommandRegistry,
    /// Loaded skills (`.claude/skills/*/SKILL.md`, etc.). Same `Arc`
    /// the agent loop holds, so `reload(...)` there is visible here
    /// without extra plumbing. Used by the slash-command palette to
    /// surface user-invocable skills, and by the dispatcher to expand
    /// `/skill_name [args]` into a SendMessage.
    pub skill_registry: std::sync::Arc<std::sync::RwLock<atomcode_core::skill::SkillRegistry>>,
    /// Snapshot of the terminal's rendering capabilities. Probed once at
    /// startup in `lib.rs`; threaded into `App::new` so `UiState` knows
    /// whether to use Unicode or ASCII fallbacks for the spinner glyph
    /// and ellipsis. Same value as `RetainedRenderer` was constructed
    /// with — single source of truth.
    pub caps: crate::terminal::TerminalCaps,
}

/// What the `/issue` wizard hands back to the event loop after the user
/// finishes step 2. The event loop turns this into a `POST /repos/.../issues`
/// API call and echoes the resulting issue URL into scrollback.
#[derive(Debug, Clone)]
pub struct NewIssueDraft {
    pub owner: String,
    pub repo: String,
    pub title: String,
    pub body: String,
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

    /// True while the user is scrolling input history (Up/Down on an
    /// empty / non-empty buffer). The slash-command menu suppresses
    /// itself in this state so that recalling a previous `/session foo`
    /// from history doesn't immediately re-pop the menu and trap Up
    /// inside it. Cleared automatically by `Insert` / `Cancel` (typing
    /// or Esc) and by `HistoryNext` returning past the newest entry
    /// to the user's stashed draft.
    pub fn is_in_history(&self) -> bool {
        self.history_idx.is_some()
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
    pub fn insert_paste(&mut self, text: String) -> String {
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

    pub(crate) fn apply(
        &mut self,
        action: Action,
        history: &[String],
        commands: &CommandRegistry,
    ) -> BufferResult {
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
                let end = self.text[self.cursor..]
                    .find('\n')
                    .map(|i| self.cursor + i)
                    .unwrap_or(self.text.len());
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
                let start = self.text[..self.cursor]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                self.cursor = start;
                BufferResult::Redraw
            }
            Action::LineEnd => {
                let end = self.text[self.cursor..]
                    .find('\n')
                    .map(|i| self.cursor + i)
                    .unwrap_or(self.text.len());
                self.cursor = end;
                BufferResult::Redraw
            }
            Action::HistoryPrev => {
                if history.is_empty() {
                    return BufferResult::Redraw;
                }
                // The current buffer (including any newlines) is stashed
                // before we replace it with a history entry, so users
                // who pressed Up mid-multi-line-compose can recover it
                // via HistoryNext (Down). No need to block the action.
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
                    // Park cursor at column 0 — recalled history is for
                    // re-running, not editing in place. A `/session foo`
                    // pulled from history would otherwise leave the
                    // cursor at end and re-trigger the slash menu via
                    // `is_in_history()`-gated logic; keeping it at 0
                    // mirrors Claude Code's behaviour and feels
                    // consistent with "this is recalled text, scroll
                    // again to keep going".
                    self.cursor = 0;
                }
                BufferResult::Redraw
            }
            Action::HistoryNext => {
                if let Some(i) = self.history_idx {
                    if i + 1 < history.len() {
                        // Still inside history — same cursor-at-0 rule
                        // as HistoryPrev.
                        self.history_idx = Some(i + 1);
                        self.text = history[i + 1].clone();
                        self.cursor = 0;
                    } else {
                        // Past the newest entry — restore the user's
                        // stashed draft. Cursor goes to end so they
                        // can keep typing where they left off before
                        // they started scrolling.
                        self.history_idx = None;
                        self.text = self.stash.clone();
                        self.cursor = self.text.len();
                    }
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
            Action::ToggleToolOutput => BufferResult::NoOp,
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
    use atomcode_core::commands::CustomCommandRegistry;

    #[test]
    fn non_slash_input_returns_no_menu() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        assert!(build_menu_items("hello world", &reg, &custom, None).is_none());
    }

    #[test]
    fn slash_prefix_returns_all_commands() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let items = build_menu_items("/", &reg, &custom, None).expect("menu should show for '/'");
        assert!(!items.is_empty(), "builtin registry should have commands");
    }

    #[test]
    fn slash_with_filter_narrows_list() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let all = build_menu_items("/", &reg, &custom, None).unwrap();
        let filtered = build_menu_items("/he", &reg, &custom, None).unwrap_or_default();
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
        let custom = CustomCommandRegistry::empty();
        assert!(build_menu_items("/cd ", &reg, &custom, None).is_none());
        assert!(build_menu_items("/cd /tmp", &reg, &custom, None).is_none());
    }

    #[test]
    fn slash_with_no_matches_returns_none() {
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        assert!(build_menu_items("/zzznomatch", &reg, &custom, None).is_none());
    }

    fn skill_fixture(name: &str, desc: &str, user_invocable: bool) -> atomcode_core::skill::Skill {
        atomcode_core::skill::Skill {
            name: name.to_string(),
            description: desc.to_string(),
            template: "do thing".to_string(),
            disable_model_invocation: false,
            user_invocable,
            argument_hint: None,
            allowed_tools: vec![],
            skill_dir: std::path::PathBuf::new(),
            source_path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn top_level_hides_individual_skills() {
        // Regression for the two-level palette: typing /bra or any
        // bare-name prefix must NOT surface skills. They live behind
        // the `/skills` gateway so the top palette stays uncluttered.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        skills.register(skill_fixture("skills:web-access", "Web", true));
        let lock = std::sync::RwLock::new(skills);

        // /bra — no skill should appear; /bra falls through to "no
        // matches" since no built-in starts with bra either.
        assert!(
            build_menu_items("/bra", &reg, &custom, Some(&lock)).is_none(),
            "individual skills must not leak into the top-level menu"
        );

        // /skills — only the built-in gateway entry, never the
        // individual skills.
        let items = build_menu_items("/skills", &reg, &custom, Some(&lock))
            .expect("/skills must include the built-in gateway");
        assert!(items.iter().any(|(n, _)| n == "skills"));
        for (n, _) in &items {
            assert!(
                !n.contains(':'),
                "namespaced skill leaked into top-level: {}",
                n
            );
        }
    }

    #[test]
    fn skills_sub_mode_lists_skills_under_bare_names() {
        // Once the user has typed `/skills ` (trailing space, normally
        // injected by the needs_args path on Enter), the palette
        // switches to second-level: bare skill names, ready to commit
        // as `/skills <name>`.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        skills.register(skill_fixture("skills:web-access", "Web", true));
        let lock = std::sync::RwLock::new(skills);

        let items = build_menu_items("/skills ", &reg, &custom, Some(&lock))
            .expect("/skills (with space) must list skills");
        assert!(items.iter().any(|(n, _)| n == "brainstorming"));
        assert!(items.iter().any(|(n, _)| n == "web-access"));
        for (n, _) in &items {
            assert!(!n.contains(':'), "sub-mode names must be bare: {}", n);
        }
    }

    #[test]
    fn skills_sub_mode_filters_by_bare_prefix() {
        // /skills bra narrows to brainstorming. /skills web narrows
        // to web-access. /skills zz returns no menu at all.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        skills.register(skill_fixture("skills:web-access", "Web", true));
        let lock = std::sync::RwLock::new(skills);

        let bra = build_menu_items("/skills bra", &reg, &custom, Some(&lock))
            .expect("filter must produce a result");
        assert_eq!(bra.len(), 1);
        assert_eq!(bra[0].0, "brainstorming");

        let web = build_menu_items("/skills web", &reg, &custom, Some(&lock))
            .expect("filter must produce a result");
        assert_eq!(web.len(), 1);
        assert_eq!(web[0].0, "web-access");

        assert!(build_menu_items("/skills zz", &reg, &custom, Some(&lock)).is_none());
    }

    #[test]
    fn skills_sub_mode_hides_after_skill_name() {
        // /skills brainstorming why X — user is typing skill args now,
        // menu should disappear so arrow keys don't navigate stale entries.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:brainstorming", "Brainstorm", true));
        let lock = std::sync::RwLock::new(skills);

        assert!(build_menu_items("/skills brainstorming why", &reg, &custom, Some(&lock)).is_none());
    }

    #[test]
    fn skills_sub_mode_excludes_hidden_skills() {
        // user_invocable=false skills must not surface in the sub-menu
        // either — they're LLM-only via the use_skill tool.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let mut skills = atomcode_core::skill::SkillRegistry::new();
        skills.register(skill_fixture("skills:visible", "shown", true));
        skills.register(skill_fixture("skills:hidden", "hidden", false));
        let lock = std::sync::RwLock::new(skills);

        let items = build_menu_items("/skills ", &reg, &custom, Some(&lock))
            .expect("at least one visible skill should produce a menu");
        assert!(items.iter().any(|(n, _)| n == "visible"));
        assert!(
            !items.iter().any(|(n, _)| n == "hidden"),
            "user_invocable=false skill leaked into sub-menu"
        );
    }

    #[test]
    fn no_skill_registry_is_no_op() {
        // Ensures the legacy call path (None) keeps working.
        let reg = CommandRegistry::builtin();
        let custom = CustomCommandRegistry::empty();
        let with_none = build_menu_items("/", &reg, &custom, None).unwrap();
        let empty_skills = std::sync::RwLock::new(atomcode_core::skill::SkillRegistry::new());
        let with_empty = build_menu_items("/", &reg, &custom, Some(&empty_skills)).unwrap();
        assert_eq!(
            with_none.len(),
            with_empty.len(),
            "empty registry must produce same menu as None"
        );
    }

    // Regression: HistoryPrev used to leave the cursor at end-of-text,
    // so a recalled `/session foo` from history would `is_in_history()`
    // true AND have the slash prefix — without the call-site gate, the
    // menu would auto-pop, trapping Up/Down inside it. The fix is twofold
    // (caller skips menu while in history; cursor parks at 0 to signal
    // "this is recalled, scroll again"). These two unit tests pin both.
    #[test]
    fn history_prev_parks_cursor_at_zero_and_marks_history_mode() {
        let mut buf = Buffer::new();
        let reg = CommandRegistry::builtin();
        let history = vec!["/session foo".to_string()];

        let _ = buf.apply(Action::HistoryPrev, &history, &reg);

        assert_eq!(buf.text, "/session foo");
        assert_eq!(buf.cursor, 0, "cursor must park at 0 to suppress menu");
        assert!(buf.is_in_history(), "buffer must report history mode");
    }

    #[test]
    fn history_next_back_to_stash_restores_cursor_to_end() {
        let mut buf = Buffer::new();
        let reg = CommandRegistry::builtin();
        let history = vec!["/session foo".to_string()];

        // Type a partial draft, then scroll into history and back out.
        let _ = buf.apply(Action::Insert('h'), &history, &reg);
        let _ = buf.apply(Action::Insert('i'), &history, &reg);
        let _ = buf.apply(Action::HistoryPrev, &history, &reg);
        assert!(buf.is_in_history());
        let _ = buf.apply(Action::HistoryNext, &history, &reg);

        // Past newest entry → restored stash with cursor at the end so
        // the user can keep typing where they left off.
        assert_eq!(buf.text, "hi");
        assert_eq!(buf.cursor, 2);
        assert!(!buf.is_in_history());
    }

    #[test]
    fn typing_clears_history_mode() {
        // Sanity check — Insert resets history_idx, so the menu can
        // re-appear naturally once the user starts editing the recall.
        let mut buf = Buffer::new();
        let reg = CommandRegistry::builtin();
        let history = vec!["/session foo".to_string()];

        let _ = buf.apply(Action::HistoryPrev, &history, &reg);
        assert!(buf.is_in_history());
        let _ = buf.apply(Action::Insert('x'), &history, &reg);
        assert!(!buf.is_in_history());
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
        let args = format!(r#"{{"command":"{}"}}"#, "a".repeat(500));
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
        assert_eq!(summarise("ok", true), "ok");
    }

    #[test]
    fn summarise_multi_line_adds_line_count() {
        let out = summarise("first line\nsecond line\nthird line", true);
        assert!(out.starts_with("first line"));
        assert!(out.contains("(3 lines)"));
    }

    #[test]
    fn summarise_empty_string_has_fallback() {
        let out = summarise("", true);
        // Empty input: `lines()` yields nothing, so first falls back
        // to "(no output)" and n==0 means no " (N lines)" suffix.
        assert!(out.contains("(no output)"), "got: {}", out);
    }

    /// Reproduces the bug: a long error message ending in a deep WSL
    /// path used to silently truncate to 80 cols, leaving `f_stor`
    /// instead of `f_store` with no `…` to indicate the cut. Failures
    /// now get a 200-col budget so the path stays intact, and any
    /// truncation that does happen is visibly marked with `…`.
    #[test]
    fn summarise_failure_keeps_long_path_intact() {
        let err = "Error: old_string not found in \
                   /mnt/d/docs/work/cangjie/projects/fountain/f_store.";
        let out = summarise(err, false);
        assert!(
            out.contains("/mnt/d/docs/work/cangjie/projects/fountain/f_store"),
            "the full path must survive the summary. got: {}",
            out
        );
        assert!(
            !out.contains("f_stor "),
            "must not produce mid-token truncation like `f_stor ` (note the \
             trailing space — that's where (N lines) would attach). got: {}",
            out
        );
    }

    /// Sanity check: success summaries still respect the tighter
    /// 80-col cap (we don't want to flood the body with full status
    /// output on every successful tool call). When that cap *does*
    /// truncate, the ellipsis must appear — that was the second leg
    /// of the fix beyond just enlarging the budget.
    #[test]
    fn summarise_success_truncates_with_ellipsis_at_80() {
        let long: String = "x".repeat(200);
        let out = summarise(&long, true);
        // 80 col cap means at most 80 chars of x, plus the ellipsis.
        assert!(
            out.ends_with('…'),
            "ellipsis is the visible-truncation marker. got: {}",
            out
        );
        assert!(out.chars().count() <= 80);
    }

    /// Failure summaries keep the line-count suffix when the original
    /// was multi-line — the budget bump shouldn't change that behaviour.
    #[test]
    fn summarise_failure_multi_line_still_appends_count() {
        let err = "Error: foo\nbar\nbaz";
        let out = summarise(err, false);
        assert!(out.starts_with("Error: foo"));
        assert!(out.contains("(3 lines)"));
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
    while !s.is_char_boundary(p) {
        p -= 1;
    }
    p
}

fn next_boundary(s: &str, mut p: usize) -> usize {
    p += 1;
    while p < s.len() && !s.is_char_boundary(p) {
        p += 1;
    }
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
    /// call_id → (tool_name, detail, call_rendered). Populated on
    /// ToolCallStarted, read by `ApprovalNeeded` (which renders the
    /// `▸ Tool(detail)` line eagerly so the user sees *what* they're
    /// being asked to approve), and consumed on ToolCallResult. The
    /// `call_rendered` flag prevents rendering the tool-call line
    /// twice when ApprovalNeeded fired first.
    pub pending_tools: std::collections::HashMap<String, (String, String, bool)>,
    /// Timestamp of the first Ctrl+C press on an empty idle buffer.
    /// Requires a second press within `CTRL_C_EXIT_WINDOW` to actually
    /// exit — protects against accidental single-tap exits.
    pub exit_pending: Option<std::time::Instant>,
    /// Set by `/fixissue <url>` while the agent is resolving that issue.
    /// On `TurnComplete` the text buffered in `fixissue_buffer` is posted
    /// back as an issue comment + the `fixed` label is applied. Cleared
    /// on TurnComplete / TurnCancelled / Error so a subsequent normal
    /// message doesn't accidentally trigger a post-back.
    pub fixissue_pending: Option<atomcode_core::atomgit::IssueRef>,
    /// Accumulates every visible `AssistantText` delta produced during a
    /// fixissue turn, verbatim. Sent as the AtomGit comment body on
    /// successful completion.
    pub fixissue_buffer: String,
    /// Accumulates reasoning/thinking content for display in verbose mode.
    /// Flushed on newline or when buffer exceeds threshold.
    pub reasoning_buffer: String,
}

/// How long the "press Ctrl+C again to exit" confirmation stays armed.
const CTRL_C_EXIT_WINDOW: Duration = Duration::from_secs(2);

impl App {
    fn new(caps: &crate::terminal::TerminalCaps) -> Self {
        Self {
            state: UiState::with_unicode(caps.unicode_symbols),
            buf: Buffer::new(),
            menu: MenuState::new(),
            active_modal: None,
            message_queue: VecDeque::new(),
            think: ThinkStripper::new(),
            pending_tools: std::collections::HashMap::new(),
            exit_pending: None,
            fixissue_pending: None,
            fixissue_buffer: String::new(),
            reasoning_buffer: String::new(),
        }
    }
}

pub async fn run_loop(mut ctx: LoopCtx, renderer: &mut dyn Renderer) -> Result<()> {
    let mut app = App::new(&ctx.caps);

    crate::tuix_trace!(
        "SES",
        "run_loop start model={} cwd={}",
        ctx.model_name,
        ctx.working_dir.display()
    );

    // Draw welcome + initial prompt
    let dir_display = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
    renderer.render(UiLine::Welcome {
        model: ctx.model_name.clone(),
        working_dir: dir_display.clone(),
    });
    // If this process was spawned by `apply_pending_upgrade` → `re_exec_self`,
    // an env var carries the version we just upgraded from. Surface one line
    // on the welcome screen so the user knows the upgrade succeeded, then
    // clear the var so any subprocesses we spawn don't inherit a stale hint.
    if let Ok(prev) = std::env::var("ATOMCODE_UPGRADED_FROM") {
        std::env::remove_var("ATOMCODE_UPGRADED_FROM");
        let current = format!("v{}", env!("CARGO_PKG_VERSION"));
        renderer.render(UiLine::CommandOutput(format!(
            "  ✓ Upgraded {} → {}\n",
            prev, current
        )));
    }
    // Same env-var handoff from `atomcode codingplan` (see CLI `run()`):
    // the subcommand stashes its rendered SetupReport here instead of
    // printing to stdout, so the user sees the ✔/✘ lines in the chat
    // scrollback rather than scrolled off above the welcome banner.
    if let Ok(report) = std::env::var("ATOMCODE_CODINGPLAN_REPORT") {
        std::env::remove_var("ATOMCODE_CODINGPLAN_REPORT");
        if !report.is_empty() {
            renderer.render(UiLine::CommandOutput(report));
        }
    }

    // Terminal keyboard hint: when the terminal doesn't support Kitty
    // keyboard protocol (CSI u), Shift+Enter is indistinguishable from
    // plain Enter. Show a hint so users know to use Alt+Enter or
    // Ctrl+Enter for newline insertion instead.
    if std::env::var("ATOMCODE_KBD_NOT_ENHANCED").is_ok() {
        std::env::remove_var("ATOMCODE_KBD_NOT_ENHANCED");
        // Show platform-appropriate hint. On macOS, Option+Enter may not work
        // in all terminals, so we recommend Ctrl+Enter as the primary fallback.
        #[cfg(target_os = "macos")]
        renderer.render(UiLine::CommandOutput(
            "  ⚠ Terminal does not support enhanced keyboard protocol.\n    Use Ctrl+Enter for newline (Shift+Enter won't work).\n\n".into(),
        ));
        #[cfg(not(target_os = "macos"))]
        renderer.render(UiLine::CommandOutput(
            "  ⚠ Terminal does not support enhanced keyboard protocol.\n    Use Alt+Enter or Ctrl+Enter for newline (Shift+Enter won't work).\n\n".into(),
        ));
    }

    // JediTerm auto-fallback hint: lib.rs detected
    // `TERMINAL_EMULATOR=JetBrains-JediTerm` (Android Studio, IntelliJ,
    // PyCharm, etc.) and routed to AltScreenRenderer because the
    // retained renderer's DECSTBM-pinned footer misaligns there.
    // Tell the user about the trade-off — alt-screen owns the
    // viewport so the host terminal's native scrollback isn't
    // available; the app provides its own (PageUp / Shift+Up /
    // mouse wheel). Only set by lib.rs when the user did NOT
    // explicitly opt in via ATOMCODE_PLAIN / ATOMCODE_ALT —
    // informed choices don't get lectured.
    if std::env::var("ATOMCODE_JEDITERM_FALLBACK").is_ok() {
        std::env::remove_var("ATOMCODE_JEDITERM_FALLBACK");
        renderer.render(UiLine::CommandOutput(
            "  ⓘ JetBrains IDE terminal detected — running in alt-screen mode.\n    \
             Use mouse wheel, PageUp/PageDown, or Shift+Up/Down to scroll history.\n    \
             Native terminal scrollback is unavailable while atomcode runs;\n    \
             on exit your host terminal restores its pre-atomcode state.\n    \
             Set ATOMCODE_PLAIN=1 for a bare CI-style baseline, or\n    \
             ATOMCODE_RETAIN=1 to bypass this fallback (may misalign).\n\n"
                .into(),
        ));
    }

    // Legacy Windows console (cmd.exe / classic conhost) auto-fallback
    // hint: lib.rs detected Windows + neither WT_SESSION nor
    // TERM_PROGRAM, which means the user is on stock conhost where
    // DECSTBM misbehaves (rows duplicate in scrollback on Page-Up).
    // Phase 5: now routes to AltScreenRenderer. Mutually exclusive
    // with the JediTerm hint (lib.rs gates legacy_conhost on
    // `!is_jediterm`).
    if std::env::var("ATOMCODE_LEGACY_CONHOST_FALLBACK").is_ok() {
        std::env::remove_var("ATOMCODE_LEGACY_CONHOST_FALLBACK");
        renderer.render(UiLine::CommandOutput(
            "  ⓘ Legacy Windows console detected — running in alt-screen mode.\n    \
             Use mouse wheel, PageUp/PageDown, or Shift+Up/Down to scroll history.\n    \
             Native terminal scrollback is unavailable while atomcode runs.\n    \
             For full host-terminal scrollback support, install Windows Terminal\n    \
             (free, Microsoft Store), ConEmu, Alacritty, or WezTerm.\n    \
             Set ATOMCODE_PLAIN=1 for a bare baseline, or ATOMCODE_RETAIN=1 to\n    \
             bypass this fallback (may show duplicated content on scroll).\n\n"
                .into(),
        ));
    }

    // First-run onboarding: no providers configured AND no OAuth login
    // on disk means the user has never set this up. Show the legacy-tui
    // 3-choice wizard (Login / Configure manually / Skip) as a modal —
    // same mechanism as /resume, /provider, etc. Users with a config or
    // prior OAuth auth are never shown this and boot straight to idle.
    let is_first_run =
        ctx.config.providers.is_empty() && atomcode_core::auth::get_stored_auth().is_none();
    if is_first_run {
        // Body-side guide — pushed to scrollback above the footer menu,
        // gives the user context before they navigate the MenuPayload.
        // Kept compact (5 lines) so on small terminals the menu still
        // fits without scrolling the welcome banner off-screen.
        renderer.render(UiLine::CommandOutput(
            "\n  Welcome to AtomCode. Pick an option to get started:\n  \
             (↑↓ to navigate, Enter to confirm, Esc to skip)\n\n"
                .into(),
        ));
        app.active_modal = Some(Box::new(crate::modals::WelcomeWizard::new()));
        if let Some(m) = app.active_modal.as_mut() {
            m.draw(&app.buf, &app.state, &ctx, renderer);
        }
    } else {
        renderer.render(UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: None,
            status: build_status(&app.state, &ctx),
        });
        renderer.flush();
    }

    // Startup CodingPlan drift check. Without this, a user who ran
    // `/codingplan` days ago and now sees a new model in the plan lineup
    // wouldn't learn until they typed a message — the mid-turn trigger
    // at the submit-path only fires on user action. Gating:
    //
    //   * Only when the active provider is an AtomGit* (CodingPlan)
    //     provider — non-CodingPlan users do zero network work on boot.
    //   * Still respects the 15-min cooldown against `monitor_last_check_at`
    //     so rapid restarts (e.g. crash-loop during development) don't
    //     spam the API gateway.
    //
    // The check itself is fully async (`spawn_check` returns immediately
    // and runs on a tokio task); the event loop entering its main tick
    // loop below isn't blocked, and the warning — when it arrives a
    // second or two later — wakes the loop via `wake_tx` so the status
    // row repaints without the user needing to press a key.
    if monitor::is_codingplan_provider(&ctx.config.default_provider) {
        let cooled = ctx
            .monitor_last_check_at
            .map(|t| t.elapsed() >= monitor::CHECK_COOLDOWN)
            .unwrap_or(true);
        if cooled {
            ctx.monitor_last_check_at = Some(std::time::Instant::now());
            monitor::spawn_check(
                ctx.config.clone(),
                ctx.model_name.clone(),
                ctx.monitor_warning.clone(),
                ctx.wake_tx.clone(),
            );
        }
    }

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
    let mut sigtstp =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP))?;
    #[cfg(unix)]
    let mut sigcont =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGCONT))?;

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

            // ── MCP connection events ──
            // Render connection success/failure into scrollback as they arrive.
            // Also register tools dynamically when servers connect.
            Some(ev) = async {
                if let Some(rx) = ctx.mcp_connect_rx.as_mut() {
                    rx.recv().await
                } else {
                    None
                }
            }, if ctx.mcp_connect_rx.is_some() => {
                use atomcode_core::mcp::{McpConnectEvent, register_mcp_tools_async};
                match &ev {
                    McpConnectEvent::Connected { name } => {
                        renderer.render(UiLine::CommandOutput(format!("✓ MCP server '{}' connected", name)));
                        // Register tools from this newly connected server.
                        // Important: do this in a background task so a slow `tools/list`
                        // can't block the TUI event loop and freeze input.
                        if let Some(registry) = &ctx.mcp_registry {
                            let registry = registry.clone();
                            let tools = ctx.agent.tool_registry.clone();
                            let name = name.clone();
                            let tx = registry.event_sender();
                            tokio::spawn(async move {
                                let server_tools = match tokio::time::timeout(
                                    Duration::from_secs(15),
                                    registry.list_tools_for_server(&name),
                                )
                                .await
                                {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Some(tx) = tx {
                                            let _ = tx.send(McpConnectEvent::Warning {
                                                name,
                                                message: "tools/list timed out after 15s during auto-registration"
                                                    .to_string(),
                                            });
                                        }
                                        return;
                                    }
                                };
                                if !server_tools.is_empty() {
                                    register_mcp_tools_async(&tools, registry, server_tools).await;
                                }
                            });
                        }
                    }
                    McpConnectEvent::Failed { name, error } => {
                        renderer.render(UiLine::Error(format!("✗ MCP server '{}' failed: {}", name, error)));
                    }
                    McpConnectEvent::Warning { name, message } => {
                        // Default: keep MCP startup/runtime noise out of scrollback.
                        //
                        // Exception: `/mcp tools <server>` uses Warning events to return the tool list
                        // (and related timeouts) from a background task. Those should be user-visible.
                        if message.starts_with("tools:\n")
                            || message.contains("tools/list timed out")
                            || message.contains("tools/list failed")
                        {
                            renderer.render(UiLine::CommandOutput(format!(
                                "  [mcp:{}] {}\n",
                                name,
                                message.trim_end()
                            )));
                        } else {
                            // Route to the opt-in tuix trace log instead (safe for raw-mode TUI).
                            crate::tuix_trace!("MCP", "server='{}' warning: {}", name, message);
                        }
                    }
                }

                // `/mcp reload` progress: once every configured server has reported a
                // terminal state (Connected/Failed), emit a summary line.
                if let Some(p) = ctx.mcp_reload.as_mut() {
                    match &ev {
                        McpConnectEvent::Connected { .. } => {
                            p.done = p.done.saturating_add(1);
                            p.connected = p.connected.saturating_add(1)
                        }
                        McpConnectEvent::Failed { .. } => {
                            p.done = p.done.saturating_add(1);
                            p.failed = p.failed.saturating_add(1)
                        }
                        McpConnectEvent::Warning { .. } => {}
                    }
                    if p.done >= p.total {
                        let elapsed_ms = p.started_at.elapsed().as_millis();
                        renderer.render(UiLine::CommandOutput(format!(
                            "  MCP reload complete: {} connected, {} failed ({}ms)\n",
                            p.connected, p.failed, elapsed_ms
                        )));
                        ctx.mcp_reload = None;
                    }
                }
                renderer.flush();
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
            // Consumed regardless of phase. Gating on Streaming missed
            // the TurnComplete that arrives *after* an Error event: the
            // Error handler flips phase to Idle, so the very next event
            // on the channel is stuck until the user submits again —
            // which is what "得发两次你好才结束" looked like in the UI.
            // Phase-specific behaviour (spinner redraw, type-ahead queue
            // drain) lives inside the match arms on `app.state.phase`.
            maybe = ctx.agent.event_rx.recv() => {
                let Some(ev) = maybe else { break };
                let pre_phase = app.state.phase;
                handle_agent_event(ev, &mut app.state, &mut app.think, renderer, &mut app.pending_tools, &mut ctx, &mut app.fixissue_pending, &mut app.fixissue_buffer, &mut app.reasoning_buffer);
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

            // ── MCP connection events ──
            // Render connection success/failure into scrollback as they arrive.
            // Also register tools dynamically when servers connect.
            Some(ev) = async {
                if let Some(rx) = ctx.mcp_connect_rx.as_mut() {
                    rx.recv().await
                } else {
                    None
                }
            }, if ctx.mcp_connect_rx.is_some() => {
                use atomcode_core::mcp::{McpConnectEvent, register_mcp_tools_async};
                match &ev {
                    McpConnectEvent::Connected { name } => {
                        renderer.render(UiLine::CommandOutput(format!("✓ MCP server '{}' connected", name)));
                        // Register tools from this newly connected server (backgrounded).
                        if let Some(registry) = &ctx.mcp_registry {
                            let registry = registry.clone();
                            let tools = ctx.agent.tool_registry.clone();
                            let name = name.clone();
                            let tx = registry.event_sender();
                            tokio::spawn(async move {
                                let server_tools = match tokio::time::timeout(
                                    Duration::from_secs(15),
                                    registry.list_tools_for_server(&name),
                                )
                                .await
                                {
                                    Ok(v) => v,
                                    Err(_) => {
                                        if let Some(tx) = tx {
                                            let _ = tx.send(McpConnectEvent::Warning {
                                                name,
                                                message: "tools/list timed out after 15s during auto-registration"
                                                    .to_string(),
                                            });
                                        }
                                        return;
                                    }
                                };
                                if !server_tools.is_empty() {
                                    register_mcp_tools_async(&tools, registry, server_tools).await;
                                }
                            });
                        }
                    }
                    McpConnectEvent::Failed { name, error } => {
                        renderer.render(UiLine::Error(format!("✗ MCP server '{}' failed: {}", name, error)));
                    }
                    McpConnectEvent::Warning { name, message } => {
                        // Default: keep MCP startup/runtime noise out of scrollback.
                        //
                        // Exception: `/mcp tools <server>` uses Warning events to return the tool list
                        // (and related timeouts) from a background task. Those should be user-visible.
                        if message.starts_with("tools:\n")
                            || message.contains("tools/list timed out")
                            || message.contains("tools/list failed")
                        {
                            renderer.render(UiLine::CommandOutput(format!(
                                "  [mcp:{}] {}\n",
                                name,
                                message.trim_end()
                            )));
                        } else {
                            // Route to the opt-in tuix trace log instead (safe for raw-mode TUI).
                            crate::tuix_trace!("MCP", "server='{}' warning: {}", name, message);
                        }
                    }
                }

                // `/mcp reload` progress: once every configured server has reported a
                // terminal state (Connected/Failed), emit a summary line.
                if let Some(p) = ctx.mcp_reload.as_mut() {
                    match &ev {
                        McpConnectEvent::Connected { .. } => {
                            p.done = p.done.saturating_add(1);
                            p.connected = p.connected.saturating_add(1)
                        }
                        McpConnectEvent::Failed { .. } => {
                            p.done = p.done.saturating_add(1);
                            p.failed = p.failed.saturating_add(1)
                        }
                        McpConnectEvent::Warning { .. } => {}
                    }
                    if p.done >= p.total {
                        let elapsed_ms = p.started_at.elapsed().as_millis();
                        renderer.render(UiLine::CommandOutput(format!(
                            "  MCP reload complete: {} connected, {} failed ({}ms)\n",
                            p.connected, p.failed, elapsed_ms
                        )));
                        ctx.mcp_reload = None;
                    }
                }
                renderer.flush();
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
            // Consumed regardless of phase. Gating on Streaming missed
            // the TurnComplete that arrives *after* an Error event: the
            // Error handler flips phase to Idle, so the very next event
            // on the channel is stuck until the user submits again —
            // which is what "得发两次你好才结束" looked like in the UI.
            // Phase-specific behaviour (spinner redraw, type-ahead queue
            // drain) lives inside the match arms on `app.state.phase`.
            maybe = ctx.agent.event_rx.recv() => {
                let Some(ev) = maybe else { break };
                let pre_phase = app.state.phase;
                handle_agent_event(ev, &mut app.state, &mut app.think, renderer, &mut app.pending_tools, &mut ctx, &mut app.fixissue_pending, &mut app.fixissue_buffer, &mut app.reasoning_buffer);
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

/// If another atomcode process just ran `/codingplan` (i.e. the shared
/// sync marker file advanced since we last looked), pull the fresh
/// config from disk, clear our stale drift warning, and hand the new
/// config to the agent. Cheap on every keystroke: a single file-read
/// + serde parse. Idempotent — when no other process has synced, the
/// early return skips all work.
fn refresh_after_cross_process_codingplan_sync(ctx: &mut LoopCtx) {
    let current = atomcode_core::coding_plan::read_last_sync();
    let advanced = match (current, ctx.monitor_last_sync_seen) {
        (Some(new), Some(old)) => new > old,
        (Some(_), None) => true, // marker just appeared
        _ => false,
    };
    if !advanced {
        return;
    }
    ctx.monitor_last_sync_seen = current;

    // Hot-reload the config file. Fail silently: if the other process
    // wrote a malformed config (shouldn't happen — it would have
    // rejected its own reload), leave our in-memory snapshot alone.
    let path = atomcode_core::config::Config::default_path();
    if let Ok(fresh) = atomcode_core::config::Config::load(&path) {
        ctx.config = fresh;
        if let Some(p) = ctx.config.providers.get(&ctx.config.default_provider) {
            ctx.model_name = p.model.clone();
        }
        let _ = ctx
            .agent
            .cmd_tx
            .send(AgentCommand::ReloadConfig(ctx.config.clone()));
    }

    // Sync marker = another process just reconciled config with
    // server, so any drift warning we're still showing is stale by
    // definition. Reset the cooldown too so the next drift check
    // (if needed) fires immediately instead of waiting 15 min from
    // whenever we last checked.
    if let Ok(mut g) = ctx.monitor_warning.lock() {
        *g = None;
    }
    ctx.monitor_last_check_at = None;
}

fn handle_input(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    ev: InputEvent,
) -> Result<()> {
    use crate::modals::ModalAction;

    // Pick up any cross-process `/codingplan` that ran since the last
    // input — hot-reloads config + clears stale drift hint before we
    // act on the current keystroke.
    refresh_after_cross_process_codingplan_sync(ctx);

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
            InputEvent::MouseScroll(d) => format!("mouse_scroll({})", d),
            InputEvent::MouseDown { col, row } => format!("mouse_down({},{})", col, row),
            InputEvent::MouseDrag { col, row } => format!("mouse_drag({},{})", col, row),
            InputEvent::MouseUp => "mouse_up".into(),
        }
    );

    match ev {
        InputEvent::MouseScroll(delta) => {
            // Mouse wheel — only the alt-screen renderer takes action;
            // retained / plain default to no-op (host terminal handles
            // their scrollback natively, mouse capture isn't enabled
            // for them anyway).
            renderer.scroll_body(delta);
        }
        InputEvent::MouseDown { col, row } => {
            // Anchor a new selection. Only AltScreenRenderer responds
            // (it owns mouse capture); other backends no-op since the
            // host terminal still does native drag-to-select for them.
            renderer.begin_selection(col, row);
        }
        InputEvent::MouseDrag { col, row } => {
            renderer.update_selection(col, row);
        }
        InputEvent::MouseUp => {
            renderer.end_selection();
        }
        InputEvent::Resize(cols, rows) => {
            // Forward to the renderer so DECSTBM-based backends can
            // re-issue their scroll region and repaint the footer at
            // the new geometry. Fire-and-forget; the render worker
            // serialises this against in-flight content writes.
            renderer.on_resize(cols, rows);
        }
        InputEvent::Paste(text) => {
            // Route paste to the active modal when one is installed — the
            // provider/model/session wizards all have text-input steps
            // where pasting URLs / API keys / tokens is the natural UX.
            // Modals that don't want paste can override `handle_paste`
            // to drop it; the default inserts into `buf` + redraws.
            if matches!(app.state.phase, UiPhase::Idle) {
                if let Some(modal) = app.active_modal.as_mut() {
                    let action =
                        modal.handle_paste(&text, &mut app.buf, &mut app.state, ctx, renderer)?;
                    if matches!(action, crate::modals::ModalAction::Close) {
                        app.active_modal = None;
                        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                    }
                    return Ok(());
                }
            }
            // No modal: paste goes into the type-ahead buffer just like
            // keyboard input (Idle or Streaming, both consume it).
            if matches!(app.state.phase, UiPhase::Idle | UiPhase::Streaming) {
                app.buf.insert_paste(text);
                if matches!(app.state.phase, UiPhase::Streaming) {
                    draw_spinner_now(
                        &mut app.state,
                        &app.buf,
                        ctx,
                        renderer,
                        app.message_queue.len(),
                        app.menu.selected,
                    );
                } else {
                    redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                }
            }
        }
        InputEvent::Eof => {}
        // Act on Press AND Repeat. Release is dropped (it would double-fire
        // every handler on Windows, where crossterm emits all three kinds
        // per keystroke).
        //
        // Repeat is what the Kitty protocol's `REPORT_EVENT_TYPES` bit
        // (enabled in lib.rs) turns OS key autorepeat into — without
        // accepting it, holding Left/Right/Backspace only moves one step
        // because every autorepeat tick gets dropped here. Accepting it
        // also doesn't cause runaway Submit on a held Enter: Submit
        // transitions to Streaming phase, and Streaming's Enter handler
        // doesn't submit again.
        //
        // Terminals that don't support `REPORT_EVENT_TYPES` (iTerm2 3.5+,
        // Apple Terminal) leak autorepeat as repeated Press events
        // instead; the reader-level `MODIFIER_ENTER_DEDUP` handles the
        // one case where that's harmful (modifier+Enter → spurious
        // newlines).
        InputEvent::Key(KeyEvent {
            kind: KeyEventKind::Press | KeyEventKind::Repeat,
            code,
            modifiers,
            ..
        }) => {
            // Modal trumps phase handlers when it's installed — /model,
            // /provider, /resume all install a modal and the event loop
            // funnels every keystroke through it until it reports Close.
            if matches!(app.state.phase, UiPhase::Idle) {
                if let Some(modal) = app.active_modal.as_mut() {
                    let action = modal.handle_key(
                        code,
                        modifiers,
                        &mut app.buf,
                        &mut app.state,
                        ctx,
                        renderer,
                    )?;
                    if matches!(action, ModalAction::Close) {
                        app.active_modal = None;
                        // IssueWizard signals a staged title+body via
                        // `ctx.pending_new_issue`. Drain + POST to the
                        // AtomGit API here and echo the created-issue
                        // URL into scrollback. Blocking call — the
                        // wizard is modal so UI freezing briefly is
                        // expected / acceptable.
                        if let Some(draft) = ctx.pending_new_issue.take() {
                            match atomcode_core::atomgit::Client::from_stored_auth().and_then(|c| {
                                c.create_issue(&draft.owner, &draft.repo, &draft.title, &draft.body)
                            }) {
                                Ok(created) => {
                                    let shown_url = created.html_url.clone().unwrap_or_else(|| {
                                        format!(
                                            "https://atomgit.com/{}/{}/issues/{}",
                                            draft.owner, draft.repo, created.number
                                        )
                                    });
                                    renderer.render(UiLine::CommandOutput(format!(
                                        "  [issue] ✔ created #{}: {}\n  {}\n",
                                        created.number, created.title, shown_url,
                                    )));
                                }
                                Err(e) => {
                                    renderer.render(UiLine::CommandOutput(format!(
                                        "  [issue] ✗ create failed: {:#}\n",
                                        e
                                    )));
                                }
                            }
                            renderer.flush();
                        }
                        // WelcomeWizard signals its follow-up via two bool
                        // flags. Drain one, execute it here — the
                        // CodingPlan flow (which internally handles
                        // OAuth login when needed) needs suspend/resume
                        // of raw mode (only event-loop scope can drive
                        // that safely), and opening ProviderWizard is a
                        // Modal-to-Modal swap that needs mutable
                        // `active_modal` access the modals themselves
                        // don't have.
                        if std::mem::take(&mut ctx.pending_run_codingplan) {
                            crate::event_loop::commands::run_codingplan_flow(renderer, ctx)?;
                        }
                        if std::mem::take(&mut ctx.pending_open_provider_wizard) {
                            let pw = crate::modals::ProviderWizard::MainMenu { selected: 0 };
                            app.active_modal = Some(Box::new(pw));
                            if let Some(m) = app.active_modal.as_mut() {
                                m.draw(&app.buf, &app.state, ctx, renderer);
                            }
                            // ProviderWizard owns the next frame now; skip
                            // the idle redraw below so we don't clobber it.
                            return Ok(());
                        }
                        redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                    }
                    return Ok(());
                }
            }
            // PageUp / PageDown / Home / End: scroll the body
            // viewport. Universal across phases — same as a terminal's
            // own scrollback navigation. Only AltScreenRenderer
            // implements these (it owns the alt-screen buffer and
            // host-terminal scrollback is unavailable while we're
            // in alt-screen); other renderers default to no-op so
            // these keys do nothing in retained / plain modes (as
            // before — those rely on the host terminal's native
            // scrollback). We intercept BEFORE phase dispatch so
            // scrolling works in Idle / Streaming alike.
            if let Some(handled) =
                handle_scroll_key(code, modifiers, renderer, &app.buf)
            {
                if handled {
                    return Ok(());
                }
            }
            match app.state.phase {
                UiPhase::Idle => handle_idle_key(app, ctx, renderer, code, modifiers)?,
                UiPhase::Streaming => handle_streaming_key(app, ctx, renderer, code, modifiers)?,
                UiPhase::Approval => handle_approval_key(app, ctx, renderer, code, modifiers)?,
                UiPhase::Suspended => {}
            }
        }
        // Release key events: drop on the floor. Press / Repeat are handled
        // above; Release is noise on Windows.
        InputEvent::Key(_) => {}
    }
    Ok(())
}

/// Try handling a scroll-related key (PageUp/PageDown/Home/End).
/// Returns:
///   - `Some(true)`  → key consumed; caller should skip phase dispatch
///   - `Some(false)` → key was a scroll key but not consumed (e.g.
///     Home/End with text in input buffer, where they should move
///     cursor instead)
///   - `None`        → not a scroll key at all
///
/// AltScreenRenderer is the only renderer that does anything with
/// these calls; the trait defaults are no-op so retained / plain
/// silently fall through and let the existing phase dispatch handle
/// the key (e.g. End-of-line cursor movement during input).
fn handle_scroll_key(
    code: crossterm::event::KeyCode,
    modifiers: crossterm::event::KeyModifiers,
    renderer: &mut dyn crate::render::Renderer,
    buf: &Buffer,
) -> Option<bool> {
    use crossterm::event::{KeyCode, KeyModifiers};
    // Don't intercept Home/End when the user is editing a non-empty
    // buffer — those should move the cursor, not jump scrollback.
    // PageUp/PageDown and Shift+Up/Shift+Down always scroll regardless
    // (they're explicit scroll commands, not in-line editing keys).
    let buf_empty = buf.text.is_empty();
    let has_shift = modifiers.contains(KeyModifiers::SHIFT);
    match code {
        // Page-step. macOS keyboards: Fn+Up / Fn+Down generate
        // PageUp / PageDown. iTerm2 / Windows have dedicated keys.
        KeyCode::PageUp => {
            renderer.scroll_body(-10);
            Some(true)
        }
        KeyCode::PageDown => {
            renderer.scroll_body(10);
            Some(true)
        }
        // Line-step. Shift+Up / Shift+Down is the cross-keyboard
        // alternative for users without a dedicated PageUp/Down key.
        // Bare Up/Down stays bound to input-history navigation
        // (Action::HistoryPrev/Next via key_action::map) for backward
        // compat with retained mode.
        KeyCode::Up if has_shift => {
            renderer.scroll_body(-1);
            Some(true)
        }
        KeyCode::Down if has_shift => {
            renderer.scroll_body(1);
            Some(true)
        }
        KeyCode::Home if buf_empty && modifiers.is_empty() => {
            renderer.scroll_body_to_top();
            Some(true)
        }
        KeyCode::End if buf_empty && modifiers.is_empty() => {
            renderer.scroll_body_to_bottom();
            Some(true)
        }
        _ => None,
    }
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
/// Custom commands are appended after built-in matches; duplicates (custom
/// command with the same name as a built-in) are suppressed.
fn build_menu_items(
    buf: &str,
    commands: &CommandRegistry,
    custom: &atomcode_core::commands::CustomCommandRegistry,
    skill_registry: Option<&std::sync::RwLock<atomcode_core::skill::SkillRegistry>>,
) -> Option<Vec<(String, String)>> {
    if !buf.starts_with('/') {
        return None;
    }

    // Two-level palette for skills.
    //
    // Level 1 (top): the built-in `/skills` entry acts as a gateway —
    // it does NOT expand into individual skills here, so it cannot
    // crowd or collide with built-in / custom commands.
    //
    // Level 2 (sub-mode): once the user has typed `/skills ` (with a
    // trailing space, usually injected by the needs_args path on
    // Enter), this branch fires and lists user-invocable skills under
    // their bare names. Submission rewrites the committed line back
    // to `/skills <name>` so the `skills` arm in execute_slash_command
    // looks up `skills:<name>` in the registry and dispatches.
    if let Some(after) = buf.strip_prefix("/skills ") {
        // Beyond the skill name (user typing skill args) — close menu.
        if after.contains(char::is_whitespace) {
            return None;
        }
        let prefix_lower = after.to_ascii_lowercase();
        let mut items: Vec<(String, String)> = Vec::new();
        if let Some(reg) = skill_registry {
            if let Ok(reg) = reg.read() {
                for skill in reg.user_invocable() {
                    let bare = skill
                        .name
                        .split_once(':')
                        .map(|(_, s)| s)
                        .unwrap_or(skill.name.as_str());
                    if bare.to_ascii_lowercase().starts_with(&prefix_lower) {
                        items.push((bare.to_string(), skill.description.clone()));
                    }
                }
            }
        }
        // Stable order so navigation feels predictable across runs.
        items.sort_by(|a, b| a.0.cmp(&b.0));
        return if items.is_empty() { None } else { Some(items) };
    }

    let rest = &buf[1..];
    // Once a space appears (user is typing args), stop showing menu.
    if rest.contains(char::is_whitespace) {
        return None;
    }
    let prefix_lower = rest.to_ascii_lowercase();
    // Top-level: built-ins (which now include the `/skills` gateway)
    // followed by custom commands. Individual skills are intentionally
    // hidden from this level — users access them via `/skills <name>`.
    let mut matches: Vec<(String, String)> = commands
        .matching_prefix(rest)
        .into_iter()
        .map(|c| (c.name.to_string(), c.desc.to_string()))
        .collect();
    for (name, desc) in custom.command_names_and_descriptions() {
        if name.starts_with(&prefix_lower) && !matches.iter().any(|(n, _)| *n == name) {
            matches.push((name, desc));
        }
    }
    let _ = skill_registry; // referenced only inside the sub-mode branch above
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
    // Suppress while scrolling history — otherwise a recalled `/se…` from
    // history immediately re-pops the menu and traps Up inside it.
    let menu_items = if app.buf.is_in_history() {
        None
    } else {
        build_menu_items(&app.buf.text, &ctx.commands, &ctx.custom_commands, Some(&ctx.skill_registry))
    };
    if let Some(items) = &menu_items {
        // Clamp selection in range.
        if app.menu.selected >= items.len() {
            app.menu.selected = items.len() - 1;
        }
        match (code, modifiers) {
            (KeyCode::Up, _) => {
                // Wrap to the last item (mirror Down's modular wrap below).
                // The menu is fully modal — to reach input history with a
                // partial slash buffer like `/se`, press Esc or Backspace
                // to clear the buffer first.  Previously Up at index 0
                // cleared the buffer and fell through to history nav,
                // which felt like the menu had silently swallowed your
                // text and dumped you somewhere unexpected.
                app.menu.selected = if app.menu.selected == 0 {
                    items.len() - 1
                } else {
                    app.menu.selected - 1
                };
                redraw_with_menu(
                    &app.buf,
                    items,
                    app.menu.selected,
                    &app.state,
                    ctx,
                    renderer,
                );
                return Ok(());
            }
            (KeyCode::Down, _) => {
                app.menu.selected = (app.menu.selected + 1) % items.len();
                redraw_with_menu(
                    &app.buf,
                    items,
                    app.menu.selected,
                    &app.state,
                    ctx,
                    renderer,
                );
                return Ok(());
            }
            (KeyCode::Enter, m) if !m.contains(crossterm::event::KeyModifiers::SHIFT) => {
                // Accept the highlighted command. Two shapes:
                //   * arg-less commands (e.g. /help, /quit, /login) → execute
                //     immediately on Enter, as before.
                //   * commands that require an arg (e.g. /background <task>) →
                //     auto-complete the name + trailing space and park the
                //     cursor so the user types the arg next. A SECOND Enter
                //     (once the arg is filled in) commits normally through
                //     the regular BufferResult::Commit → execute_slash_command
                //     path at the bottom of this function.
                let name = items[app.menu.selected].0.clone();
                let needs_args = ctx
                    .commands
                    .find(&name)
                    .map(|c| c.needs_args)
                    .unwrap_or(false);
                app.menu.selected = 0;

                if needs_args {
                    // Rewrite buffer to `/name ` and park cursor at the end.
                    // Menu rebuilds on next keystroke — with the trailing
                    // space parse_slash_line returns `Some(("name", ""))`
                    // so build_menu_items correctly hides the menu.
                    app.buf.text = format!("/{} ", name);
                    app.buf.cursor = app.buf.text.len();

                    // The `/skills` gateway is special: build_menu_items
                    // recognises the `/skills ` prefix and returns the
                    // second-level palette of skills. Render that
                    // immediately so the user doesn't see the menu blink
                    // out and reappear.
                    if name == "skills" {
                        if let Some(items) = build_menu_items(
                            &app.buf.text,
                            &ctx.commands,
                            &ctx.custom_commands,
                            Some(&ctx.skill_registry),
                        ) {
                            app.menu.selected = 0;
                            redraw_with_menu(
                                &app.buf,
                                &items,
                                0,
                                &app.state,
                                ctx,
                                renderer,
                            );
                            return Ok(());
                        }
                    }

                    redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
                    return Ok(());
                }

                // Sub-mode submit: items in the skills palette carry
                // bare names (e.g. "brainstorming"). Re-prefix with
                // `/skills ` so dispatch routes through the `skills`
                // arm in execute_slash_command, which performs the
                // registry lookup + expand.
                let in_skills_sub_mode = app.buf.text.starts_with("/skills ");
                let committed = if in_skills_sub_mode {
                    format!("/skills {}", name)
                } else {
                    format!("/{}", name)
                };
                renderer.render(UiLine::ClearTransient);
                renderer.render(UiLine::User(committed.clone()));
                app.buf.text.clear();
                app.buf.cursor = 0;
                if let Some((cmd, arg)) = parse_slash_line(&committed) {
                    execute_slash_command(
                        cmd,
                        arg,
                        &mut app.state,
                        ctx,
                        renderer,
                        &mut app.active_modal,
                        &mut app.fixissue_pending,
                        &mut app.fixissue_buffer,
                    )?;
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
    // Any key that's not the Ctrl+C-on-empty-buffer exit path resets the
    // "press again to exit" arming — otherwise the prompt would stick around
    // across arbitrary edits, defeating the point of a short time window.
    if !matches!(result, BufferResult::Exit) {
        app.exit_pending = None;
    }
    match result {
        BufferResult::NoOp => {}
        BufferResult::Redraw => {
            // Rebuild menu after buf change. Same is_in_history gate
            // as above so a HistoryPrev that just landed on `/se…`
            // doesn't immediately re-show the slash menu.
            let items = if app.buf.is_in_history() {
                None
            } else {
                build_menu_items(&app.buf.text, &ctx.commands, &ctx.custom_commands, Some(&ctx.skill_registry))
            };
            if let Some(items) = items {
                if app.menu.selected >= items.len() {
                    app.menu.selected = 0;
                }
                redraw_with_menu(
                    &app.buf,
                    &items,
                    app.menu.selected,
                    &app.state,
                    ctx,
                    renderer,
                );
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
            // Only treat `/name …` as a slash command when `name` is
            // actually registered. Unrecognised `/foo …` (e.g. the user
            // typed `/test 文件下有哪些文件` meaning to *ask about*
            // `/test`, or just `/test` as a question) falls through to
            // the regular message path — better than the old
            // "Unknown command: /foo" dead-end.
            let as_slash = parse_slash_line(&line).filter(|(cmd, _)| {
                ctx.commands.find(cmd).is_some()
                    || ctx.custom_commands.get(&cmd.to_ascii_lowercase()).is_some()
                    || ctx
                        .skill_registry
                        .read()
                        .ok()
                        .and_then(|r| r.get(cmd).map(|s| s.user_invocable))
                        .unwrap_or(false)
            });
            if let Some((cmd, arg)) = as_slash {
                execute_slash_command(
                    cmd,
                    arg,
                    &mut app.state,
                    ctx,
                    renderer,
                    &mut app.active_modal,
                    &mut app.fixissue_pending,
                    &mut app.fixissue_buffer,
                )?;
                if matches!(app.state.phase, UiPhase::Idle) {
                    redraw_after_slash(&app.buf, &app.state, ctx, &app.active_modal, renderer);
                }
            } else {
                ctx.history.push(line.clone());
                // Cache the full expanded form before dispatch. If the
                // user hits Ctrl+C / Esc mid-stream, `handle_streaming_key`
                // takes this Option and restores it to `app.buf.text`
                // so the cancelled message can be edited and resent.
                app.state.last_submitted_message = Some(expanded.clone());
                ctx.agent
                    .cmd_tx
                    .send(AgentCommand::SendMessage(expanded))
                    .ok();
                app.state.on_submit();
                // CodingPlan drift check — fire before every turn sent
                // to a CodingPlan-managed provider, gated by a 15-min
                // cooldown so rapid-fire messages don't spam the API.
                // Non-CodingPlan users skip entirely (zero network).
                if monitor::is_codingplan_provider(&ctx.config.default_provider) {
                    let cooled = ctx
                        .monitor_last_check_at
                        .map(|t| t.elapsed() >= monitor::CHECK_COOLDOWN)
                        .unwrap_or(true);
                    if cooled {
                        ctx.monitor_last_check_at = Some(std::time::Instant::now());
                        monitor::spawn_check(
                            ctx.config.clone(),
                            ctx.model_name.clone(),
                            ctx.monitor_warning.clone(),
                            ctx.wake_tx.clone(),
                        );
                    }
                }
            }
        }
        BufferResult::Exit => {
            // Two-press confirmation: first Ctrl+C on an empty buffer arms
            // the exit; a second Ctrl+C within the window actually exits.
            // Any other keystroke (handled above) resets the arming.
            let now = std::time::Instant::now();
            let armed = app
                .exit_pending
                .is_some_and(|t| now.duration_since(t) <= CTRL_C_EXIT_WINDOW);
            if armed {
                ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
            } else {
                app.exit_pending = Some(now);
                renderer.render(UiLine::CommandOutput(
                    "  (press Ctrl+C again to exit)\n".into(),
                ));
                renderer.flush();
                redraw_idle_plain(&app.buf, &app.state, ctx, renderer);
            }
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
fn redraw_idle_plain(buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
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

/// On Ctrl+C / Esc during streaming, pull the running message back
/// into the input buffer so the user can edit and resend without
/// re-typing. Also drops any type-ahead queue entries: a user
/// pulling the escape cord doesn't want queued messages to
/// auto-fire after the current one dies. The actual `TurnCancelled`
/// event (plus the flip back to Idle + footer redraw) arrives later
/// via the agent round-trip — but the spinner tick at 80ms+ redraws
/// the StreamingBox with `buf.text`, so the restored message shows
/// up within a frame.
fn restore_cancelled_message_to_buf(app: &mut App, renderer: &mut dyn Renderer, ctx: &LoopCtx) {
    app.message_queue.clear();
    if let Some(msg) = app.state.last_submitted_message.take() {
        app.buf.text = msg;
        app.buf.cursor = app.buf.text.len();
        app.menu.selected = 0;
        // Force an immediate StreamingBox repaint so the restored
        // text shows in the input box on this frame, not the next
        // spinner tick.
        draw_spinner_now(
            &mut app.state,
            &app.buf,
            ctx,
            renderer,
            app.message_queue.len(),
            app.menu.selected,
        );
    }
}

fn handle_streaming_key(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Result<()> {
    // Ctrl+O toggles verbose mode (real-time tool output + reasoning visibility)
    if code == KeyCode::Char('o') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        app.state.toggle_tool_output();
        // Show feedback to the user about the current state
        let status = if app.state.show_tool_output {
            "  ○ Verbose mode enabled (tool output + reasoning visible) (Ctrl+O to hide)\n"
        } else {
            "  ◯ Verbose mode disabled (Ctrl+O to show tool output + reasoning)\n"
        };
        renderer.render(UiLine::CommandOutput(status.to_string()));
        renderer.flush();
        draw_spinner_now(
            &mut app.state,
            &app.buf,
            ctx,
            renderer,
            app.message_queue.len(),
            app.menu.selected,
        );
        return Ok(());
    }

    // Ctrl+C always cancels the running turn — highest priority so
    // users have a reliable escape hatch even mid-edit. Also drops
    // the type-ahead queue: a user yanking the escape cord doesn't
    // want queued messages to auto-fire after the current one dies.
    if code == KeyCode::Char('c') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        ctx.agent.cmd_tx.send(AgentCommand::Cancel).ok();
        restore_cancelled_message_to_buf(app, renderer, ctx);
        return Ok(());
    }

    // Esc also cancels a running turn (CC-style). Placed before the
    // menu-nav block so Streaming + menu-open Esc still cancels the
    // stream — mid-stream the higher-value action is "stop the agent",
    // not "clear an unsubmitted slash token" (users can Ctrl+U for that).
    if code == KeyCode::Esc {
        ctx.agent.cmd_tx.send(AgentCommand::Cancel).ok();
        restore_cancelled_message_to_buf(app, renderer, ctx);
        return Ok(());
    }

    // When the menu is active (buf starts with `/`), intercept nav keys
    // so the user can browse candidate commands mid-stream. Execution
    // is still blocked below — Enter falls through to the commit arm,
    // which emits the "disabled while a turn is running" hint.
    let menu_items = build_menu_items(&app.buf.text, &ctx.commands, &ctx.custom_commands, Some(&ctx.skill_registry));
    if let Some(items) = &menu_items {
        if app.menu.selected >= items.len() {
            app.menu.selected = items.len() - 1;
        }
        match code {
            KeyCode::Up => {
                app.menu.selected = app.menu.selected.saturating_sub(1);
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
                return Ok(());
            }
            KeyCode::Down => {
                if app.menu.selected + 1 < items.len() {
                    app.menu.selected += 1;
                }
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
                return Ok(());
            }
            KeyCode::Esc => {
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
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
            if let Some(items) = build_menu_items(&app.buf.text, &ctx.commands, &ctx.custom_commands, Some(&ctx.skill_registry)) {
                if app.menu.selected >= items.len() {
                    app.menu.selected = 0;
                }
            } else {
                app.menu.selected = 0;
            }
            draw_spinner_now(
                &mut app.state,
                &app.buf,
                ctx,
                renderer,
                app.message_queue.len(),
                app.menu.selected,
            );
        }
        BufferResult::Commit(line) => {
            // Slash commands are not queued — they need ctx access
            // that only makes sense between turns. Show a hint and
            // leave the buf alone. Gate strictly on *registered*
            // commands; unrecognised `/foo …` falls through to the
            // type-ahead queue as a regular message.
            let is_known_slash = parse_slash_line(&line)
                .map(|(cmd, _)| ctx.commands.find(cmd).is_some())
                .unwrap_or(false);
            if is_known_slash {
                renderer.render(UiLine::CommandOutput(
                    "  (slash commands are disabled while a turn is running)\n".into(),
                ));
                renderer.flush();
                app.buf.text.clear();
                app.buf.cursor = 0;
                app.menu.selected = 0;
                draw_spinner_now(
                    &mut app.state,
                    &app.buf,
                    ctx,
                    renderer,
                    app.message_queue.len(),
                    app.menu.selected,
                );
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
            draw_spinner_now(
                &mut app.state,
                &app.buf,
                ctx,
                renderer,
                app.message_queue.len(),
                app.menu.selected,
            );
        }
        BufferResult::Exit => {
            // Ctrl+C on empty buf during streaming — treat as cancel
            // (consistent with the explicit Ctrl+C branch above).
            ctx.agent.cmd_tx.send(AgentCommand::Cancel).ok();
            restore_cancelled_message_to_buf(app, renderer, ctx);
        }
    }
    Ok(())
}

fn handle_approval_key(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    code: KeyCode,
    modifiers: crossterm::event::KeyModifiers,
) -> Result<()> {
    // Ctrl+C: first press denies the tool and arms exit confirmation;
    // second press within the window actually exits.
    if code == KeyCode::Char('c') && modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
        let now = std::time::Instant::now();
        let armed = app
            .exit_pending
            .is_some_and(|t| now.duration_since(t) <= CTRL_C_EXIT_WINDOW);
        if armed {
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
        } else {
            // First Ctrl+C: deny the tool and arm the exit confirmation
            app.exit_pending = Some(now);
            renderer.pop_approval_prompt();
            ctx.agent.cmd_tx.send(AgentCommand::DenyTool).ok();
            app.state.on_approval_resolved();
            renderer.render(UiLine::CommandOutput(
                "  (press Ctrl+C again to exit)\n".into(),
            ));
            renderer.flush();
        }
        return Ok(());
    }

    // Any other key resets the exit confirmation
    app.exit_pending = None;

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
    app.state.on_approval_resolved();
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
            renderer.render(UiLine::CommandOutput(format!("  最新版本: {}\n", version)));
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
    pending_tools: &mut std::collections::HashMap<String, (String, String, bool)>,
    ctx: &mut LoopCtx,
    fixissue_pending: &mut Option<atomcode_core::atomgit::IssueRef>,
    fixissue_buffer: &mut String,
    reasoning_buffer: &mut String,
) {
    match ev {
        AgentEvent::TextDelta(text) => {
            let visible = think.feed(&text);
            if !visible.is_empty() {
                if fixissue_pending.is_some() {
                    fixissue_buffer.push_str(&visible);
                }
                renderer.render(UiLine::AssistantText(visible));
                renderer.flush();
            }
        }
        AgentEvent::ReasoningDelta(text) => {
            // Display reasoning/thinking content in verbose mode (Ctrl+O)
            // Only show when the user has enabled it
            if state.show_reasoning {
                reasoning_buffer.push_str(&text);
                // Flush on newline or when buffer gets large
                if reasoning_buffer.contains('\n') || reasoning_buffer.len() > 80 {
                    let output = std::mem::take(reasoning_buffer);
                    // Render as gray/dimmed text with automatic line wrapping
                    renderer.render(UiLine::ReasoningText(output));
                    renderer.flush();
                }
            }
        }
        AgentEvent::ToolCallStreaming { name, .. } => {
            state.on_tool_call_streaming(&display_tool_name(&name));
        }
        AgentEvent::ToolCallStarted {
            id,
            name,
            arguments,
        } => {
            // Emit the ▸ line immediately so users can see what command
            // is running, especially for long-running bash commands.
            // The line will be shown before the command completes.
            let detail = format_tool_detail(&name, &arguments);
            let display = display_tool_name(&name);

            // Close any in-flight assistant line before emitting the tool call.
            renderer.render(UiLine::AssistantLineBreak);
            renderer.render(UiLine::ToolCallInFlight {
                id: id.clone(),
                name: display.clone(),
                detail: detail.clone(),
            });
            renderer.flush();

            // Mark as rendered so ToolCallResult doesn't emit it again.
            pending_tools.insert(id, (display.clone(), detail, true));
            state.on_tool_call_started(&display);
        }
        AgentEvent::ToolOutputChunk { call_id: _, chunk } => {
            // Display real-time tool output (e.g., bash stdout/stderr)
            // Only show when the user has enabled it via Ctrl+O
            if state.show_tool_output {
                // Append to the scrollback as command output
                renderer.render(UiLine::CommandOutput(chunk));
                renderer.flush();
            }
        }
        AgentEvent::ToolCallResult {
            call_id,
            name,
            output,
            success,
            ..
        } => {
            // Close any in-flight assistant line before emitting the pair.
            renderer.render(UiLine::AssistantLineBreak);
            // Freeze the animated in-flight tool-call row to its final
            // static `▸` icon before the `⎿ result` body row lands beneath
            // it. Pass the call_id so we only freeze if the inflight_tool matches.
            // This prevents freezing a different tool's spinner when multiple
            // tools are in flight (e.g., WriteFile result arrives while Bash spinner is active).
            renderer.render(UiLine::ToolCallCommit {
                call_id: Some(call_id.clone()),
            });

            // Prefer the display-name we stored at ToolCallStarted time;
            // fall back to converting the raw name if we missed the Start
            // (e.g. protocol surfaced a Result without a matching Start).
            let (display_name, detail, call_rendered) = pending_tools
                .remove(&call_id)
                .unwrap_or_else(|| (display_tool_name(&name), String::new(), false));

            // Filter empty tool names (model occasionally emits malformed
            // tool calls with "" as the name; agent surfaces the error via
            // a ToolCallResult but there's no useful ▸ line to render).
            let safe_name = if display_name.is_empty() {
                "(invalid)".to_string()
            } else {
                display_name
            };

            // Only emit the tool-call line here if ApprovalNeeded didn't
            // already render it — otherwise we'd print it twice.
            if !call_rendered {
                renderer.render(UiLine::ToolCall {
                    name: safe_name.clone(),
                    detail: detail.clone(),
                });
            }
            let summary = summarise(&output, success);
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
            // Show hint for bash commands if real-time output is disabled
            // Display AFTER the result so user sees the command first
            if name == "bash" && !state.show_tool_output {
                renderer.render(UiLine::CommandOutput(
                    "  ◯ Press Ctrl+O to show real-time output\n".to_string()
                ));
            }
            renderer.flush();
            let _ = name;
        }
        AgentEvent::ApprovalNeeded {
            tool_name, call, ..
        } => {
            // Emit the `▸ Tool(detail)` row BEFORE the approval prompt
            // so the user sees what they're approving.
            let display = display_tool_name(&tool_name);
            let detail = format_tool_detail(&tool_name, &call.arguments);
            
            // Check if ToolCallStarted already rendered this tool call as a
            // dynamic ToolCallInFlight spinner. If so, we need to freeze it
            // to a static `▸` row before showing the approval prompt.
            if let Some(entry) = pending_tools.get_mut(&call.id) {
                let (disp, det, rendered) = entry;
                if *rendered {
                    // ToolCallInFlight is animating — commit it to a static row
                    // so the approval prompt appears below a frozen `▸ Bash(...)`.
                    // Pass the call_id to ensure we only freeze the matching tool.
                    renderer.render(UiLine::ToolCallCommit {
                        call_id: Some(call.id.clone()),
                    });
                } else {
                    // Not yet rendered, emit it now
                    renderer.render(UiLine::ToolCall {
                        name: disp.clone(),
                        detail: det.clone(),
                    });
                    *rendered = true;
                }
            } else {
                // No entry from ToolCallStarted, render and insert
                renderer.render(UiLine::ToolCall {
                    name: display.clone(),
                    detail: detail.clone(),
                });
                pending_tools.insert(call.id.clone(), (display.clone(), detail.clone(), true));
            }
            
            renderer.render(UiLine::ApprovalPrompt {
                tool: display.clone(),
                detail: detail.clone(),
            });
            renderer.flush();
            atomcode_core::notify::notify(
                &ctx.config.notifications,
                atomcode_core::notify::NotificationEvent::ApprovalNeeded(
                    atomcode_core::notify::ApprovalNotification {
                        tool_name: &display_tool_name(&tool_name),
                        detail: Some(&format_tool_detail(&tool_name, &call.arguments)),
                        working_dir: Some(&ctx.working_dir),
                    },
                ),
            );
            state.on_approval_needed(&tool_name);
        }
        AgentEvent::PhaseChange(AgentPhase::Thinking) => state.on_thinking(),
        AgentEvent::PhaseChange(AgentPhase::CallingTool(name)) => {
            state.on_tool_call_streaming(&display_tool_name(&name));
        }
        AgentEvent::PhaseChange(_) => {}
        AgentEvent::TurnComplete {
            duration,
            total_tokens,
            turn_count,
            tool_call_count,
            stop_reason,
            messages,
        } => {
            atomcode_core::notify::notify(
                &ctx.config.notifications,
                atomcode_core::notify::NotificationEvent::TurnFinished(
                    atomcode_core::notify::TurnNotification {
                        duration,
                        turn_count,
                        tool_call_count,
                        total_tokens: Some(total_tokens),
                        stop_reason,
                        working_dir: Some(&ctx.working_dir),
                    },
                ),
            );
            renderer.render(UiLine::AssistantLineBreak);
            pending_tools.clear();
            let done = state.next_done_label();
            let dur = crate::render::fmt_dur(duration);
            let label = format!(
                "✓ {} · {} rounds · {} tools · {} · {} tokens",
                done, turn_count, tool_call_count, dur, total_tokens
            );
            renderer.render(UiLine::TurnSeparator { label });
            renderer.flush();
            state.on_turn_complete();

            // Reset the think stripper between turns. If the previous turn
            // left an unclosed `<think>` in flight (cancelled mid-stream,
            // model never emitted `</think>`, provider switch that doesn't
            // use `<think>` tags like Kimi thinking-mode via reasoning_content),
            // the stripper stays `inside=true` and silently swallows every
            // TextDelta of the NEXT turn — user sees blank assistant bubbles
            // while datalog proves the model did return text.
            think.reset();

            // Clear reasoning buffer between turns
            reasoning_buffer.clear();

            // Persist session after every completed turn so /resume can
            // find it after a clean exit — the whole point of sessions.
            persist_current_session(ctx, messages);

            // fixissue post-run side effects — only on successful TurnComplete
            // (TurnCancelled / Error arms below clear `fixissue_pending`
            // without posting). Takes the IssueRef out so only this turn's
            // completion triggers the post-back.
            if let Some(issue_ref) = fixissue_pending.take() {
                let body = std::mem::take(fixissue_buffer);
                if body.trim().is_empty() {
                    renderer.render(UiLine::CommandOutput(format!(
                        "  [fixissue] agent produced no text; skipping comment + label on issue #{}\n",
                        issue_ref.number
                    )));
                } else {
                    match atomcode_core::atomgit::fixissue::post_completion(&issue_ref, &body) {
                        Ok(()) => renderer.render(UiLine::CommandOutput(format!(
                            "  [fixissue] ✔ posted summary + applied 'fixed' label to issue #{}\n",
                            issue_ref.number
                        ))),
                        Err(e) => renderer.render(UiLine::CommandOutput(format!(
                            "  [fixissue] ✗ post-back failed (local fix still saved): {:#}\n",
                            e
                        ))),
                    }
                }
                renderer.flush();
            }
        }
        AgentEvent::TurnCancelled { messages } => {
            atomcode_core::notify::notify(
                &ctx.config.notifications,
                atomcode_core::notify::NotificationEvent::TurnFinished(
                    atomcode_core::notify::TurnNotification {
                        duration: state.turn_elapsed().unwrap_or_default(),
                        turn_count: 0,
                        tool_call_count: pending_tools.len(),
                        total_tokens: None,
                        stop_reason: atomcode_core::agent::TurnStopReason::Cancelled,
                        working_dir: Some(&ctx.working_dir),
                    },
                ),
            );
            // Render any in-flight tool calls that never got a result
            // as "(cancelled)" so the user sees what was mid-flight.
            for (_id, (name, detail, call_rendered)) in pending_tools.drain() {
                let safe_name = if name.is_empty() {
                    "(invalid)".into()
                } else {
                    name
                };
                if !call_rendered {
                    renderer.render(UiLine::ToolCall {
                        name: safe_name,
                        detail,
                    });
                }
                renderer.render(UiLine::ToolResult {
                    success: false,
                    summary: "(cancelled)".into(),
                });
            }
            renderer.render(UiLine::TurnCancelled);
            renderer.flush();
            state.on_turn_cancelled();
            // Cancellation = agent didn't finish; don't post a comment
            // against an incomplete "fix".
            fixissue_pending.take();
            fixissue_buffer.clear();
            // Same reset rationale as TurnComplete: a cancelled turn is the
            // single most common way for `<think>` to go unclosed, so this
            // branch is even more important for the stripper's hygiene.
            think.reset();
            // Save what we did have — a user who Ctrl+C'd mid-stream
            // should still be able to /resume the cleaned conversation.
            persist_current_session(ctx, messages);
        }
        AgentEvent::Error(e) => {
            renderer.render(UiLine::Error(e));
            renderer.flush();
            fixissue_pending.take();
            fixissue_buffer.clear();
            state.on_error();
            // Same reset rationale as TurnComplete / TurnCancelled — an
            // aborted turn is another way to leave `<think>` half-open.
            think.reset();
        }
        AgentEvent::TokenUsage(u) => {
            state.prompt_tokens += u.prompt_tokens;
            state.completion_tokens += u.completion_tokens;
            state.cached_tokens += u.cached_tokens;
            state.total_tokens += u.completion_tokens;
        }
        AgentEvent::WorkingDirChanged(new_dir) => {
            // Fires when a tool (change_dir / bash cd) or an AgentCommand::ChangeDir
            // mutated the shared cwd. Sync the footer's view so the status row
            // reflects the new directory on the next redraw (spinner tick if
            // streaming, idle redraw after turn complete). Without this the
            // footer is stuck on the old path until the user types `/cd` or
            // restarts the session.
            if ctx.working_dir != new_dir {
                ctx.previous_dir = Some(std::mem::replace(&mut ctx.working_dir, new_dir.clone()));
                commands::push_recent_dir(&mut ctx.recent_dirs, new_dir);
            }
        }
        AgentEvent::ContextStats {
            system_tokens,
            sent_tokens,
            dropped_tokens: _,
            working_set_tokens: _,
            total_messages,
            tool_defs_tokens,
            cold_zone_tokens,
            ctx_window,
            ctx_name,
            system_prompt,
        } => {
            state.on_context_stats(
                system_tokens,
                sent_tokens,
                tool_defs_tokens,
                cold_zone_tokens,
                total_messages,
                ctx_window,
                &ctx_name,
                &system_prompt,
            );
            // If `/context` is waiting for fresh stats, the rich emission
            // (ctx_window > 0) is the signal to render. Narrow emissions
            // from TurnRunner leave ctx_window at 0 and must not trigger
            // a report render (they'd race ahead of the pending refresh
            // and print partial data). Clears the flag on fire so a
            // single dispatch yields a single render even when multiple
            // rich emissions follow (e.g. inside a long multi-round turn).
            if ctx_window > 0 {
                if let Some(show_prompt) = state.pending_context_render.take() {
                    renderer.render(UiLine::CommandOutput(commands::render_context_report(
                        state,
                        ctx,
                        show_prompt,
                    )));
                    renderer.flush();
                }
            }
        }
        AgentEvent::SubAgentProgress { .. } => {}
        AgentEvent::BackgroundComplete { summary, files_edited, turns, success } => {
            let header = if success {
                format!("  Background task complete ({} turn{}):\n", turns, if turns == 1 { "" } else { "s" })
            } else {
                format!("  Background task failed after {} turn{}:\n", turns, if turns == 1 { "" } else { "s" })
            };
            let mut body = String::from(&header);
            body.push_str("  ");
            body.push_str(&summary);
            if !body.ends_with('\n') {
                body.push('\n');
            }
            if !files_edited.is_empty() {
                body.push_str("  Files edited:\n");
                for f in &files_edited {
                    body.push_str(&format!("    - {}\n", f));
                }
            }
            if success {
                renderer.render(UiLine::CommandOutput(body));
            } else {
                renderer.render(UiLine::Error(body));
            }
            renderer.flush();
        }
    }
}

/// Copy the latest conversation into `ctx.current_session`, auto-name
/// the session from the first user message when it's still at its
/// default label, and write the session file to disk. Called on every
/// TurnComplete and TurnCancelled so `/resume` can find the
/// conversation after a quit. No-op when the conversation is empty
/// (don't save a blank session).
fn persist_current_session(
    ctx: &mut LoopCtx,
    messages: Vec<atomcode_core::conversation::message::Message>,
) {
    if messages.is_empty() {
        return;
    }
    ctx.current_session.messages = messages;
    ctx.current_session.touch();
    // Rename from the generated default (`default` or `session-<ts>`)
    // to the first user message's first line, truncated. Keeps the
    // `/resume` picker scannable.
    let should_rename =
        ctx.current_session.name == "default" || ctx.current_session.name.starts_with("session-");
    if should_rename {
        use atomcode_core::conversation::message::Role;
        if let Some(first_user) = ctx
            .current_session
            .messages
            .iter()
            .find(|m| matches!(m.role, Role::User))
        {
            if let Some(text) = first_user.text() {
                let name: String = text.lines().next().unwrap_or("").chars().take(40).collect();
                if !name.is_empty() {
                    ctx.current_session.name = name;
                }
            }
        }
    }
    let _ = ctx.session_manager.save(&ctx.current_session);
}

/// Build the persistent status line shown directly below the input box.
/// Pulls model name from ctx, cwd from ctx.working_dir (with $HOME
/// collapsed to `~`), and running token count from state.
pub(crate) fn build_status(state: &UiState, ctx: &LoopCtx) -> crate::render::StatusLine {
    let cwd = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
    // Priority:
    //   1. No provider configured + not logged in — show "configure" nudge.
    //      This wins over the upgrade hint because without a provider the
    //      app literally cannot answer any message; the user needs to know
    //      why before they're told to upgrade.
    //   2. Upgrade-available hint (existing behavior).
    //   3. None.
    let no_provider =
        ctx.config.providers.is_empty() && atomcode_core::auth::get_stored_auth().is_none();
    // Priority: no-provider (Warning red) > CodingPlan drift monitor
    // (both ModelMissing and StaleList render as Warning red — model
    // list drift is an actionable UX event worth the same visual weight
    // as "active model gone") > upgrade banner (Info dim).
    // Only one hint can render at a time (right-aligned on the status row).
    let hint: Option<(String, crate::render::HintSeverity)> = if no_provider {
        Some((
            "no provider · /provider to configure".into(),
            crate::render::HintSeverity::Warning,
        ))
    } else if let Some(warning) = ctx.monitor_warning.lock().ok().and_then(|g| g.clone()) {
        Some((warning.display_text(), crate::render::HintSeverity::Warning))
    } else {
        ctx.update_hint
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|v| {
                (
                    format!("↑ {} 使用/upgrade升级", v),
                    crate::render::HintSeverity::Info,
                )
            })
    };
    // Pre-configure, `ctx.model_name` is a dummy from the startup fallback
    // (empty string or "not-configured") — showing that raw in the status
    // line reads as a glitch. Replace with an explicit placeholder so the
    // user sees the state, not a rendering artifact.
    let model = if no_provider {
        "(not configured)".to_string()
    } else {
        ctx.model_name.clone()
    };
    crate::render::StatusLine {
        model,
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
    let menu = build_menu_items(&buf.text, &ctx.commands, &ctx.custom_commands, Some(&ctx.skill_registry)).map(|items| {
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
    let mut out = format!("{}{}", base, state.ellipsis());
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
            get_str("file_path")
                .map(|p| basename(&p))
                .unwrap_or_default()
        }
        "read_symbol" => {
            let sym = get_str("symbol").unwrap_or_default();
            let file = get_str("file_path")
                .map(|p| basename(&p))
                .unwrap_or_default();
            if sym.is_empty() {
                file
            } else if file.is_empty() {
                sym
            } else {
                format!("{} in {}", sym, file)
            }
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
        "list_directory" | "change_dir" => get_str("path").unwrap_or_else(|| ".".into()),
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
                (Some(f), Some(p)) => format!(
                    "{}: {}",
                    basename(&f),
                    crate::width::truncate_with_ellipsis(&p, 60)
                ),
                (Some(f), None) => basename(&f),
                (None, Some(p)) => crate::width::truncate_with_ellipsis(&p, 100),
                _ => String::new(),
            }
        }
        "use_skill" => get_str("name").unwrap_or_default(),
        _ => {
            // Fallback: try common single-key args that make sense as detail.
            for key in [
                "file_path",
                "path",
                "file",
                "pattern",
                "query",
                "url",
                "name",
                "symbol",
                "command",
            ] {
                if let Some(s) = get_str(key) {
                    return crate::width::truncate_with_ellipsis(&s, 100);
                }
            }
            String::new()
        }
    }
}

pub(crate) fn summarise(output: &str, success: bool) -> String {
    let first = output.lines().next().unwrap_or("(no output)");
    let n = output.lines().count();
    // Failures get a larger budget because the first line is usually
    // diagnostic ("Error: old_string not found in /mnt/d/.../f_store.")
    // and the path is the load-bearing piece of info — silently
    // chopping it at 80 cols turned `f_store` into `f_stor` and made
    // the agent loop on the wrong file. 200 cols comfortably fits a
    // typical WSL-style absolute path; anything beyond that probably
    // is too long to read inline anyway.
    let budget = if success { 80 } else { 200 };
    // `truncate_with_ellipsis` (instead of bare `truncate_to_width`)
    // so that whenever the budget IS exceeded, the user / agent sees
    // a `…` marker — silent mid-token chops were the actual UX bug.
    let trimmed = crate::width::truncate_with_ellipsis(first, budget);
    if n > 1 {
        format!("{} ({} lines)", trimmed, n)
    } else {
        trimmed
    }
}

// SessionPicker tests moved alongside the struct in
// `crate::modals::session_picker::tests`.
