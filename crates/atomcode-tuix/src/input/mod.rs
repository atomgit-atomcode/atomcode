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
    /// Terminal window resized. Event loop reacts by wiping the screen
    /// and redrawing the footer at the new width — without this, the
    /// next redraw uses stale `cursor_row_from_top` from the old width
    /// and leaves a ghost of the old box above the new one.
    Resize,
    /// Stdin closed (reader thread exiting).
    Eof,
}
