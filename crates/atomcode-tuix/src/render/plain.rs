// crates/atomcode-tuix/src/render/plain.rs
use super::{Renderer, UiLine};

pub struct PlainRenderer;

impl Renderer for PlainRenderer {
    fn render(&mut self, _line: UiLine) { todo!() }
    fn flush(&mut self) { todo!() }
    fn shutdown(&mut self) { todo!() }
}
