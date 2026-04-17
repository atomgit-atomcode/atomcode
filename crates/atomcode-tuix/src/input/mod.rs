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
}
