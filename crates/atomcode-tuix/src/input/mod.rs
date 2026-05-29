// crates/atomcode-tuix/src/input/mod.rs
pub mod history;
pub mod key_action;
pub mod reader;

use crossterm::event::KeyEvent;

/// Events the input thread sends to the main async loop.
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// A key was pressed (raw mode).
    Key(KeyEvent),
    /// A bracketed-paste payload arrived.
    Paste(String),
    /// Stdin closed (reader thread exiting).
    Eof,
    /// Terminal window resized; carries the new `(cols, rows)`.
    /// The event loop forwards this to the renderer so the DECSTBM
    /// scroll region can re-flow to the new height (footer stays
    /// pinned at `[H - footer_rows + 1, H]`).
    Resize(u16, u16),
    /// Mouse scroll wheel. `delta` lines: negative = up (older
    /// content), positive = down (newer). SGR mouse capture
    /// (`?1002h` / `?1006h`) is intentionally disabled, so the host
    /// terminal handles wheel events natively before they reach us
    /// in practice — this variant survives only as a defensive
    /// catch-all for terminals that forward wheel ticks outside the
    /// SGR mouse protocol, and the event-loop arm is a no-op.
    MouseScroll(i32),
}
