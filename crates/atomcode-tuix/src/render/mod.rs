// crates/atomcode-tuix/src/render/mod.rs
pub mod alt_screen;
pub mod cell;
pub mod plain;
pub mod qr;
pub mod retained;
pub mod screen;
pub mod theme;
pub mod worker;

use std::time::Duration;

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
    ToolResult {
        success: bool,
        summary: String,
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
    ApprovalPrompt {
        tool: String,
        detail: String,
    },
    Error(String),
    /// Non-fatal advisory line (yellow). Visually distinct from `Error`
    /// so the user can tell "we saw something fishy and want you to
    /// know" apart from "the turn died." Currently used by the OpenAI
    /// provider's truncation detector.
    Warning(String),
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
    /// Live sub-agent status line. Each emission replaces the previous
    /// `GuideStatus` row in-place — no scrollback accumulation.
    GuideStatus(String),
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
    /// `model` uses the Muted (gray) role — visually distinct from
    /// failure (yellow `! ...`) and from arbitrary command output.
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

    /// Remove the most recent `ApprovalPrompt` body row, if the tail
    /// row is one. Called by the event loop after the user responds
    /// Y/A/N so the prompt stops sitting in the body above the footer.
    /// Default: no-op — implementations that stream body lines to
    /// stdout (plain/pipe mode) can't retract them.
    fn pop_approval_prompt(&mut self) {}

    /// Terminal window was resized to `(cols, rows)`. DECSTBM-based
    /// renderers must re-issue the scroll region (`\x1b[1;H-N r`) so
    /// the fixed footer stays pinned to the new bottom. Non-DECSTBM
    /// renderers can treat this as a redraw hint or a no-op.
    ///
    /// Default is no-op — backends that don't care about geometry
    /// (Plain, tests) don't need to override.
    fn on_resize(&mut self, _cols: u16, _rows: u16) {}

    /// Scroll the body viewport up (negative `delta`) or down
    /// (positive `delta`) by `delta` rows. Used by AltScreenRenderer
    /// to support PageUp / PageDown / arrow-up scrollback navigation
    /// inside the alt-screen (where the host terminal's native
    /// scrollback is unavailable).
    ///
    /// Default no-op for renderers that delegate scrollback to the
    /// host terminal (RetainedRenderer's DECSTBM path; PlainRenderer
    /// streaming to stdout).
    fn scroll_body(&mut self, _delta: i32) {}

    /// Jump the body viewport to the absolute top / bottom of
    /// scrollback. Used for Home / End key handling.
    fn scroll_body_to_top(&mut self) {}
    fn scroll_body_to_bottom(&mut self) {}

    /// Mouse text-selection hooks. Backends that own mouse capture can
    /// override these; streaming/native-scrollback backends keep host
    /// terminal selection behavior and no-op here.
    fn begin_selection(&mut self, _col: u16, _row: u16) {}
    fn update_selection(&mut self, _col: u16, _row: u16) {}
    fn end_selection(&mut self) {}

    /// Copy the current mouse-selection text to the system clipboard
    /// (using arboard, not OSC 52) and clear the selection highlight.
    /// Returns `true` if a non-empty selection was copied.
    ///
    /// This is the Ctrl+C fallback for terminals (Windows Terminal,
    /// conhost) that ignore OSC 52 — the user selects text with the
    /// mouse, then presses Ctrl+C to copy it. AltScreenRenderer
    /// implements this; other backends return `false` (they use the
    /// host terminal's native selection).
    fn copy_selection(&mut self) -> bool {
        false
    }

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
}

/// "model · cwd · ctx_used / ctx_window" chrome. Visible in both Idle
/// and Streaming phases so the user always sees what provider is active
/// and how much of the context window is currently in use. Cumulative
/// session token totals are NOT shown here — they're per-session and
/// don't tell the user whether the next turn is at risk of overflow.
/// `ctx_used` answers "what does the model see right now"; `ctx_window`
/// is the cap. Together they answer "how close are we to compaction".
#[derive(Debug, Clone, Default)]
pub struct StatusLine {
    pub model: String,
    pub cwd: String, // HOME replaced with "~"
    /// Tokens currently in the model's context (last turn's `sent_tokens`).
    /// Pre-first-turn this is 0; the renderer hides the field then.
    pub ctx_used: usize,
    /// Provider's context window (cap). 0 when not yet known — renderer
    /// falls back to a bare "12.3k tok" display in that case.
    pub ctx_window: usize,
    /// Right-aligned passive hint with severity. `Warning` renders red
    /// (no-provider nudge, CodingPlan model-missing); `Info` renders
    /// muted (upgrade banner, CodingPlan drift notice). None → no hint.
    pub hint: Option<(String, HintSeverity)>,
    /// Left-aligned mode indicator, prepended before `model`. Present
    /// only when the user explicitly switched to a non-default agent
    /// mode (Plan today; conceivably others later). `None` for the
    /// default Build mode so the status row doesn't gain noise for
    /// the common case. Renders in brand color (Role::Brand) to draw
    /// the eye — switching modes changes whether file edits and shell
    /// run, so the user wants this prominent.
    pub mode_indicator: Option<String>,
    /// Current session display name, shown as a right-aligned cyan
    /// pill overlaid on the input box's top rule. `Some` only after
    /// the user has explicitly run `/rename` (Session::user_renamed) —
    /// auto-named / default sessions leave this `None` to keep the
    /// chrome quiet on fresh conversations.
    pub session_name: Option<String>,
}

/// One line in a diff batch. `added = true` renders as `+`, false as `-`.
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub added: bool,
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

/// Convert a Duration to a short label like "1.2s" or "340ms".
pub fn fmt_dur(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}
