//! Shared text-selection module used by both AltScreenRenderer and
//! RetainedRenderer. Owns: anchor/head pos, drag tracking, range
//! computation, line rendering with reverse-video highlight, OSC 52
//! emission and arboard fallback for Ctrl+C copy.
//!
//! Each renderer holds a `SelectionState` and implements `BodyLineView`
//! over its native body buffer type (`Vec<String>` for alt-screen,
//! `Vec<Vec<Cell>>` for retained).

use std::borrow::Cow;

/// A single (row, col) cursor position in body_lines coordinates.
/// `row` is the index into body_lines; `col` is display-column.
pub type BodyPos = (usize, u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: BodyPos,
    pub head: BodyPos,
}

#[derive(Debug, Default)]
pub struct SelectionState {
    pub selection: Option<Selection>,
    pub active: bool,  // true while mouse button held down
}

/// Trait adapter so the selection module can read body content without
/// caring whether the renderer stores `Vec<String>` or `Vec<Vec<Cell>>`.
pub trait BodyLineView {
    fn line_count(&self) -> usize;
    fn line_text(&self, idx: usize) -> Cow<'_, str>;
}

// Impl for the alt-screen body_lines type.
impl BodyLineView for Vec<String> {
    fn line_count(&self) -> usize { self.len() }
    fn line_text(&self, idx: usize) -> Cow<'_, str> {
        Cow::Borrowed(self.get(idx).map(|s| s.as_str()).unwrap_or(""))
    }
}
