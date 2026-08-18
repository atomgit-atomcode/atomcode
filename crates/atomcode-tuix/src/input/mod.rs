// crates/atomcode-tuix/src/input/mod.rs
pub mod history;
pub mod key_action;
pub mod reader;

use crossterm::event::KeyEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerKind {
    Down,
    Up,
    Drag,
    Move,
    /// Vertical scroll amount. Negative moves toward older content.
    Scroll(i16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Primary,
    Secondary,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerEvent {
    pub kind: PointerKind,
    pub button: Option<PointerButton>,
    pub row: u16,
    pub col: u16,
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
}

/// Events the input thread sends to the main async loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputEvent {
    /// A key was pressed (raw mode).
    Key(KeyEvent),
    /// A bracketed-paste payload arrived.
    Paste(String),
    /// Stdin closed (reader thread exiting).
    Eof,
    /// Terminal focus changed. The reader still updates the process-wide
    /// notification focus state, while the main loop uses `true` to force a
    /// repaint of the retained terminal projection after the host regains
    /// focus. No runtime or conversation state changes on this event.
    FocusChanged(bool),
    /// Terminal window resized; carries the new `(cols, rows)`.
    /// The event loop forwards this to the renderer so the DECSTBM
    /// scroll region can re-flow to the new height (footer stays
    /// pinned at `[H - footer_rows + 1, H]`).
    Resize(u16, u16),
    /// Normalized crossterm mouse input with its original coordinates,
    /// button and supported modifiers retained.
    Pointer(PointerEvent),
}
