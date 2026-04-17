// crates/atomcode-tuix/src/render/mod.rs
pub mod theme;
pub mod ansi;
pub mod plain;

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
    /// Draw the input prompt "❯ " + current buffer (transient, idle).
    InputPrompt { buf: String, cursor_cols: usize },
    /// Streaming chrome: one spinner line above the 3-line input box.
    StreamingBox {
        buf: String,
        cursor_cols: usize,
        frame: &'static str,
        label: String,
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
