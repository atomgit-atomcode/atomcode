// crates/atomcode-tuix/src/render/mod.rs
pub mod cell;
pub(crate) mod diff;
pub mod interaction;
pub mod mascot;
pub mod plain;
pub mod qr;
pub mod retained;
pub mod screen;
pub mod theme;
pub mod welcome_tips;
pub mod worker;

use std::time::Duration;

/// Boundary marker for an originated message in the body buffer. Drives
/// "jump to prev/next message" navigation keys. Marked at push time;
/// kept in sync when body_lines drains from the front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
}

#[derive(Debug, Clone, Copy)]
pub struct MessageMark {
    /// Index into the renderer's visible body buffer (`Vec<Vec<Cell>>`).
    /// Drives "jump to message" — viewport_top is compared against this.
    pub line_idx: usize,
    pub kind: MarkKind,
}

/// Semantic line to render. Renderer implementations translate this to bytes.
///
/// Permanent lines (User, Assistant, ToolCall, ToolResult, Diff, Approval,
/// Error, Blank) all enter scrollback. Spinner and InputPrompt are transient.
#[derive(Debug, Clone)]
pub enum UiLine {
    Welcome {
        model: String,
        working_dir: String,
    },
    User(String),
    /// A user message and its image echoes rendered as one append-only
    /// group. Retained renderers must not commit a temporary spacer between
    /// the text and attachments because that scroll cannot be undone.
    UserWithAttachments {
        text: String,
        attachments: Vec<usize>,
    },
    AssistantText(String),
    /// LLM reasoning/thinking content (displayed in gray/dimmed style)
    ReasoningText(String),
    AssistantLineBreak,
    ToolCall {
        name: String,
        detail: String,
    },
    /// Animated tool-call line. Pushed on `AgentEvent::ToolCallStarted`
    /// instead of the static `ToolCall`, so the user sees the call land
    /// the moment the model commits to it AND its leading icon ticks in
    /// lockstep with the footer spinner via the live-row mechanism (see
    /// `RetainedRenderer::push_or_update_inflight_tool`). Switched to a
    /// static `▸` icon by `ToolCallCommit` once the matching result
    /// lands, freeing the live-row slot for the spinner to resume.
    ToolCallInFlight {
        id: String,
        name: String,
        detail: String,
        /// Optional ephemeral hint rendered as part of the inflight strip
        /// (e.g. the bash "Press Ctrl+o …" line). Kept INSIDE the strip so
        /// the spinner tick / commit erase cover it atomically — emitting it
        /// as a separate body row breaks the "inflight strip = body tail"
        /// invariant and orphans the spinner glyph on commit.
        hint: Option<String>,
    },
    /// Freeze the most recent `ToolCallInFlight` row to its final
    /// static `▸` icon. Emitted right before `ToolResult` so the
    /// bottom body row stops animating exactly when the result is
    /// about to be appended below it.
    /// If `call_id` is provided, only commits if the inflight_tool matches.
    ToolCallCommit {
        call_id: Option<String>,
    },
    /// Push a parallel-tool batch as a live multi-row group: one
    /// header line + N child rows (one per tool call), all visible
    /// from the start. Subsequent `ToolGroupChildUpdate` events find
    /// child rows by `call_id` and update them in place (CC-style
    /// ✓ light-up). The group is "live" only as long as it remains
    /// the bottom of body_lines; any other body push freezes it (in
    /// place forever, but no further child updates take effect).
    ToolGroupRender {
        batch_id: String,
        header: String,
        children: Vec<ToolGroupChild>,
    },
    /// Update one child row inside an active live-group. Renderer
    /// finds the row keyed by `call_id` and CUPs to its terminal
    /// position to rewrite. Falls back to no-op if the group has been
    /// frozen (other content was pushed below it).
    ToolGroupChildUpdate {
        batch_id: String,
        call_id: String,
        new_text: String,
    },
    /// One-shot summary line for a completed tool batch — rendered
    /// with bold + brand-color emphasis so it stands out as the
    /// "this is what happened" anchor (mirrors CC's task-completion
    /// summary visual). Used by both ToolBatchCompleted and
    /// SubAgentDispatchEnd.
    ToolGroupSummary {
        text: String,
    },
    /// Live Task/Team agent projection rendered in the conversation body when
    /// the persistent Todo/Tasks panel already owns the footer. Repeated events
    /// with the same `call_id` replace the existing rows in place; `finished`
    /// freezes the final snapshot into conversation history.
    AgentGroup {
        progress: SubtaskProgress,
        finished: bool,
    },
    /// Freeze every live conversation-body Agent block at its current
    /// snapshot, used when Todo stops owning the footer and Task/Team progress
    /// returns to the fixed panel.
    AgentGroupsFreeze,
    ToolResult {
        success: bool,
        summary: String,
        /// Optional edit statistics appended to the first summary row.
        /// Renderers color additions/removals with the active diff theme.
        diff_stats: Option<(usize, usize)>,
    },
    DiffLine {
        added: bool,
        text: String,
    },
    /// A batch of diff lines emitted in a single render call. Use this
    /// instead of N individual `DiffLine` renders when a tool result
    /// carries many changed lines — each `DiffLine` triggers a full
    /// erase_footer + redraw_footer cycle, so 50 diff lines translate
    /// into 50 footer redraws and tens of KB of ANSI, blocking the
    /// event loop long enough to freeze the spinner. `DiffBlock` does
    /// one erase + N writes + one redraw.
    DiffBlock(Vec<DiffEntry>),
    /// Edit-tool diff rows whose statistics are already shown inline in
    /// the preceding `ToolResult`; avoids a duplicate standalone summary.
    EditDiffBlock(Vec<DiffEntry>),
    Error(String),
    /// Non-fatal advisory line (yellow). Visually distinct from `Error`
    /// so the user can tell "we saw something fishy and want you to
    /// know" apart from "the turn died." Currently used by the OpenAI
    /// provider's truncation detector.
    Warning(String),
    /// Dim, non-bold informational line with no forced prefix. Used for
    /// notable but non-alarming status lines that should not read as
    /// warnings or errors — e.g. a rate-limit pause announcement. Rendered
    /// in DarkGrey (same palette as `CompactionMark`) so it recedes into
    /// the scrollback without grabbing attention.
    Muted(String),
    /// A compaction occurred here — a dim, left-aligned dash rule marking the
    /// scrollback point where history was folded/summarized. Unified across
    /// auto-compaction and manual `/compact`. Payload is the localized label
    /// (e.g. "已压缩 · 摘要 12 条 · ~48.2K→~9.1K"); renderers wrap it in a dash
    /// rule honoring the terminal's unicode caps. Permanent (enters scrollback).
    CompactionMark(String),
    TurnCancelled,
    TurnComplete,
    /// Legacy single-line spinner (kept for tests / PlainRenderer fallback).
    /// During Streaming the event loop emits `StreamingBox` instead so the
    /// spinner sits ABOVE the input box rather than inside it.
    Spinner {
        frame: &'static str,
        label: String,
    },
    /// Clear the current transient line (prepares for a permanent write).
    ClearTransient,
    /// Draw the input prompt "> " + current buffer (transient, idle).
    /// When `menu` is Some, a command palette is drawn above the box.
    /// `cursor_byte` is a byte offset into `buf` — the renderer wraps
    /// `buf` to the available input width and derives the 2D cursor
    /// position (row, col) itself so the input box can grow multi-line
    /// when the user exceeds a single row.
    InputPrompt {
        buf: String,
        cursor_byte: usize,
        menu: Option<MenuPayload>,
        status: StatusLine,
        /// Marker numbers (`N` from `[Image #N]`) that actually have
        /// image bytes ready to ship — either freshly attached this
        /// turn or recalled from cache via arrow-up. Renderers cross-
        /// reference each marker against `buf` and draw a `└ [Image #N]`
        /// preview row for the intersection right under the input box,
        /// so users can tell "real attachment" from "literal text" at
        /// a glance, before submit. Empty means no preview rows. Only
        /// the main idle / streaming compose paths populate this; modal
        /// flows that reuse `InputPrompt` for text entry pass `Vec::new()`.
        attachments: Vec<usize>,
    },
    /// Streaming chrome: spinner line above a (possibly multi-line)
    /// input box. Same `cursor_byte` semantics as `InputPrompt`.
    /// When `menu` is Some (user typed `/` into the type-ahead buffer
    /// mid-stream), the slash-command palette is drawn above the box
    /// in place of the spinner — same rendering path as `InputPrompt`.
    StreamingBox {
        buf: String,
        cursor_byte: usize,
        frame: &'static str,
        label: String,
        status: StatusLine,
        menu: Option<MenuPayload>,
        /// Same semantics as `InputPrompt::attachments` — type-ahead
        /// during streaming can carry pasted attachments too, so the
        /// preview path needs to fire here as well.
        attachments: Vec<usize>,
    },
    /// User pressed Enter: commit the current InputPrompt to scrollback.
    InputCommit,
    /// Slash-command output (arbitrary text, already sanitised by caller).
    CommandOutput(String),
    /// Image-attachment echo (`└ [Image #N]`). Emitted right after the
    /// `UiLine::User` row that contains the matching `[Image #N]`
    /// marker, so each renderer can align the `└` glyph at the same
    /// column as the `[` of the marker in the user message above
    /// (col 2). A dedicated variant rather than `CommandOutput` so
    /// alignment stays consistent across renderers — retained's
    /// `push_body_text` auto-prefixes PAD_COL (2 spaces) but
    /// alt-screen's `push_command_output` does not, so the same
    /// CommandOutput payload would land at col 2 in one and col 4
    /// (or col 0) in the other.
    ImageAttachment(usize),
    /// One-line success notice for vision-preprocessor OCR. Renders as
    /// `{msg}  {model}` where `msg` uses the default text style and
    /// `model` is bold only (no themed colour) so the VL model identity
    /// stands out from the notice text without a loud accent hue — just
    /// emphasis, per user request. `model` is the bare model name
    /// (vendor prefix stripped), not the `config.providers` key.
    /// The actual VL description is intentionally NOT shown in the UI;
    /// it still rides into conversation history for the main model.
    VisionPreprocessSuccess {
        msg: String,
        model: String,
    },
    /// A visible separator between turns: `────── {label} ──────`.
    TurnSeparator {
        label: String,
    },
    /// Semantic `/diff` overlay. Spans carry theme roles instead of embedded
    /// ANSI so retained rendering can clip safely and plain rendering can
    /// ignore the transient panel.
    DiffPanel {
        title: DiffPanelRow,
        rows: Vec<DiffPanelRow>,
        footer: String,
        win_width: u16,
        win_height: u16,
    },
    /// Clear the overlay modal and restore the underlying frame.
    ModalOverlayClear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffPanelTone {
    Default,
    Muted,
    Brand,
    Add,
    Remove,
    Warning,
    /// The `/resume`-picker selection colour: theme-aware bold foreground (cyan
    /// on dark, magenta on light), no reverse-video and no background.
    Highlight,
}

#[derive(Debug, Clone)]
pub struct DiffPanelSpan {
    pub text: String,
    pub tone: DiffPanelTone,
    pub bold: bool,
}

impl DiffPanelSpan {
    pub fn new(text: impl Into<String>, tone: DiffPanelTone) -> Self {
        Self {
            text: text.into(),
            tone,
            bold: false,
        }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
}

#[derive(Debug, Clone)]
pub struct DiffPanelRow {
    pub spans: Vec<DiffPanelSpan>,
}

impl DiffPanelRow {
    pub fn new(spans: Vec<DiffPanelSpan>) -> Self {
        Self { spans }
    }
}

pub trait Renderer: Send {
    /// Emit one UiLine. Implementations may batch internally; call `flush()` to force.
    fn render(&mut self, line: UiLine);
    fn flush(&mut self);
    /// Shutdown: disable bracketed paste, disable raw mode, etc.
    fn shutdown(&mut self);
    /// Forget all cached rendering state (footer rows, last footer snapshot,
    /// assistant-text mid-line buffer, markdown parser) AND clear the
    /// physical terminal screen. Used by callers that hand control back
    /// to a non-TUI process (e.g. the blocking OAuth flow in /login)
    /// and then want a clean slate — without this, the next render
    /// tries to `erase_footer` at a position the terminal cursor is no
    /// longer at, corrupting every subsequent ANSI cursor move.
    fn reset(&mut self);

    /// Wipe the physical terminal with `\x1b[2J\x1b[H` and flush.
    /// **Does not** touch cached footer/stream state — callers that want a
    /// full state wipe should call `reset()` instead. Use this when only
    /// the visible scrollback should be cleared (e.g. the `/clear`
    /// command after which the footer immediately redraws).
    fn clear_screen(&mut self);

    /// Open a single DECSET 2026 synchronized-output envelope spanning the
    /// burst of operations up to the matching `end_sync()`. Used by the
    /// `/resume` replay so the screen wipe + full-transcript re-emit paint
    /// as ONE atomic update on capable hosts instead of visibly blanking
    /// and re-scrolling (the flicker). Between the two calls, per-frame
    /// envelopes are suppressed so they can't end the batch early.
    ///
    /// Default no-op: renderers that don't use synchronized output
    /// (PlainRenderer, pipe mode, tests) just emit their writes as usual.
    fn begin_sync(&mut self) {}

    /// Close the envelope opened by `begin_sync()` (after landing the final
    /// frame inside it). Default no-op. Must be paired with `begin_sync()`.
    fn end_sync(&mut self) {}

    /// Rebuild an initial `/resume` replay in memory instead of eagerly
    /// scrolling every historical row through the host terminal.
    fn begin_initial_history_replay(&mut self) {}

    /// Emit the bounded suffix retained by `begin_initial_history_replay`.
    fn end_initial_history_replay(&mut self) {}

    /// Configure the terminal projection cap. `None` preserves full replay.
    fn set_history_replay_max_rows(&mut self, _max_rows: Option<usize>) {}

    /// Suppress automatic clipboard copy during history replay so that
    /// `/resume`, `/undo` and `atomcode -c` don't overwrite the user's
    /// clipboard or inject stale "Copied" hints into the replay output
    /// (issue #699). Default no-op — only the retained renderer implements
    /// this.
    fn set_suppress_auto_copy(&mut self, _suppress: bool) {}

    /// Enable/disable the code-block auto-copy feature (issue #699). Default
    /// OFF (opt-in via `config.ui.auto_copy_code_blocks` / `ATOMCODE_AUTO_COPY`),
    /// set once at startup. Default no-op — only the retained renderer implements it.
    fn set_auto_copy_enabled(&mut self, _enabled: bool) {}

    /// Set the terminal window/tab title. Default no-op — only the
    /// interactive retained renderer implements this, so title bytes never
    /// leak into piped/plain (non-TTY) output. `title` is already sanitised
    /// (see `crate::title::session_terminal_title`).
    fn set_title(&mut self, _title: String) {}

    /// Hand the terminal off to a non-TUI child process (blocking OAuth
    /// flow, `/shell`, etc.): disable raw mode + bracketed paste, finish
    /// any pending writes. After this returns, the child is free to use
    /// the terminal in cooked mode; `resume_from_external()` must be
    /// called before any further `render()` calls.
    fn suspend_for_external(&mut self);

    /// Take the terminal back after `suspend_for_external()`: re-enable
    /// raw mode + bracketed paste AND call `reset()` to wipe the cached
    /// state (the child wrote to stdout in cooked mode, so our cursor
    /// tracking is now lying).
    fn resume_from_external(&mut self);

    /// Paint any throttled payload that's been sitting in the deferred
    /// queue past its throttle window. Called from the event loop on a
    /// ~50fps timer so the "trailing edge" of a burst of input renders
    /// actually lands — without this tick a lone stale payload would
    /// stay invisible until the next unrelated render arrived.
    ///
    /// Implementations without throttling (e.g. PlainRenderer) can
    /// treat this as a flush.
    fn flush_deferred(&mut self);

    /// Force the currently retained projection to be emitted again even when
    /// its logical state has not changed. Terminal hosts may discard or defer
    /// visible updates while their window is unfocused; a normal diff would
    /// then produce no bytes after focus returns because the renderer's cache
    /// still matches the authoritative UI state.
    ///
    /// Default to the ordinary deferred flush for renderers without a retained
    /// screen cache. The retained backend invalidates its physical projection
    /// and performs an immediate full-frame repaint.
    fn force_repaint(&mut self) {
        self.flush_deferred();
    }

    /// Returns (and clears) whether a body overflow scrolled the whole
    /// viewport — footer included — up one row since the last call. The
    /// render worker calls this after each command and, when true, repaints
    /// the footer immediately via `flush_deferred` instead of waiting for the
    /// event loop's next ~5ms deferred tick, so the footer doesn't visibly lag
    /// the scroll on hosts that don't vsync-coalesce (native Win10 conhost /
    /// pwsh7). Default `false`: only the retained renderer scrolls a viewport;
    /// plain/pipe and the cross-thread proxy renderer never do.
    fn take_pending_scroll_flush(&mut self) -> bool {
        false
    }

    /// Terminal window was resized to `(cols, rows)`. The retained
    /// backend uses this to re-flow body width and reposition the
    /// pinned footer; non-geometry-sensitive backends (Plain, tests)
    /// keep the no-op default.
    fn on_resize(&mut self, _cols: u16, _rows: u16) {}

    /// Scroll the body viewport up (negative `delta`) or down
    /// (positive `delta`) by `delta` rows.
    ///
    /// Default no-op for renderers that delegate scrollback to the
    /// host terminal (RetainedRenderer's append-only path; PlainRenderer
    /// streaming to stdout).
    fn scroll_body(&mut self, _delta: i32) {}

    /// Jump the body viewport to the absolute top / bottom of
    /// scrollback. Used for Home / End key handling.
    fn scroll_body_to_top(&mut self) {}
    fn scroll_body_to_bottom(&mut self) {}

    /// Update the cached welcome banner's model / working_dir fields in
    /// place and trigger a repaint of the banner rows. Used after the
    /// QR-onboarding `/codingplan` claim finishes: the banner was
    /// painted at the top of scrollback with `model=""` (the claim
    /// hadn't picked a default provider yet) — once the claim writes
    /// `ctx.model_name`, this hook splices the resolved model into the
    /// existing banner rows so the user doesn't see a permanently
    /// blank model bullet.
    ///
    /// Default no-op: renderers without a retained body buffer can't
    /// edit already-emitted rows in place.
    fn refresh_welcome_banner(&mut self, _model: &str, _working_dir: &str) {}

    /// Jump body viewport to the prev/next message boundary. No-op when no
    /// such boundary exists in the configured direction.
    fn scroll_to_prev_message(&mut self) {}
    fn scroll_to_next_message(&mut self) {}
    fn scroll_to_prev_user_message(&mut self) {}
    fn scroll_to_next_user_message(&mut self) {}
}

/// Visual style for the menu popup. Drives whether the renderer prefixes
/// each row with `/` (slash-command palette) or `+ ` (file/dir mention),
/// and which marker indicates the selected row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MenuKind {
    /// Default: rows shown as `/<name>`, selected row marked `▸`.
    #[default]
    SlashCommand,
    /// `@`-mention popup: rows shown as `+ <path>`, no slash prefix.
    /// Selected row uses reverse-video only (no extra arrow).
    AtMention,
    /// `$`-trigger skills picker. Rows show the bare skill name + description,
    /// no `/`, `/skills`, or `$` prefix; selection marked with `▸`.
    Skill,
    /// Modal action picker. Rows show a bare action label + description,
    /// with no slash-command prefix; selection is marked with `▸`.
    Action,
    TwoColumn {
        row_prefix: &'static str,
        selected_marker: &'static str,
    },
    /// `/cd` project list. Uses `/resume`-style title/search/hint chrome with
    /// single-line directory rows, capped to roughly half the terminal height.
    DirectoryList,
    /// Plugin manager list: 2-line rendering per item.
    /// Row 1: Plugin Name + Marketplace + Installation Status
    /// Row 2: Description
    Plugin,
    /// Marketplace list tab screen: 3-line rendering + 1 blank line separator per item
    Marketplace,
    /// Plugin manager details / scope selection screens: 1-line rendering per item, input box hidden
    PluginInfo,
    /// `/resume` session picker: same chrome as `Plugin` (bordered search box,
    /// hidden composer, title + bottom hint), but each session row is 2 lines —
    /// row 1 = bright session title, row 2 = gray metadata. Mirrors `Plugin`
    /// throughout the render loop EXCEPT the per-item leaf builder.
    SessionList,
}

impl MenuKind {
    /// Max visible rows for this menu kind. Both `paint_footer` and
    /// `current_footer_rows` use this so the estimate matches actual
    /// rendering.
    pub fn max_visible_rows(&self, screen_height: usize, item_count: usize) -> usize {
        match self {
            MenuKind::SlashCommand | MenuKind::AtMention => item_count.min(4),
            MenuKind::Skill | MenuKind::Action | MenuKind::TwoColumn { .. } => {
                item_count.min((screen_height / 2).max(4))
            }
            // Leave one row for the surrounding footer rule/status chrome so
            // the complete `/cd` surface, not just its menu payload, stays at
            // roughly half of the terminal.
            MenuKind::DirectoryList => item_count.min((screen_height / 2).saturating_sub(1).max(1)),
            MenuKind::Plugin | MenuKind::SessionList => {
                let plugin_count = item_count.saturating_sub(3);
                let max_plugins = (screen_height / 4).max(2);
                let visible_plugins = plugin_count.min(max_plugins);
                3 + visible_plugins * 2
            }
            MenuKind::Marketplace => {
                let mp_count = (item_count.saturating_sub(4)) / 2;
                let max_mps = (screen_height / 6).max(1);
                let visible_mps = mp_count.min(max_mps);
                5 + visible_mps * 4
            }
            MenuKind::PluginInfo => item_count,
        }
    }
}

/// Slash-command palette payload: filtered entries + which one is selected.
#[derive(Debug, Clone, Default)]
pub struct MenuPayload {
    pub items: Vec<(String, String)>, // (name, desc)
    pub selected: usize,
    /// Visual style. Defaults to `SlashCommand`; existing call sites
    /// using `MenuPayload { items, selected }` get the slash style for
    /// free. `@`-mention path explicitly sets `MenuKind::AtMention`.
    pub kind: MenuKind,
}

/// Persistent status line drawn directly below the input box — CC-style
/// Severity classification for the right-aligned status hint.
/// Warning → Role::Error (red, e.g. "no provider", "model retired").
/// Info → Role::Muted (dim, e.g. "new version available", drift notice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HintSeverity {
    #[default]
    Warning,
    Info,
    /// `!` shell-mode affordance — renders in atomcode brand purple
    /// (`Role::Shell`), matching the shell-mode box / badge.
    Shell,
}

/// "model · cwd · ctx_used / ctx_window" chrome. Visible in both Idle
/// and Streaming phases so the user always sees what provider is active
/// and how much of the context window is currently in use. Cumulative
/// session token totals are NOT shown here — they're per-session and
/// don't tell the user whether the next turn is at risk of overflow.
/// `ctx_used` answers "what does the model see right now"; `ctx_window`
/// is the cap. Together they answer "how close are we to compaction".
/// Colour slot for a left-aligned mode badge. Each variant maps to a
/// concrete `CellStyle` in the renderer, so the badge's colour is decided
/// at construction time (in `build_status`) rather than hard-coded in the
/// rendering `if/else if` chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BadgeColour {
    /// AcceptEdits — periwinkle `Role::Mode`.
    #[default]
    Mode,
    /// Plan — orange `Role::Plan`.
    Plan,
    /// Build — faint secondary (blends into the status row).
    Secondary,
}

/// Left-aligned mode badge: a label string plus the colour slot it
/// renders in. Replaces the previous trio of `Option<String>` fields
/// (`mode_indicator` / `plan_indicator` / `build_indicator`) so adding
/// a new mode only needs a new `BadgeColour` variant + one `match` arm
/// in `build_status`, not a fresh `StatusLine` field and a parallel
/// `if/else if` branch in the renderer.
#[derive(Debug, Clone)]
pub struct ModeBadge {
    pub label: String,
    pub colour: BadgeColour,
}

#[derive(Debug, Clone, Default)]
pub struct StatusLine {
    pub model: String,
    pub cwd: String, // HOME replaced with "~"
    /// Messages submitted during the active turn but not yet accepted at a
    /// model/tool boundary. Rendered as a transient panel above the composer.
    pub pending_messages: Vec<String>,
    /// Current recalled input-history position, shown on the input box's
    /// top rule only while Up/Down navigation is active.
    pub history: Option<HistoryPosition>,
    /// Active Ctrl+R reverse-i-search state, shown on the input box's
    /// top rule while searching. `None` outside search mode.
    pub search: Option<SearchState>,
    /// Ephemeral text rendered faintly inside an otherwise-empty composer.
    /// Right Arrow acceptance belongs to the input driver, not the renderer.
    pub next_prompt_suggestion: Option<String>,
    /// Optional read-only command report rendered as a transient multi-line
    /// footer panel directly below the input box. It never enters scrollback.
    pub command_output: Option<String>,
    /// Tokens currently in the model's context (last turn's `sent_tokens`).
    /// Pre-first-turn this is 0; when `ctx_window` is known the renderer shows
    /// zero usage against that window.
    pub ctx_used: usize,
    /// Provider's context window (cap). 0 when not yet known — renderer
    /// falls back to a bare "12.3k tok" display in that case.
    pub ctx_window: usize,
    /// Right-aligned passive hint with severity. `Warning` renders red
    /// (no-provider nudge, CodingPlan model-missing); `Info` renders
    /// muted (upgrade banner, CodingPlan drift notice). None → no hint.
    pub hint: Option<(String, HintSeverity)>,
    /// Left-aligned mode badge (`ModeBadge`), prepended before `model`.
    /// `None` for the default Build startup so the status row stays clean.
    /// The badge carries both its label and its colour slot, so the
    /// renderer no longer needs a separate field per mode.
    pub mode_indicator: Option<ModeBadge>,
    /// Right-aligned bypass indicator, appended after `hint` on the
    /// right side of the status row. Shown whenever the execution mode is
    /// `Auto` (auto-approve all tools) — whether entered via
    /// `--dangerously-skip-permissions / -y` at startup or the `/auto` /
    /// Tab cycle at runtime — rendering a yellow warning badge so the user
    /// is always aware that all tool calls are auto-approved. Kept separate
    /// from `mode_indicator` (left-aligned PLAN badge) so it does not
    /// displace the mode indicator.
    pub bypass_indicator: Option<String>,
    /// Current session display name, shown as a right-aligned cyan
    /// pill overlaid on the input box's top rule. `Some` only after
    /// the user has explicitly run `/rename` (Session::user_renamed) —
    /// auto-named / default sessions leave this `None` to keep the
    /// chrome quiet on fresh conversations.
    pub session_name: Option<String>,
    /// Current reasoning_effort for the active provider's model.
    /// None = not set (API uses its own default). Cycled via Ctrl+T.
    pub reasoning_effort: Option<String>,
    /// Active todo list progress, rendered on a DEDICATED footer row above the
    /// status line (like the goal/loop row) so multi-step progress — including
    /// which task is running — is visible without the inline todowrite block
    /// (which scrolls away). `None` ⇒ no todo list, row omitted (no noise for
    /// conversations that never used todowrite). Carries raw fields; the
    /// renderer owns glyph/width/terminal-safety (mirrors GoalStatus).
    pub todo: Option<TodoProgress>,
    /// Active `task`/Team fan-out rendered as a fixed panel above the input when
    /// no Todo/Tasks plan already owns that slot. With an active Todo plan the
    /// same projection is routed to a live conversation-body Agent block.
    pub subtasks: Option<SubtaskProgress>,
    /// When the approval panel is active (user must confirm/deny a tool call),
    /// this carries its current state for the dedicated footer approval panel
    /// (rendered above the todo panel). `None` ⇒ no approval pending, panel
    /// omitted. Mirrors `todo` — the renderer owns glyph/width/terminal-safety.
    pub approval: Option<ApprovalPanelView>,
    /// When a `request_user_input` question is active, this carries its state for
    /// the dedicated footer panel (rendered in the same slot as `approval` — the
    /// two are mutually exclusive). `None` ⇒ no question pending, panel omitted.
    /// Mirrors `approval` — the renderer owns glyph/width/terminal-safety.
    pub user_input: Option<UserInputPanelView>,
    /// When the round-cap checkpoint is active (the agent reached its configured
    /// max-rounds limit and is asking the user whether to continue), this carries
    /// the panel view for the dedicated footer picker. Rendered in the same slot
    /// as `approval` / `user_input` (all three are mutually exclusive; the priority
    /// order is: approval > user_input > round_cap_panel). `None` ⇒ no checkpoint.
    pub round_cap_panel: Option<UserInputPanelView>,
    /// When an autonomous `/goal` loop is active, this carries its live status
    /// for the DEDICATED footer goal row (its own full-width line above the
    /// status row). `None` ⇒ no goal running, row omitted. Previously this was
    /// a pre-formatted suffix crammed onto the shared status line, where it was
    /// the first thing truncated under a hint / narrow terminal and omitted the
    /// condition text — so users couldn't reliably see the goal while tool
    /// output scrolled. Its own row fixes that.
    pub goal: Option<GoalStatus>,
    /// When a `/loop` is active, this carries its live status for the dedicated
    /// footer loop row (its own full-width line, shown instead of the goal row
    /// — only one of goal/loop is active at a time). `None` ⇒ no loop running.
    pub loop_status: Option<LoopStatus>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryPosition {
    pub current: usize,
    pub total: usize,
}

/// Live Ctrl+R reverse-i-search state, rendered on the input box's top
/// rule while the user is searching input history. `current` is the
/// 1-based position of the shown match among all `total` matches; when
/// `matched` is false the query has no matches and `current` is 0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchState {
    pub query: String,
    pub current: usize,
    pub total: usize,
    pub matched: bool,
}

/// Renderer-facing snapshot of the approval panel (mirrors how `TodoProgress`
/// feeds the todo panel). Header + option labels + selected index.
#[derive(Debug, Clone)]
pub struct ApprovalPanelView {
    pub tool: String,
    pub detail: String,
    pub options: Vec<String>,
    pub selected: usize,
    /// Optional advisory line rendered under the header (e.g. a credential warning).
    pub note: Option<String>,
}

/// Renderer-facing snapshot of the `request_user_input` panel (mirrors
/// `ApprovalPanelView`). Header + question + the mode-specific body: a
/// highlighted option list (single), `[x]`/`[ ]` checkboxes (multiple),
/// or a bordered input field (text).
#[derive(Debug, Clone)]
pub struct UserInputPanelView {
    pub header: String,
    pub question: String,
    pub mode: atomcode_capabilities::tools::request_user_input::UserInputMode,
    /// Concrete options: label + optional description (faint second line).
    /// Does NOT include the always-appended "Other" free-text row.
    pub options: Vec<(String, Option<String>)>,
    /// Highlighted row. For single/multiple this ranges over the concrete
    /// option rows (`0..options.len()`), then the always-appended "Other" row
    /// (`options.len()`). For multiple mode the cursor also reaches
    /// `options.len()+1` (the Submit row); single mode's last row is the
    /// custom-answer row at `options.len()`.
    pub cursor: usize,
    /// Per-row checked flags (multiple mode). Length `options.len() + 1` — one
    /// per concrete option plus the trailing "Other" row.
    pub checked: Vec<bool>,
    /// Standalone text-mode input buffer.
    pub text: String,
    pub text_cursor_byte: usize,
    /// "Other" free-text row buffer for single/multiple mode.
    pub custom_text: String,
    pub custom_text_cursor_byte: usize,
    /// Whether to render the "Other" free-text row (mirrors `UserInputPanel.custom`).
    pub custom: bool,
    /// Signed PageUp/PageDown displacement from the automatically selected
    /// viewport. Kept in TUI state; it is not part of the tool wire protocol.
    pub scroll_offset: isize,
    /// Batch navigator context. `None` = a standalone single question (rendered
    /// byte-identically to before, no chrome). `Some` = this is one question inside
    /// a multi-question batch, so the renderer adds a `Question i/N` navigator and a
    /// Tab hint (or a Submit screen when `on_submit`).
    pub batch: Option<UserInputBatchMeta>,
}

/// Navigator context for a question rendered as part of a multi-question batch.
#[derive(Debug, Clone)]
pub struct UserInputBatchMeta {
    /// Total questions in the batch.
    pub total: usize,
    /// 1-based index of the current question (for the `Question i/N` navigator).
    pub index: usize,
    /// Per-question answered flags (for the ✓/○ markers), length `total`.
    pub answered: Vec<bool>,
    /// Full question/answer projection for the final review page.
    pub summaries: Vec<UserInputAnswerSummary>,
    /// The cursor is on the Submit stop (render the submit screen, not a question).
    pub on_submit: bool,
}

#[derive(Debug, Clone)]
pub struct UserInputAnswerSummary {
    pub header: String,
    pub question: String,
    /// `None` is shown explicitly as unanswered and is serialized as declined.
    pub answer: Option<String>,
}

/// Build a [`UserInputPanelView`] for the round-cap checkpoint panel (style B:
/// Single picker, two options with descriptions, no free-text "Other" row).
///
/// `cap` is the configured round limit (displayed in both the question and the
/// continue option description). `cursor` is the currently highlighted row
/// (0 = "继续", 1 = "停止"). `stats` is a pre-formatted elapsed/token string
/// (e.g. "2h0m0s · 305.00K tokens") — appended to the question when non-empty.
pub fn round_cap_view(cap: u32, base: u32, cursor: usize, stats: &str) -> UserInputPanelView {
    use atomcode_capabilities::tools::request_user_input::UserInputMode;
    let question = if stats.is_empty() {
        format!("已运行 {cap} 轮，继续吗？")
    } else {
        format!("已运行 {cap} 轮（{stats}），继续吗？")
    };
    UserInputPanelView {
        header: "轮次上限".to_string(),
        question,
        mode: UserInputMode::Single,
        options: vec![
            // `base` (the re-arm step), not `cap`: after a continuation `cap` has
            // grown but only `base` more rounds are granted before the next prompt.
            (
                "继续".to_string(),
                Some(format!("再跑 {base} 轮后重新确认")),
            ),
            ("停止".to_string(), Some("结束本回合".to_string())),
        ],
        cursor,
        checked: vec![],
        text: String::new(),
        text_cursor_byte: 0,
        custom_text: String::new(),
        custom_text_cursor_byte: 0,
        custom: false,
        scroll_offset: 0,
        batch: None,
    }
}

/// Progress of the active todo list, rendered as the multi-line footer todo
/// panel. The renderer collapses the list to fit (`todo_panel_rows`) and
/// width-truncates item content; the `completed`/`in_progress`/`total` counts
/// show in the panel header.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TodoProgress {
    /// The description of the task currently `in_progress` (todowrite enforces
    /// at most one). `None` when no task is in progress (all pending / all done).
    pub current: Option<String>,
    /// Number of tasks marked `completed`.
    pub completed: usize,
    /// Number of tasks currently `in_progress` (todowrite enforces at most one,
    /// so this is 0 or 1). Pre-computed by the caller so the renderer doesn't
    /// have to scan `items` — keeps the three header counts (`completed`,
    /// `in_progress`, `total`) single-sourced and in sync.
    pub in_progress: usize,
    /// Total number of tasks in the list.
    pub total: usize,
    /// The full ordered list (status + content) — drives the multi-line footer
    /// todo panel. `current`/`completed`/`in_progress`/`total` are retained as
    /// pre-computed conveniences for the header + hide-when-all-done filter.
    pub items: Vec<(atomcode_capabilities::tools::todo::TodoStatus, String)>,
}

/// Fixed footer projection for one in-flight `task` fan-out. This is a TUI
/// presentation type: the kernel continues to expose generic tool-progress
/// strings, while the event loop folds the known Task contract into these
/// stable child rows.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SubtaskProgress {
    /// Parent tool call whose terminal event owns removal of this panel.
    pub call_id: String,
    pub completed: usize,
    pub total: usize,
    pub items: Vec<SubtaskItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtaskItem {
    pub label: String,
    pub description: String,
    pub model: String,
    pub activity: String,
    pub started_at: Option<std::time::Instant>,
    pub output_tokens: u64,
    pub status: SubtaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SubtaskStatus {
    #[default]
    Pending,
    Running,
    Completed,
    Stopped,
    Failed,
}

/// Live status of an active autonomous `/goal` loop, rendered on the dedicated
/// footer goal row. The renderer width-truncates `condition` to fit; `round`
/// and the elapsed time always survive (see `format_goal_row`).
#[derive(Debug, Clone)]
pub struct GoalStatus {
    /// The goal condition text (truncated with `…` to fit the row width).
    pub condition: String,
    /// Round number AS DISPLAYED — 1-based (the first attempt reads `round 1`).
    /// The caller adds 1 to the engine's 0-based internal round.
    pub round: u32,
    /// Wall-clock seconds since the goal was set.
    pub elapsed_secs: u64,
    /// Current phase of the goal (Pursuing / PausedAtCap / Satisfied / Ended).
    /// Drives badge rendering: Pursuing → live progress; PausedAtCap → paused
    /// badge with resume hint; Satisfied → achieved badge. Ended is never stored
    /// (the goal row is hidden by clearing `goal_condition` before this is
    /// constructed).
    pub phase: atomcode_coding::GoalPhase,
}

/// Live status of an active `/loop`, rendered on the dedicated footer loop row.
/// Mirrors `GoalStatus` exactly: the renderer width-truncates `label` to fit,
/// while `round` and elapsed always survive.
#[derive(Debug, Clone)]
pub struct LoopStatus {
    /// The loop label text (truncated with `…` to fit the row width).
    pub label: String,
    /// Round number AS DISPLAYED — 1-based (the first attempt reads `round 1`).
    /// The caller adds 1 to the engine's 0-based internal round.
    pub round: u32,
    /// Wall-clock seconds since the loop was started.
    pub elapsed_secs: u64,
}

/// The role of a diff line: an addition (`+`), a deletion (`-`), or unchanged
/// context (` `). Drives the sign + color in the renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    Add,
    Del,
    Context,
    /// A gap between two hunks — rendered as a dim `⋮` so far-apart edits in one
    /// file read as one block without showing the unchanged run between them.
    Separator,
}

/// One line of a rendered diff, with the file line number for its side.
/// `old_lineno` is set for Del + Context, `new_lineno` for Add + Context.
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub kind: DiffKind,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    pub text: String,
}

/// One child entry inside a `UiLine::ToolGroupRender` payload. `call_id`
/// is the model-supplied tool-call id; `text` is the display string the
/// renderer initially prints (e.g. `↳ Read File foo.rs`). Subsequent
/// `ToolGroupChildUpdate` events with the same call_id rewrite this row
/// in place (e.g. to `↳ ✓ Read File foo.rs`).
#[derive(Debug, Clone)]
pub struct ToolGroupChild {
    pub call_id: String,
    pub text: String,
}

/// True when the live input buffer puts the user in `!` shell mode: a `!` leads
/// the (left-trimmed) buffer, INCLUDING a bare `!`. Drives the shell-mode visual
/// treatment (purple input box / chevron / status badge / `! for shell mode`
/// hint). Pure fn of the buffer, so the treatment is transient — it arms the
/// instant `!` is typed and reverts the instant it's gone (submit / clear /
/// delete), no persistent mode state (unlike `/plan` `/auto`). Distinct from
/// `bash_input_hint`, which needs a runnable command (non-empty after `!`).
pub fn input_shell_mode(buf: &str) -> bool {
    buf.trim_start().starts_with('!')
}

/// Wrap a compaction marker label in a dash rule: `─── {label} ───` (unicode)
/// or `--- {label} ---` (ASCII fallback for fonts lacking box-drawing — the
/// same `unicode_symbols` gate the spinner `◐`→`|/-\` and ellipsis `…`→`...`
/// use). Pure, so the wrapping is unit-tested independent of a renderer.
pub fn compaction_rule(label: &str, unicode: bool) -> String {
    let dash = if unicode { "───" } else { "---" };
    format!("{dash} {label} {dash}")
}

/// Convert a Duration to a short label, scaling the unit with magnitude:
/// `340ms` (< 1s) → `23.1s` (< 1min) → `2m9s` (< 1h) → `1h5m9s` (≥ 1h).
/// Sub-minute keeps one decimal; at minute scale and above the sub-second
/// part is dropped (it's noise next to whole minutes/hours).
pub fn fmt_dur(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{}ms", ms);
    }
    let total = d.as_secs();
    if total < 60 {
        return format!("{:.1}s", d.as_secs_f64());
    }
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h == 0 {
        format!("{m}m{s}s")
    } else {
        format!("{h}h{m}m{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_mode_is_a_leading_bang_including_bare() {
        // Drives the shell-mode visual treatment (purple box / chevron / badge /
        // `! for shell mode` hint). Active the instant `!` leads the buffer —
        // INCLUDING a bare `!` (the affordance shows before a command is typed),
        // unlike `bash_input_hint` which needs a runnable command.
        assert!(input_shell_mode("!"), "bare ! already arms shell mode");
        assert!(input_shell_mode("!ls -la"));
        assert!(
            input_shell_mode("  !git status"),
            "leading whitespace tolerated"
        );
        // Reverts the instant the `!` is gone — pure fn of the live buffer, so a
        // submit/clear/delete flips it back with no persistent state.
        assert!(!input_shell_mode(""), "empty buffer is not shell mode");
        assert!(!input_shell_mode("   "), "blank buffer is not shell mode");
        assert!(!input_shell_mode("ls"), "no leading bang");
        assert!(!input_shell_mode("echo !x"), "bang not at the start");
    }

    #[test]
    fn compaction_rule_wraps_label_unicode() {
        assert_eq!(
            compaction_rule("已压缩 · 摘要 2 条", true),
            "─── 已压缩 · 摘要 2 条 ───"
        );
    }

    #[test]
    fn compaction_rule_wraps_label_ascii_fallback() {
        assert_eq!(compaction_rule("done", false), "--- done ---");
    }

    #[test]
    fn fmt_dur_scales_unit_with_magnitude() {
        assert_eq!(fmt_dur(Duration::from_millis(340)), "340ms");
        assert_eq!(fmt_dur(Duration::from_millis(999)), "999ms");
        // sub-minute keeps one decimal (the screenshot's `23.1s`)
        assert_eq!(fmt_dur(Duration::from_secs_f64(23.1)), "23.1s");
        assert_eq!(fmt_dur(Duration::from_secs_f64(59.4)), "59.4s");
        // minute scale drops the decimal (`129.8s` → `2m9s`)
        assert_eq!(fmt_dur(Duration::from_secs_f64(129.8)), "2m9s");
        assert_eq!(fmt_dur(Duration::from_secs(60)), "1m0s");
        assert_eq!(fmt_dur(Duration::from_secs(3599)), "59m59s");
        // hour scale shows all three components
        assert_eq!(fmt_dur(Duration::from_secs(3600)), "1h0m0s");
        assert_eq!(fmt_dur(Duration::from_secs(3661)), "1h1m1s");
    }

    fn two_column() -> MenuKind {
        MenuKind::TwoColumn {
            row_prefix: "",
            selected_marker: "▸",
        }
    }

    // `current_footer_rows` reserves `max_visible_rows` rows while
    // `paint_footer` only paints `min(item_count, cap)` rows. For the two
    // to agree, `max_visible_rows` must never exceed `item_count`.
    #[test]
    fn two_column_never_reserves_more_than_item_count() {
        let k = two_column();
        // Short lists: must equal item_count, NOT the 4-row floor.
        assert_eq!(k.max_visible_rows(40, 0), 0);
        assert_eq!(k.max_visible_rows(40, 1), 1);
        assert_eq!(k.max_visible_rows(40, 2), 2);
        assert_eq!(k.max_visible_rows(40, 3), 3);
    }

    #[test]
    fn two_column_caps_at_half_screen_for_long_lists() {
        let k = two_column();
        // 50 items, height 40 → window cap = max(20, 4) = 20.
        assert_eq!(k.max_visible_rows(40, 50), 20);
        // At the cap boundary.
        assert_eq!(k.max_visible_rows(40, 20), 20);
        assert_eq!(k.max_visible_rows(40, 19), 19);
    }

    #[test]
    fn directory_list_uses_half_screen_row_budget() {
        assert_eq!(MenuKind::DirectoryList.max_visible_rows(10, 20), 4);
        assert_eq!(MenuKind::DirectoryList.max_visible_rows(40, 30), 19);
        assert_eq!(MenuKind::DirectoryList.max_visible_rows(40, 8), 8);
    }

    #[test]
    fn two_column_floor_keeps_at_least_four_on_tiny_screens() {
        let k = two_column();
        // Tiny screen (h/2 = 3) with plenty of items → floor lifts cap to 4.
        assert_eq!(k.max_visible_rows(6, 50), 4);
        // ...but still never more than the item count.
        assert_eq!(k.max_visible_rows(6, 2), 2);
    }

    #[test]
    fn fixed_kinds_cap_at_four() {
        for k in [MenuKind::SlashCommand, MenuKind::AtMention] {
            assert_eq!(k.max_visible_rows(40, 2), 2);
            assert_eq!(k.max_visible_rows(40, 10), 4);
        }
    }
}
