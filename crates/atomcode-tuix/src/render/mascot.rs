//! Baked pixel-art mascot (orange cat) for the welcome banner.
//!
//! 9 cells wide × 4 rows. The face/ears/chin come from the offline generator
//! (`scripts/gen_mascot.py`), but the EYES are hand-tuned: each 2×2 eye is a
//! glossy black pupil with a single white highlight in its TOP-LEFT corner
//! (`w` over `k k k`), and a slightly-darker-orange "eyebrow" (`e`) sits in the
//! row directly above each eye. NOT read at runtime. Each row is
//! `2 * MASCOT_WIDTH` chars: two chars per cell `(top, bottom)`, rendered as the
//! upper-half-block `▀` with fg=top / bg=bottom.
//! Legend: '.' transparent, 'o' orange, 'e' dark-orange eyebrow, 'w' white, 'k' black.

use crossterm::style::Color;

pub const MASCOT_WIDTH: usize = 9;

pub const MASCOT_ROWS: [&str; 4] = [
    "oooo.o.o.o.o.ooooo",
    "ooooooewekooewekoo",
    "ooooookokoookokooo",
    "..o.ooooooooooo...",
];

/// Map a legend byte to its 256-color value; `.` (transparent) → None.
pub fn mascot_color(subpixel: u8) -> Option<Color> {
    match subpixel {
        b'o' => Some(Color::AnsiValue(202)), // orange       (#ff5f00)
        b'e' => Some(Color::AnsiValue(166)), // eyebrow: darker orange (#d75f00)
        b'w' => Some(Color::AnsiValue(231)), // white highlight
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
                row.bytes()
                    .all(|b| matches!(b, b'.' | b'o' | b'e' | b'w' | b'k')),
                "row {i} has an illegal legend char"
            );
        }
    }

    #[test]
    fn color_map_matches_legend() {
        assert_eq!(mascot_color(b'.'), None);
        assert_eq!(mascot_color(b'o'), Some(Color::AnsiValue(202)));
        assert_eq!(mascot_color(b'e'), Some(Color::AnsiValue(166)));
        assert_eq!(mascot_color(b'w'), Some(Color::AnsiValue(231)));
        assert_eq!(mascot_color(b'k'), Some(Color::AnsiValue(232)));

        // Each eye must be a glossy pupil: exactly one white highlight (top-left)
        // and the remaining three quadrants black.
        let eye_row = MASCOT_ROWS[1];
        assert_eq!(
            eye_row.matches('w').count(),
            2,
            "one white highlight per eye (2 eyes)"
        );
        assert_eq!(
            eye_row.matches('e').count(),
            4,
            "eyebrow above each eye (2 cells × 2 eyes)"
        );
    }
}
