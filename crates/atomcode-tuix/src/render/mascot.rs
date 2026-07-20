//! Baked pixel-art mascot (orange cat) for the welcome banner.
//!
//! Generated offline from `atomcode.gif` (see `scripts/gen_mascot.py`), NOT read
//! at runtime. Each row is `2 * MASCOT_WIDTH` chars: two chars per cell
//! `(top, bottom)`, rendered as the upper-half-block `▀` with fg=top / bg=bottom.
//! Legend: '.' transparent, 'o' orange, 'w' white, 'k' black.

use crossterm::style::Color;

pub const MASCOT_WIDTH: usize = 9;

pub const MASCOT_ROWS: [&str; 4] = [
    "oooo.o.o.o.o.ooooo",
    "ooooooowokooowokoo",
    "oooooowowooowowooo",
    "..o.ooooooooooo...",
];

/// Map a legend byte to its 256-color value; `.` (transparent) → None.
pub fn mascot_color(subpixel: u8) -> Option<Color> {
    match subpixel {
        b'o' => Some(Color::AnsiValue(202)), // orange  (#ff5f00)
        b'w' => Some(Color::AnsiValue(231)), // white
        b'k' => Some(Color::AnsiValue(232)), // near-black pupil
        _ => None,                           // '.' transparent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_well_formed() {
        assert!(!MASCOT_ROWS.is_empty());
        for (i, row) in MASCOT_ROWS.iter().enumerate() {
            assert_eq!(row.len(), 2 * MASCOT_WIDTH, "row {i} wrong width");
            assert!(
                row.bytes().all(|b| matches!(b, b'.' | b'o' | b'w' | b'k')),
                "row {i} has an illegal legend char"
            );
        }
    }

    #[test]
    fn color_map_matches_legend() {
        assert_eq!(mascot_color(b'.'), None);
        assert_eq!(mascot_color(b'o'), Some(Color::AnsiValue(202)));
        assert_eq!(mascot_color(b'w'), Some(Color::AnsiValue(231)));
        assert_eq!(mascot_color(b'k'), Some(Color::AnsiValue(232)));
    }
}
