// crates/atomcode-tuix/src/render/ansi.rs
use super::{Renderer, UiLine};

pub struct AnsiRenderer;

impl Renderer for AnsiRenderer {
    fn render(&mut self, _line: UiLine) { todo!() }
    fn flush(&mut self) { todo!() }
    fn shutdown(&mut self) { todo!() }
}
