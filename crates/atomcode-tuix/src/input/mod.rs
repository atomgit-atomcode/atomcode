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
    /// content), positive = down (newer). Only emitted when the
    /// active renderer has enabled mouse capture (currently
    /// AltScreenRenderer only — RetainedRenderer relies on host-
    /// terminal scrollback, PlainRenderer doesn't pin a UI).
    MouseScroll(i32),
    /// Mouse primary button pressed at terminal cell `(col, row)`.
    MouseDown { col: u16, row: u16 },
    /// Mouse drag moved to terminal cell `(col, row)`.
    MouseDrag { col: u16, row: u16 },
    /// Mouse primary button released.
    MouseUp,
}
