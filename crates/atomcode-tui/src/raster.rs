//! Cell-grid bitmaps: a grid of `(code point, foreground, background)` that can
//! be repainted in place.
//!
//! **Not a graphics protocol.** Everything here is cells, so scrolling, the
//! frame diff, glyph downgrading and containment keep working the way they do
//! for every other thing on screen. See `docs/adr/0027`.
//!
//! Two types, split by who owns what:
//!
//! * [`Raster`] — one bitmap. **Immutable**: a write replaces it rather than
//!   amending it, so a frame that took a handle keeps the picture it drew.
//! * [`Rasters`] — the table a writer holds. The host keeps one and hands a
//!   [`RastersView`] snapshot to every frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;

use crate::frame::{Line, Rect, Span, Style};
use crate::width;

/// How wide a bitmap may be.
///
/// The limit is an arithmetic result, not a copied number: a cell is three
/// `u32`s = 12 bytes, so this is 128 × 64 = 8192 cells = 96 KiB of payload. A
/// bitmap is meant to be a widget, not a whole screen — and the reference
/// implementation's 512 × 256 would be 1.5 MiB before encoding.
pub const MAX_COLUMNS: u16 = 128;
/// How tall a bitmap may be. See [`MAX_COLUMNS`].
pub const MAX_ROWS: u16 = 64;

/// One cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// A printable BMP code point that is **one cell wide in the narrow
    /// convention** — what [`crate::width`] already treats as the authority.
    ///
    /// Ambiguous-width characters are accepted, not refused: this UI's own box
    /// drawing (`─│┌┐└┘`) is ambiguous too, so refusing them here would refuse
    /// the alphabet the rest of the screen already draws with and protect
    /// nothing. See `docs/adr/0027` decision ③.
    pub ch: char,
    /// `None` is the terminal's own colour — the payload's `0x0100_0000`.
    ///
    /// Raw RGB rather than a [`Color`]: which *drawable* colour this becomes
    /// depends on the terminal, and that is not known here. Decoding happens in
    /// [`Raster::lines_in`], which is handed the capabilities.
    pub fg: Option<crate::theme::Rgb>,
    pub bg: Option<crate::theme::Rgb>,
}

/// One bitmap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raster {
    pub columns: u16,
    pub rows: u16,
    cells: Vec<Cell>,
}

/// Why a payload was refused.
///
/// Every variant names what was wrong rather than saying "invalid": a raster can
/// be eight thousand cells, and "one of them is bad" leaves the caller guessing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RasterError {
    /// The payload is not base64.
    BadBase64,
    /// It decoded, but not to `columns * rows * 3` words.
    BadLength { got: usize, want: usize },
    /// One cell is not drawable. `index` is row-major.
    BadCell { index: usize, why: &'static str },
    /// The requested size is outside what a raster may be.
    TooLarge {
        columns: u16,
        rows: u16,
        max_columns: u16,
        max_rows: u16,
    },
    /// A write to a bitmap nobody mounted. Nothing is mounted implicitly.
    NotMounted { module: String, key: String },
    /// A write whose size is not the mounted one. Change size by unmounting.
    SizeMismatch { want: (u16, u16), got: (u16, u16) },
}

impl Raster {
    /// Decode and validate a payload.
    ///
    /// The size gate runs first, so an oversized raster is refused without
    /// decoding a megabyte to find out.
    pub fn decode(columns: u16, rows: u16, payload: &str) -> Result<Self, RasterError> {
        if columns == 0 || rows == 0 || columns > MAX_COLUMNS || rows > MAX_ROWS {
            return Err(RasterError::TooLarge {
                columns,
                rows,
                max_columns: MAX_COLUMNS,
                max_rows: MAX_ROWS,
            });
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|_| RasterError::BadBase64)?;
        let want = columns as usize * rows as usize * 12;
        if bytes.len() != want {
            return Err(RasterError::BadLength {
                got: bytes.len(),
                want,
            });
        }
        let mut cells = Vec::with_capacity(columns as usize * rows as usize);
        for (index, chunk) in bytes.as_chunks::<12>().0.iter().enumerate() {
            let word = |at: usize| {
                u32::from_le_bytes(chunk[at..at + 4].try_into().expect("a 4-byte window"))
            };
            let ch = char::from_u32(word(0))
                .filter(|c| !c.is_control() && (*c as u32) <= 0xffff)
                .ok_or(RasterError::BadCell {
                    index,
                    why: "not a printable BMP character",
                })?;
            let cells_width = width::char_width(ch);
            if cells_width != 1 {
                return Err(RasterError::BadCell {
                    index,
                    why: if cells_width == 2 {
                        "is 2 cells wide"
                    } else {
                        "is not one cell wide"
                    },
                });
            }
            cells.push(Cell {
                ch,
                fg: colour(word(4), index)?,
                bg: colour(word(8), index)?,
            });
        }
        Ok(Self {
            columns,
            rows,
            cells,
        })
    }

    /// Build one in this process, from cells that are already cells.
    ///
    /// [`decode`](Self::decode) is the wire: a writer outside this process
    /// sends base64 and it is validated on the way in. Something drawn *here* —
    /// a QR code, a chart — has no wire to cross, and encoding it only to
    /// decode it again would be a round trip whose only product is a chance to
    /// get the encoding wrong. Same rules either way: the size gate first, then
    /// every cell one column wide.
    pub fn from_cells(columns: u16, rows: u16, cells: Vec<Cell>) -> Result<Self, RasterError> {
        if columns == 0 || rows == 0 || columns > MAX_COLUMNS || rows > MAX_ROWS {
            return Err(RasterError::TooLarge {
                columns,
                rows,
                max_columns: MAX_COLUMNS,
                max_rows: MAX_ROWS,
            });
        }
        let want = columns as usize * rows as usize;
        if cells.len() != want {
            return Err(RasterError::BadLength {
                got: cells.len(),
                want,
            });
        }
        for (index, cell) in cells.iter().enumerate() {
            if cell.ch.is_control() || (cell.ch as u32) > 0xffff {
                return Err(RasterError::BadCell {
                    index,
                    why: "not a printable BMP character",
                });
            }
            if width::char_width(cell.ch) != 1 {
                return Err(RasterError::BadCell {
                    index,
                    why: "is not one cell wide",
                });
            }
        }
        Ok(Self {
            columns,
            rows,
            cells,
        })
    }

    /// The rows `rect` can show, each cut to `rect.w` cells.
    ///
    /// **The cost is the rectangle's, not the bitmap's**: a frame lays out only
    /// what is visible, the same trade `LiveCache` makes ("a frame's cost is the
    /// screen's, not the answer's"). Row 0 is the first row — a bitmap does not
    /// scroll inside itself.
    ///
    /// `caps` is how the colours become drawable ones. A bitmap states arbitrary
    /// RGB, which no role can express, so it goes through
    /// [`crate::theme::exact_colour`] — the one door an arbitrary RGB enters a
    /// frame by. Without it a 24-bit sequence goes to a 256-index terminal,
    /// which then guesses, and two terminals guess differently.
    pub fn lines_in(&self, rect: Rect, caps: crate::caps::Caps) -> Vec<Line> {
        let rows = (rect.h as usize).min(self.rows as usize);
        let cols = (rect.w as usize).min(self.columns as usize);
        if rows == 0 || cols == 0 {
            return Vec::new();
        }
        (0..rows)
            .map(|row| {
                let base = row * self.columns as usize;
                let slice = &self.cells[base..base + cols];
                // Runs of one style become one span, so a row's span count is
                // its runs rather than its cells.
                let mut spans: Vec<Span> = Vec::new();
                for cell in slice {
                    let style = style_of(cell, caps);
                    match spans.last_mut() {
                        Some(last) if last.style == style => last.text.push(cell.ch),
                        _ => spans.push(Span::styled(cell.ch.to_string(), style)),
                    }
                }
                Line::from_spans(spans)
            })
            .collect()
    }

    pub fn cell(&self, column: u16, row: u16) -> Option<&Cell> {
        if column >= self.columns || row >= self.rows {
            return None;
        }
        self.cells
            .get(row as usize * self.columns as usize + column as usize)
    }

    /// Whether every cell would survive a terminal with no Unicode.
    ///
    /// `█` and friends do — [`crate::caps::has_ascii_stand_in`] rewrites them to
    /// `#`, one column for one column, so such a bitmap is still a picture on an
    /// old console. Braille does **not** (deliberately: `caps.rs` explains why
    /// for the spinner), so a braille bitmap must not be drawn there rather than
    /// arriving as a grid of tofu.
    pub fn all_downgradable(&self) -> bool {
        self.cells
            .iter()
            .all(|cell| crate::caps::has_ascii_stand_in(cell.ch))
    }
}

/// `0x00RRGGBB` is a colour; `0x0100_0000` (bit 24 alone) is the terminal's own.
///
/// Returns a raw triple rather than a `Color` on purpose: which *drawable* colour
/// it becomes depends on the terminal, and that answer is not known here. A
/// `Color` built at decode time would be a colour nobody resolved — see
/// [`crate::theme::exact_colour`].
fn colour(word: u32, index: usize) -> Result<Option<crate::theme::Rgb>, RasterError> {
    match word {
        0x0100_0000 => Ok(None),
        w if w & 0xff00_0000 == 0 => Ok(Some(((w >> 16) as u8, (w >> 8) as u8, w as u8))),
        _ => Err(RasterError::BadCell {
            index,
            why: "colour word is neither 0x00RRGGBB nor 0x01000000",
        }),
    }
}

fn style_of(cell: &Cell, caps: crate::caps::Caps) -> Style {
    let mut style = Style::new();
    if let Some(fg) = cell.fg {
        style = style.fg(crate::theme::exact_colour(fg, caps));
    }
    if let Some(bg) = cell.bg {
        style = style.bg(crate::theme::exact_colour(bg, caps));
    }
    style
}

type Mounted = HashMap<(String, String), Arc<Raster>>;

/// The mounted bitmaps as of one frame.
///
/// Immutable, and cloning it is one `Arc` bump rather than a copy of the pixels
/// — so a `Moment` can carry it and two renders against that moment see the same
/// picture. That is the same promise `caps` and `cwd` keep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RastersView(Arc<Mounted>);

impl RastersView {
    /// The bitmap mounted under `(module, key)`.
    ///
    /// The lookup allocates two `String`s today. That is a per-frame cost on the
    /// module's side; it is bounded by the number of modules, not by the size of
    /// any bitmap, and it goes away when the key type does.
    pub fn get(&self, module: &str, key: &str) -> Option<&Arc<Raster>> {
        self.0.get(&(module.to_string(), key.to_string()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// The table a writer holds.
///
/// Copy-on-write: [`view`](Self::view) is O(1) because it is called every frame,
/// while rebuilding the map happens on a write. The same split `LiveCache` and
/// `Settled` make — the hot path is free and the rare path pays.
#[derive(Default)]
pub struct Rasters {
    current: Mutex<Arc<Mounted>>,
    revision: AtomicU64,
}

impl Rasters {
    pub fn new() -> Self {
        Self::default()
    }

    /// This frame's bitmaps. **O(1)** — see the type's docs.
    pub fn view(&self) -> RastersView {
        RastersView(self.current.lock().expect("rasters poisoned").clone())
    }

    /// How many writes have landed. Tests use it to say "that refusal changed
    /// nothing", which equality of the picture cannot show.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Relaxed)
    }

    /// Put a bitmap on screen under `(module, key)`.
    ///
    /// Mounting an id that is already mounted is a refusal rather than a
    /// replacement: a silent overwrite is how two widgets come to fight over one
    /// rectangle, and the caller that lost would never hear about it.
    pub fn mount(&self, module: &str, key: &str, raster: Raster) -> Result<(), RasterError> {
        let mut held = self.current.lock().expect("rasters poisoned");
        if held.contains_key(&(module.to_string(), key.to_string())) {
            return Err(RasterError::NotMounted {
                module: module.to_string(),
                key: key.to_string(),
            });
        }
        let mut next = (**held).clone();
        next.insert((module.to_string(), key.to_string()), Arc::new(raster));
        *held = Arc::new(next);
        self.revision.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    pub fn unmount(&self, module: &str, key: &str) {
        let mut held = self.current.lock().expect("rasters poisoned");
        if !held.contains_key(&(module.to_string(), key.to_string())) {
            return;
        }
        let mut next = (**held).clone();
        next.remove(&(module.to_string(), key.to_string()));
        *held = Arc::new(next);
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// Replace the contents of a mounted bitmap.
    ///
    /// The size comes from the mount, so a payload of the wrong size is refused
    /// as [`RasterError::BadLength`]. **A refusal changes nothing** — not the
    /// picture, not the revision — because a write that half landed would leave
    /// the widget showing a mixture of two frames.
    pub fn write(&self, module: &str, key: &str, payload: &str) -> Result<(), RasterError> {
        self.write_for(module, key, None, payload)
    }

    /// The same, by a caller that wants to **state** the size it believes.
    ///
    /// A caller that thinks its bitmap is 3×3 when 2×2 is mounted gets told
    /// that, instead of a byte-count that is off by exactly one row and says
    /// nothing about why. Same refusal semantics: nothing changes.
    pub fn write_sized(
        &self,
        module: &str,
        key: &str,
        columns: u16,
        rows: u16,
        payload: &str,
    ) -> Result<(), RasterError> {
        self.write_for(module, key, Some((columns, rows)), payload)
    }

    fn write_for(
        &self,
        module: &str,
        key: &str,
        stated: Option<(u16, u16)>,
        payload: &str,
    ) -> Result<(), RasterError> {
        let mut held = self.current.lock().expect("rasters poisoned");
        let id = (module.to_string(), key.to_string());
        let Some(existing) = held.get(&id) else {
            return Err(RasterError::NotMounted {
                module: module.to_string(),
                key: key.to_string(),
            });
        };
        let mounted = (existing.columns, existing.rows);
        if let Some(want) = stated {
            if want != mounted {
                return Err(RasterError::SizeMismatch { want, got: mounted });
            }
        }
        let raster = Raster::decode(mounted.0, mounted.1, payload)?;
        let mut next = (**held).clone();
        next.insert(id, Arc::new(raster));
        *held = Arc::new(next);
        self.revision.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests name a `Color`: the module itself handles raw RGB and lets
    // the palette resolve it, which is the point (see `theme::exact_colour`).
    use crate::frame::Color;

    /// One cell, encoded as the three little-endian words the format promises.
    fn cell(ch: char, fg: u32, bg: u32) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..4].copy_from_slice(&(ch as u32).to_le_bytes());
        out[4..8].copy_from_slice(&fg.to_le_bytes());
        out[8..12].copy_from_slice(&bg.to_le_bytes());
        out
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// The capability set a layout is drawn against in these tests.
    ///
    /// `Caps::default()` is a **256-index** terminal, not a truecolour one — worth
    /// stating because "the default is what a modern terminal does" reads as
    /// truecolour and is not (`caps.rs`). Tests that want exact colours ask for
    /// them explicitly.
    fn caps() -> crate::caps::Caps {
        crate::caps::Caps::default()
    }

    fn truecolour() -> crate::caps::Caps {
        crate::caps::Caps {
            colors: crate::caps::Colors::True,
            ..crate::caps::Caps::default()
        }
    }

    fn solid_payload(columns: u16, rows: u16) -> String {
        let mut bytes = Vec::new();
        for _ in 0..columns as usize * rows as usize {
            bytes.extend_from_slice(&cell('\u{2588}', 0x0100_0000, 0x0100_0000));
        }
        b64(&bytes)
    }

    fn solid(columns: u16, rows: u16) -> Raster {
        Raster::decode(columns, rows, &solid_payload(columns, rows)).expect("a solid raster")
    }

    /// A bitmap built in this process is held to the same rules as one that
    /// came over the wire.
    ///
    /// The shortcut this refuses: `from_cells` skipping validation because "we
    /// made it ourselves". A two-cell-wide character in a grid whose whole
    /// premise is one character per cell tears every row after it.
    #[test]
    fn a_bitmap_built_here_is_checked_like_one_that_arrived() {
        let cell = Cell {
            ch: '\u{2580}',
            fg: Some((0, 0, 0)),
            bg: Some((255, 255, 255)),
        };
        assert!(Raster::from_cells(2, 1, vec![cell, cell]).is_ok());
        assert_eq!(
            Raster::from_cells(2, 1, vec![cell]),
            Err(RasterError::BadLength { got: 1, want: 2 })
        );
        let wide = Cell {
            ch: '\u{4e2d}',
            ..cell
        };
        assert_eq!(
            Raster::from_cells(1, 1, vec![wide]),
            Err(RasterError::BadCell {
                index: 0,
                why: "is not one cell wide"
            })
        );
        assert!(matches!(
            Raster::from_cells(MAX_COLUMNS + 1, 1, vec![cell]),
            Err(RasterError::TooLarge { .. })
        ));
    }

    #[test]
    fn a_truecolour_terminal_gets_the_bitmaps_own_colours() {
        let payload = b64(&cell('\u{2588}', 0x00ff_8800, 0x0100_0000));
        let raster = Raster::decode(1, 1, &payload).expect("valid");
        let line = &raster.lines_in(Rect::sized(1, 1), truecolour())[0];
        assert_eq!(
            line.spans[0].style.fg,
            Some(Color::rgb((0xff, 0x88, 0x00))),
            "a truecolour terminal draws exactly what the bitmap said"
        );
    }

    #[test]
    fn a_256_colour_terminal_gets_an_index_and_never_a_24_bit_sequence() {
        // The bug this fixes: `Color::Rgb` was written straight through, so a
        // 256-index terminal was handed `38;2;…` and left to guess. Two terminals
        // guess differently, and a bitmap is the one thing on screen whose
        // colours nobody chose — so it is resolved against the palette like
        // everything else. See `theme::exact_colour`.
        let payload = b64(&cell('\u{2588}', 0x00ff_8800, 0x0100_0000));
        let raster = Raster::decode(1, 1, &payload).expect("valid");
        let ansi256 = crate::caps::Caps {
            colors: crate::caps::Colors::Ansi256,
            ..caps()
        };
        let line = &raster.lines_in(Rect::sized(1, 1), ansi256)[0];
        assert!(
            matches!(line.spans[0].style.fg, Some(Color::Ansi(_))),
            "expected an index, got {:?}",
            line.spans[0].style.fg
        );

        // And the proof at the byte level: no 24-bit sequence reaches the wire.
        let mut frame = crate::frame::Frame::new(1, 1);
        frame.place(
            "raster",
            Rect::sized(1, 1),
            raster.lines_in(Rect::sized(1, 1), ansi256),
        );
        let encoded = crate::ansi::encode_with(&frame, ansi256);
        assert!(
            !encoded.contains("38;2;") && !encoded.contains("48;2;"),
            "a 256-colour terminal must not be handed 24-bit colour: {encoded:?}"
        );
    }

    #[test]
    fn a_sixteen_colour_terminal_gets_one_of_its_sixteen() {
        let payload = b64(&cell('\u{2588}', 0x00ff_8800, 0x0100_0000));
        let raster = Raster::decode(1, 1, &payload).expect("valid");
        let ansi16 = crate::caps::Caps {
            colors: crate::caps::Colors::Ansi16,
            ..caps()
        };
        let line = &raster.lines_in(Rect::sized(1, 1), ansi16)[0];
        match line.spans[0].style.fg {
            Some(Color::Ansi(n)) => assert!(n <= 15, "slot {n} is not one of the sixteen"),
            other => panic!("expected a slot, got {other:?}"),
        }
    }

    #[test]
    fn a_terminal_with_no_colour_gets_no_colour_sequences() {
        let payload = b64(&cell('\u{2588}', 0x00ff_8800, 0x00ff_0000));
        let raster = Raster::decode(1, 1, &payload).expect("valid");
        let none = crate::caps::Caps {
            colors: crate::caps::Colors::None,
            ..caps()
        };
        let mut frame = crate::frame::Frame::new(1, 1);
        frame.place(
            "raster",
            Rect::sized(1, 1),
            raster.lines_in(Rect::sized(1, 1), none),
        );
        let encoded = crate::ansi::encode_with(&frame, none);
        assert!(
            !encoded.contains("38;") && !encoded.contains("48;"),
            "no colours at all means no colour sequences: {encoded:?}"
        );
    }

    #[test]
    fn a_one_cell_raster_decodes_and_draws() {
        let payload = b64(&cell('\u{2588}', 0x00ff_8800, 0x0100_0000));
        let raster = Raster::decode(1, 1, &payload).expect("valid");
        // A truecolour terminal, because this asks about the *decoding*: what the
        // bitmap said. What a narrower terminal gets instead is the next test.
        let lines = raster.lines_in(Rect::sized(1, 1), truecolour());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].plain(), "\u{2588}");
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(Color::rgb((0xff, 0x88, 0x00))),
            "0x00RRGGBB is a colour"
        );
        assert_eq!(
            lines[0].spans[0].style.bg, None,
            "0x01000000 means the terminal's own background, not a colour"
        );
    }

    #[test]
    fn a_payload_that_is_not_base64_is_refused() {
        assert_eq!(
            Raster::decode(1, 1, "!!!!").unwrap_err(),
            RasterError::BadBase64
        );
    }

    #[test]
    fn a_payload_of_the_wrong_length_names_both_numbers() {
        let payload = b64(&cell('x', 0, 0)[..6]);
        assert_eq!(
            Raster::decode(1, 1, &payload).unwrap_err(),
            RasterError::BadLength { got: 6, want: 12 }
        );
    }

    #[test]
    fn an_illegal_cell_names_its_index() {
        let mut bytes = cell('x', 0, 0).to_vec();
        bytes.extend_from_slice(&cell('\u{4e2d}', 0, 0));
        match Raster::decode(2, 1, &b64(&bytes)).unwrap_err() {
            RasterError::BadCell { index, why } => {
                assert_eq!(index, 1, "the refusal must name which cell");
                assert!(why.contains('2'), "and how wide it measured: {why}");
            }
            other => panic!("expected BadCell, got {other:?}"),
        }
    }

    #[test]
    fn braille_and_ambiguous_blocks_are_both_accepted() {
        // The executable form of the correction in `docs/adr/0027` decision ③:
        // one cell wide is the whole test. Braille is EAW=N (the safest of the
        // candidates) and `█` is EAW=A — the same class as this UI's box
        // drawing, so refusing it would refuse the alphabet already in use.
        for ch in ['\u{2800}', '\u{2588}', '\u{2580}', '\u{2591}', '\u{2592}'] {
            let payload = b64(&cell(ch, 0, 0));
            Raster::decode(1, 1, &payload)
                .unwrap_or_else(|e| panic!("{ch:?} should be accepted, got {e:?}"));
        }
    }

    #[test]
    fn a_colour_word_that_is_neither_form_is_refused_and_names_its_cell() {
        let mut bytes = cell('x', 0, 0).to_vec();
        bytes.extend_from_slice(&cell('y', 0xff00_0000, 0));
        match Raster::decode(2, 1, &b64(&bytes)).unwrap_err() {
            RasterError::BadCell { index, why } => {
                assert_eq!(index, 1);
                assert!(why.contains("colour"), "{why}");
            }
            other => panic!("expected BadCell, got {other:?}"),
        }
    }

    #[test]
    fn the_size_limit_is_a_hard_gate() {
        let at_limit = solid(MAX_COLUMNS, 1);
        assert_eq!(at_limit.columns, MAX_COLUMNS, "the limit itself is allowed");
        let over = MAX_COLUMNS + 1;
        assert_eq!(
            Raster::decode(over, 1, "").unwrap_err(),
            RasterError::TooLarge {
                columns: over,
                rows: 1,
                max_columns: MAX_COLUMNS,
                max_rows: MAX_ROWS
            },
            "and it is checked before the payload is decoded"
        );
    }

    #[test]
    fn only_the_rect_is_rendered_so_a_frame_costs_the_screen() {
        let raster = solid(4, 4);
        let lines = raster.lines_in(Rect::sized(2, 2), caps());
        assert_eq!(lines.len(), 2, "rows past the rect are not laid out");
        for line in &lines {
            assert_eq!(line.width(), 2, "columns past the rect are cut");
        }
    }

    #[test]
    fn a_rect_with_no_room_draws_nothing() {
        let raster = solid(4, 4);
        assert!(raster.lines_in(Rect::sized(0, 4), caps()).is_empty());
        assert!(raster.lines_in(Rect::sized(4, 0), caps()).is_empty());
    }

    #[test]
    fn adjacent_cells_of_one_style_become_one_span() {
        // Not a micro-optimisation: a row of one colour is one span, so a row's
        // span count is its runs rather than its cells.
        let raster = solid(4, 1);
        let lines = raster.lines_in(Rect::sized(4, 1), caps());
        assert_eq!(lines[0].spans.len(), 1);
        assert_eq!(lines[0].plain(), "\u{2588}\u{2588}\u{2588}\u{2588}");
    }

    #[test]
    fn a_break_between_styles_starts_a_new_span() {
        let mut bytes = cell('\u{2588}', 0x00ff0000, 0x0100_0000).to_vec();
        bytes.extend_from_slice(&cell('\u{2588}', 0x0000ff00, 0x0100_0000));
        let raster = Raster::decode(2, 1, &b64(&bytes)).expect("valid");
        let lines = raster.lines_in(Rect::sized(2, 1), caps());
        assert_eq!(lines[0].spans.len(), 2, "a colour change is a new span");
    }

    // ---- the table -------------------------------------------------------

    #[test]
    fn writing_to_a_raster_nobody_mounted_is_refused() {
        let rasters = Rasters::new();
        assert_eq!(
            rasters.write("pane", "main", "AA==").unwrap_err(),
            RasterError::NotMounted {
                module: "pane".into(),
                key: "main".into()
            },
            "nothing is mounted implicitly"
        );
    }

    #[test]
    fn mounting_one_id_twice_is_refused_rather_than_replacing() {
        let rasters = Rasters::new();
        rasters.mount("pane", "main", solid(2, 2)).unwrap();
        assert!(
            rasters.mount("pane", "main", solid(3, 3)).is_err(),
            "a silent overwrite is how two widgets come to fight over one rect"
        );
    }

    #[test]
    fn a_write_of_the_wrong_size_is_refused_and_changes_nothing() {
        let rasters = Rasters::new();
        rasters.mount("pane", "main", solid(2, 2)).unwrap();
        let before = rasters.revision();
        // No size stated: the payload simply is not 2×2's worth of cells.
        assert_eq!(
            rasters
                .write("pane", "main", &b64(&cell('x', 0, 0)))
                .unwrap_err(),
            RasterError::BadLength { got: 12, want: 48 }
        );
        assert_eq!(
            rasters.revision(),
            before,
            "a refusal must change nothing, not even the revision"
        );
    }

    #[test]
    fn a_caller_that_states_the_wrong_size_is_told_which_size_it_got() {
        // The clearer refusal: "you said 3×3, this is mounted at 2×2" beats a
        // byte count that is off by exactly one row.
        let rasters = Rasters::new();
        rasters.mount("pane", "main", solid(2, 2)).unwrap();
        let before = rasters.revision();
        let payload = b64(&[0u8; 3 * 3 * 12]);
        assert_eq!(
            rasters
                .write_sized("pane", "main", 3, 3, &payload)
                .unwrap_err(),
            RasterError::SizeMismatch {
                want: (3, 3),
                got: (2, 2)
            }
        );
        assert_eq!(rasters.revision(), before, "and it changes nothing either");
        // Stating the mounted size is fine.
        assert!(rasters
            .write_sized("pane", "main", 2, 2, &solid_payload(2, 2))
            .is_ok());
    }

    #[test]
    fn a_write_that_lands_bumps_the_revision_and_swaps_the_bitmap() {
        let rasters = Rasters::new();
        rasters.mount("pane", "main", solid(1, 1)).unwrap();
        let before = rasters.view().get("pane", "main").cloned();
        let drawn_before = before
            .as_ref()
            .map(|r| r.lines_in(Rect::sized(1, 1), caps())[0].plain());
        rasters
            .write("pane", "main", &b64(&cell('x', 0, 0)))
            .unwrap();
        let after = rasters.view().get("pane", "main").cloned();
        let drawn_after = after
            .as_ref()
            .map(|r| r.lines_in(Rect::sized(1, 1), caps())[0].plain());
        assert_ne!(drawn_before, drawn_after, "the write landed");
        assert_eq!(drawn_after.as_deref(), Some("x"));
        assert!(rasters.revision() > 0);
    }

    #[test]
    fn a_view_is_a_snapshot_so_a_later_write_cannot_change_it() {
        // The promise `caps` and `cwd` keep: two renders against one `Moment`
        // see one picture. Without this, a bitmap could tear mid-frame.
        let rasters = Rasters::new();
        rasters.mount("pane", "main", solid(1, 1)).unwrap();
        let held = rasters.view();
        rasters
            .write("pane", "main", &b64(&cell('y', 0, 0)))
            .unwrap();
        let draw = |v: &RastersView| {
            v.get("pane", "main")
                .map(|r| r.lines_in(Rect::sized(1, 1), caps())[0].plain())
        };
        assert_eq!(
            draw(&held),
            draw(&held),
            "the snapshot must not move between two renders"
        );
        assert_eq!(
            draw(&held).as_deref(),
            Some("\u{2588}"),
            "it still has the old one"
        );
        assert_eq!(
            draw(&rasters.view()).as_deref(),
            Some("y"),
            "and the new one is separate"
        );
    }

    #[test]
    fn unmounting_takes_it_out_of_the_next_view() {
        let rasters = Rasters::new();
        rasters.mount("pane", "main", solid(1, 1)).unwrap();
        assert!(!rasters.view().is_empty());
        rasters.unmount("pane", "main");
        assert!(rasters.view().is_empty());
    }

    #[test]
    fn two_modules_do_not_see_each_others_bitmaps() {
        let rasters = Rasters::new();
        rasters.mount("left", "main", solid(1, 1)).unwrap();
        rasters.mount("right", "main", solid(2, 2)).unwrap();
        let view = rasters.view();
        assert_eq!(view.get("left", "main").map(|r| r.columns), Some(1));
        assert_eq!(view.get("right", "main").map(|r| r.columns), Some(2));
        assert!(
            view.get("left", "other").is_none(),
            "the key is half the address"
        );
    }
}
