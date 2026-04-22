// crates/atomcode-tuix/src/modals/whip_overlay.rs
//
// Display-only modal that plays the 15-frame whip sweep, then closes.
// Swallows all keys (except Esc for early dismiss) so mid-animation
// typing doesn't corrupt anything. Frames advance via explicit
// `advance(now)` calls from the event loop's 33ms tick.

use std::time::Instant;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{Buffer, LoopCtx};
use crate::render::{Renderer, UiLine};
use crate::state::UiState;
use crate::whip::anim;

/// Time between animation frames. 15 × 33ms ≈ 495ms total.
pub const FRAME_MS: u64 = 33;

pub struct WhipOverlay {
    phrase: String,
    started_at: Instant,
    last_frame_drawn: Option<u16>,
    done: bool,
}

impl WhipOverlay {
    pub fn open(phrase: String) -> Self {
        Self {
            phrase,
            started_at: Instant::now(),
            last_frame_drawn: None,
            done: false,
        }
    }

    /// Frame index AT `now`. Clamped to `TOTAL_FRAMES`.
    pub fn current_frame(&self, now: Instant) -> u16 {
        let ms = now.duration_since(self.started_at).as_millis() as u64;
        let f = (ms / FRAME_MS) as u16;
        f.min(anim::TOTAL_FRAMES)
    }

    fn paint_frame(
        &self,
        frame_idx: u16,
        _buf: &Buffer,
        _state: &UiState,
        _ctx: &LoopCtx,
        renderer: &mut dyn Renderer,
    ) {
        // Hardcoded width until Task 10 threads real terminal cols through.
        let width: u16 = 60;
        let f = anim::frame(frame_idx, width, &self.phrase);
        renderer.render(UiLine::WhipFrame {
            rows: f.rows,
            phrase: f.phrase,
            flash: f.flash,
        });
        renderer.flush();
    }
}

impl Modal for WhipOverlay {
    fn handle_key(
        &mut self,
        code: KeyCode,
        _mods: KeyModifiers,
        _buf: &mut Buffer,
        _state: &mut UiState,
        _ctx: &mut LoopCtx,
        _renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // Esc dismisses early. Anything else is swallowed.
        if code == KeyCode::Esc {
            self.done = true;
            return Ok(ModalAction::Close);
        }
        Ok(ModalAction::Continue)
    }

    fn draw(
        &self,
        buf: &Buffer,
        state: &UiState,
        ctx: &LoopCtx,
        renderer: &mut dyn Renderer,
    ) {
        // Initial paint so the user sees frame 0 before the first tick
        // arrives (~33ms later).
        self.paint_frame(0, buf, state, ctx, renderer);
    }

    fn advance(
        &mut self,
        buf: &Buffer,
        state: &UiState,
        ctx: &LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> bool {
        if self.done {
            return true;
        }
        let now = Instant::now();
        let frame = self.current_frame(now);
        if self.last_frame_drawn != Some(frame) {
            self.last_frame_drawn = Some(frame);
            self.paint_frame(frame, buf, state, ctx, renderer);
        }
        if frame >= anim::TOTAL_FRAMES {
            self.done = true;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn current_frame_at_t0_is_0() {
        let o = WhipOverlay::open("x".into());
        assert_eq!(o.current_frame(o.started_at), 0);
    }

    #[test]
    fn current_frame_advances_with_elapsed() {
        let o = WhipOverlay::open("x".into());
        let at5 = o.started_at + Duration::from_millis(5 * FRAME_MS + 5);
        assert_eq!(o.current_frame(at5), 5);
    }

    #[test]
    fn current_frame_clamps_after_total() {
        let o = WhipOverlay::open("x".into());
        let way_later = o.started_at + Duration::from_secs(10);
        assert_eq!(o.current_frame(way_later), anim::TOTAL_FRAMES);
    }
}
