//! A pane that draws a cell-grid bitmap.
//!
//! Stateless and redrawn every frame — which is exactly what a bitmap wants. The
//! stream's blocks *freeze* (ADR 0004), so a bitmap living in one could not be
//! repainted after the turn that created it ended; a view module has no history
//! to freeze, so it can be repainted forever. See `docs/adr/0027` decision ①.
//!
//! The bitmap reaches it through `viewport.moment.rasters`, not through a
//! service: `View::render` takes `&State` and a `&Viewport`, so a module cannot
//! reach a `Context`, a service handle, a channel or a clock — the signature
//! makes that unrepresentable rather than merely discouraged (`module.rs`).

use atomcode_harness::session::SessionEvent;

use crate::frame::Line;
use crate::module::{Height, View};
use crate::moment::{Moment, Viewport};

/// This module's id, and the first half of a bitmap's address.
pub const ID: &str = "raster";

/// The second half of the address. One bitmap per pane today; the key is here so
/// a pane that draws several does not need a second id for each.
pub const KEY: &str = "main";

pub struct RasterPane;

impl View for RasterPane {
    /// Stateless on purpose: the bitmaps are not folded from facts, so there is
    /// nothing to fold into. They arrive through `Moment`.
    type State = ();

    fn id() -> &'static str {
        ID
    }

    fn absorb(_state: &mut (), _fact: &SessionEvent) {}

    fn render(_state: &(), vp: &Viewport<'_>) -> Vec<Line> {
        let Some(raster) = vp.moment.rasters.get(ID, KEY) else {
            return Vec::new();
        };
        // A terminal with no Unicode still gets a picture if the bitmap is drawn
        // in characters that have a stand-in (`█` → `#`, one column for one), and
        // gets nothing rather than a grid of tofu if it is not. Braille has no
        // stand-in — deliberately, see `caps.rs` on the spinner.
        if !vp.moment.caps.unicode && !raster.all_downgradable() {
            return Vec::new();
        }
        // The caps this frame was composed against, so the colours are ones this
        // terminal can actually draw (see `theme::exact_colour`).
        raster.lines_in(vp.rect, vp.moment.caps)
    }

    /// Ask for nothing in particular.
    ///
    /// The rectangle is the layout tree's to give (`Region::view` or
    /// `LayoutOp::Show`) — deliberately **not** derived from the bitmap, because
    /// `height` is handed the caller's `Moment` and only `compose` fills in
    /// `rasters`; a height that read the bitmap would depend on which caller
    /// asked. `Fill` means "whatever the layout gave me", and `lines_in` clips to
    /// it.
    fn height(_state: &(), _moment: &Moment, _width: u16) -> Height {
        Height::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::raster::Rasters;
    use crate::tui_conformance;

    tui_conformance!(view RasterPane as raster_pane_conformance);

    fn solid_payload(columns: u16, rows: u16) -> String {
        use base64::Engine as _;
        let mut bytes = Vec::new();
        for _ in 0..columns as usize * rows as usize {
            bytes.extend_from_slice(&0x2588u32.to_le_bytes());
            bytes.extend_from_slice(&0x0100_0000u32.to_le_bytes());
            bytes.extend_from_slice(&0x0100_0000u32.to_le_bytes());
        }
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// One braille cell: the glyph the downgrade table deliberately has none for.
    fn braille_payload() -> String {
        use base64::Engine as _;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&0x280Bu32.to_le_bytes());
        bytes.extend_from_slice(&0x0100_0000u32.to_le_bytes());
        bytes.extend_from_slice(&0x0100_0000u32.to_le_bytes());
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn mounted(columns: u16, rows: u16) -> Rasters {
        let rasters = Rasters::new();
        rasters
            .mount(
                ID,
                KEY,
                crate::raster::Raster::decode(columns, rows, &solid_payload(columns, rows))
                    .expect("a solid payload"),
            )
            .expect("mount");
        rasters
    }

    #[test]
    fn nothing_mounted_draws_nothing() {
        // Not one blank row: a pane that always occupied its rectangle would be
        // chrome on every screen that never mounted a bitmap.
        let moment = Moment::default();
        let vp = Viewport::new(Rect::sized(10, 4), &moment);
        assert!(RasterPane::render(&(), &vp).is_empty());
    }

    #[test]
    fn a_mounted_bitmap_reaches_the_screen_through_the_moment() {
        let rasters = mounted(3, 2);
        let moment = Moment {
            rasters: rasters.view(),
            ..Moment::default()
        };
        let vp = Viewport::new(Rect::sized(10, 4), &moment);
        let lines = RasterPane::render(&(), &vp);
        assert_eq!(lines.len(), 2, "as many rows as the bitmap has");
        assert_eq!(lines[0].plain(), "\u{2588}\u{2588}\u{2588}");
    }

    #[test]
    fn the_panes_rect_is_what_is_drawn_not_the_bitmaps_size() {
        // The extra room is not filled, and the cells past the rect are cut:
        // a pane draws inside its rectangle like every other module.
        let rasters = mounted(8, 6);
        let moment = Moment {
            rasters: rasters.view(),
            ..Moment::default()
        };
        let vp = Viewport::new(Rect::sized(4, 2), &moment);
        let lines = RasterPane::render(&(), &vp);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].width(), 4);
    }

    #[test]
    fn a_terminal_without_unicode_still_gets_a_bitmap_it_can_draw() {
        // The negative control for the braille case below: `█` has an ASCII
        // stand-in (`#`), so a blocks bitmap is still a picture on an old
        // console and must not be withheld.
        let rasters = mounted(2, 1);
        let moment = Moment {
            rasters: rasters.view(),
            caps: crate::caps::Caps::plain(),
            ..Moment::default()
        };
        let vp = Viewport::new(Rect::sized(2, 1), &moment);
        assert_eq!(
            RasterPane::render(&(), &vp).len(),
            1,
            "a bitmap of blocks is still a bitmap without Unicode"
        );
    }

    #[test]
    fn a_braille_bitmap_is_withheld_rather_than_drawn_as_tofu() {
        // Braille is deliberately absent from the downgrade table — `caps.rs`
        // says why for the spinner ("there is no one-cell ASCII stand-in that
        // reads as motion"). The same reasoning applies to a grid of it: a grid
        // of tofu is not a picture, so it is withheld.
        let rasters = Rasters::new();
        rasters
            .mount(
                ID,
                KEY,
                crate::raster::Raster::decode(1, 1, &braille_payload()).expect("valid"),
            )
            .expect("mount");
        let moment = Moment {
            rasters: rasters.view(),
            caps: crate::caps::Caps::plain(),
            ..Moment::default()
        };
        let vp = Viewport::new(Rect::sized(1, 1), &moment);
        assert!(RasterPane::render(&(), &vp).is_empty());

        // And on a terminal that can draw it, it does.
        let capable = Moment {
            caps: crate::caps::Caps {
                unicode: true,
                ..crate::caps::Caps::plain()
            },
            ..moment.clone()
        };
        let vp = Viewport::new(Rect::sized(1, 1), &capable);
        assert_eq!(RasterPane::render(&(), &vp).len(), 1);
    }

    #[test]
    fn a_snapshot_taken_before_a_write_keeps_drawing_the_old_bitmap() {
        // Why the picture cannot tear: the moment holds the frame's snapshot,
        // and a write swaps the table rather than editing what the snapshot sees.
        let rasters = mounted(1, 1);
        let moment = Moment {
            rasters: rasters.view(),
            ..Moment::default()
        };
        let vp = Viewport::new(Rect::sized(1, 1), &moment);
        let before = RasterPane::render(&(), &vp)[0].plain();
        rasters
            .write(ID, KEY, &solid_payload(1, 1))
            .expect("a write of the mounted size");
        assert_eq!(RasterPane::render(&(), &vp)[0].plain(), before);
    }
}
