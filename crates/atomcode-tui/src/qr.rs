//! A QR code, as a bitmap.
//!
//! Here rather than in the host (决策 6, `docs/plans/2026-09-19-remaining-gaps.md`):
//! turning a string into a picture is presentation, and this front end already
//! owns pictures (`docs/adr/0027` — a picture is a cell grid, not a graphics
//! protocol). What the code *says* — a login URL, a pairing code — is the
//! host's business and never appears here.
//!
//! **Black on white, not the theme's colours.** Every other colour on this
//! screen is a role resolved against the terminal's palette; this one must not
//! be. A scanner is looking for dark modules on a light field, and a QR drawn
//! in "muted" on "panel background" is a QR that does not scan. Exact RGB goes
//! through [`Raster`], which is the one door an arbitrary colour enters a frame
//! by.
//!
//! **Two modules per cell.** A terminal cell is about twice as tall as it is
//! wide, so one module per cell would draw a QR twice as wide as it is tall and
//! scanners would refuse it. Packing an upper and a lower module into one cell
//! with `▀` — foreground above, background below — makes each module square.
//! That is also why a terminal that drops cell backgrounds cannot show one at
//! all: half the picture lives in the background colour. The caller is expected
//! to show the URL as text as well, for that case and for the scanner that is
//! not to hand.

use qrcode::{Color as QrColor, QrCode};

use crate::raster::{Cell, Raster};

/// The light margin a scanner needs around the code, in modules.
///
/// Four is what the specification asks for. It is not decoration: without it a
/// scanner cannot tell where the code ends, and a code drawn tight against a
/// panel border reads as noise.
const QUIET_ZONE: usize = 4;

const BLACK: crate::theme::Rgb = (0, 0, 0);
const WHITE: crate::theme::Rgb = (255, 255, 255);

/// `data` as a bitmap, or `None` when it will not fit.
///
/// `None` is not an error to report — it is the caller's cue to show the text
/// instead. A code too big for a raster is a code too big to scan off this
/// screen anyway.
///
/// How big that is, is [`Raster::from_cells`]'s answer and not a second copy of
/// it here. The first draft did check both, and the criterion for it stayed
/// green when this file's check was deleted — which is what a rule with two
/// implementations looks like from the outside.
pub fn code(data: &str) -> Option<Raster> {
    let code = QrCode::new(data.as_bytes()).ok()?;
    let side = code.width() + QUIET_ZONE * 2;
    let columns = u16::try_from(side).ok()?;
    let rows = u16::try_from(side.div_ceil(2)).ok()?;

    // `true` is a dark module. Outside the code is the quiet zone, which is
    // light — including the half-row a code with an odd number of modules
    // leaves at the bottom.
    let dark = |x: usize, y: usize| -> bool {
        let (Some(x), Some(y)) = (x.checked_sub(QUIET_ZONE), y.checked_sub(QUIET_ZONE)) else {
            return false;
        };
        if x >= code.width() || y >= code.width() {
            return false;
        }
        matches!(code[(x, y)], QrColor::Dark)
    };

    let mut cells = Vec::with_capacity(columns as usize * rows as usize);
    for row in 0..rows as usize {
        for column in 0..columns as usize {
            let paint = |dark: bool| Some(if dark { BLACK } else { WHITE });
            cells.push(Cell {
                ch: '▀',
                fg: paint(dark(column, row * 2)),
                bg: paint(dark(column, row * 2 + 1)),
            });
        }
    }
    Raster::from_cells(columns, rows, cells).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(raster: &Raster, column: u16, row: u16) -> Cell {
        *raster.cell(column, row).expect("inside the bitmap")
    }

    /// The margin a scanner needs is part of the picture.
    ///
    /// Drawn rather than assumed: whatever is behind this is a panel, not a
    /// white field, so a code that relied on its surroundings to be light would
    /// scan on a light terminal and fail on a dark one.
    #[test]
    fn the_quiet_zone_is_drawn_and_it_is_light() {
        let raster = code("https://example.com/login?code=abc").expect("it fits");
        for column in 0..raster.columns {
            // Two whole cells is four module rows: the quiet zone exactly.
            for row in 0..2 {
                let cell = at(&raster, column, row);
                assert_eq!(
                    (cell.fg, cell.bg),
                    (Some(WHITE), Some(WHITE)),
                    "{column},{row}"
                );
            }
        }
        for row in 0..raster.rows {
            for column in 0..QUIET_ZONE as u16 {
                let cell = at(&raster, column, row);
                assert_eq!(
                    (cell.fg, cell.bg),
                    (Some(WHITE), Some(WHITE)),
                    "{column},{row}"
                );
            }
        }
    }

    /// Dark on light, whatever the terminal's palette is.
    ///
    /// The finder pattern's own corner is the one module every scanner looks
    /// for first, so it is the honest place to check polarity. Falsified by
    /// painting the modules in roles: the assertion is on the exact colours.
    #[test]
    fn a_dark_module_is_black_and_a_light_one_is_white() {
        let raster = code("https://example.com/login?code=abc").expect("it fits");
        // The top-left finder pattern starts where the quiet zone ends.
        let finder = at(&raster, QUIET_ZONE as u16, (QUIET_ZONE / 2) as u16);
        assert_eq!(finder.fg, Some(BLACK), "the finder pattern's first module");
        assert!(
            (0..raster.columns).any(|c| at(&raster, c, raster.rows / 2).fg == Some(WHITE)),
            "and light modules stayed light"
        );
    }

    /// Square modules, which is what makes it scannable at all.
    #[test]
    fn two_modules_ride_in_one_cell_so_the_code_is_square() {
        let raster = code("https://example.com/login?code=abc").expect("it fits");
        assert_eq!(
            raster.rows,
            raster.columns.div_ceil(2),
            "half as many rows as columns — a cell is about twice as tall as it is wide"
        );
        for column in 0..raster.columns {
            for row in 0..raster.rows {
                assert_eq!(at(&raster, column, row).ch, '▀', "{column},{row}");
            }
        }
    }

    /// Two different strings are two different pictures.
    #[test]
    fn the_picture_is_of_what_it_was_given() {
        let one = code("https://example.com/a").expect("it fits");
        let two = code("https://example.com/b").expect("it fits");
        assert_ne!(one, two);
    }

    /// Too big to draw is answered, not drawn badly.
    ///
    /// Two different ceilings, and the first draft of this only tested the
    /// far one — `QrCode::new` refusing 4000 bytes outright, which says
    /// nothing about this file. The one that matters is in between: a payload
    /// that encodes fine and then needs more modules than a bitmap may have.
    #[test]
    fn a_payload_that_cannot_fit_is_refused_rather_than_cropped() {
        let past_the_encoder = "x".repeat(4000);
        assert!(
            code(&past_the_encoder).is_none(),
            "more than a QR code holds"
        );

        let encodes_but_will_not_fit = "x".repeat(2200);
        assert!(
            QrCode::new(encodes_but_will_not_fit.as_bytes()).is_ok(),
            "the encoder is happy — so what refuses it next is this file"
        );
        assert!(
            code(&encodes_but_will_not_fit).is_none(),
            "a raster has a size, and this is past it"
        );
    }
}
