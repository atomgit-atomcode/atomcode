// crates/atomcode-tuix/src/render/mod.rs
pub mod theme;
pub mod cell;
pub mod screen;
pub mod plain;
pub mod retained;
pub mod worker;

use std::time::Duration;

/// Semantic line to render. Renderer implementations translate this to bytes.
///
/// Permanent lines (User, Assistant, ToolCall, ToolResult, Diff, Approval,
/// Error, Blank) all enter scrollback. Spinner and InputPrompt are transient.
#[derive(Debug, Clone)]
pub enum UiLine {
    Welcome { model: String, working_dir: String },
    User(String),
    AssistantText(String),
    AssistantLineBreak,
    ToolCall { name: String, detail: String },
    ToolResult { success: bool, summary: String },
    DiffLine { added: bool, text: String },
    /// A batch of diff lines emitted in a single render call. Use this
    /// instead of N individual `DiffLine` renders when a tool result
    /// carries many changed lines — each `DiffLine` triggers a full
    /// erase_footer + redraw_footer cycle, so 50 diff lines translate
    /// into 50 footer redraws and tens of KB of ANSI, blocking the
    /// event loop long enough to freeze the spinner. `DiffBlock` does
    /// one erase + N writes + one redraw.
    DiffBlock(Vec<DiffEntry>),
    ApprovalPrompt { tool: String, detail: String },
    Error(String),
    TurnCancelled,
    TurnComplete,
    /// Legacy single-line spinner (kept for tests / PlainRenderer fallback).
    /// During Streaming the event loop emits `StreamingBox` instead so the
    /// spinner sits ABOVE the input box rather than inside it.
    Spinner { frame: &'static str, label: String },
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
    },
    /// User pressed Enter: commit the current InputPrompt to scrollback.
    InputCommit,
    /// Slash-command output (arbitrary text, already sanitised by caller).
    CommandOutput(String),
    /// A visible separator between turns: `────── {label} ──────`.
    TurnSeparator { label: String },
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
}

/// Slash-command palette payload: filtered entries + which one is selected.
#[derive(Debug, Clone)]
pub struct MenuPayload {
    pub items: Vec<(String, String)>, // (name, desc)
    pub selected: usize,
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

/// "model · cwd · tokens" chrome. Visible in both Idle and Streaming
/// phases so the user always sees what provider is active.
#[derive(Debug, Clone, Default)]
pub struct StatusLine {
    pub model: String,
    pub cwd: String,      // HOME replaced with "~"
    pub total_tokens: usize,
    /// Right-aligned passive hint with severity. `Warning` renders red
    /// (no-provider nudge, CodingPlan model-missing); `Info` renders
    /// muted (upgrade banner, CodingPlan drift notice). None → no hint.
    pub hint: Option<(String, HintSeverity)>,
}

/// One line in a diff batch. `added = true` renders as `+`, false as `-`.
#[derive(Debug, Clone)]
pub struct DiffEntry {
    pub added: bool,
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
