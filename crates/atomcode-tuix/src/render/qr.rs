//! Render a URL as a terminal-friendly QR code for the OAuth login
//! flow (`/login`, `/codingplan`).
//!
//! Two styles, both Unicode:
//! * `Dense1x2` (default): 1×2 modules per char using `▀▄█`.
//!   ≈ 45 cols × 23 rows. Block elements (U+2580–U+259F) are
//!   Unicode-Neutral width, so no terminal renders them at double
//!   width. QR aspect stays 1:1 and scanners decode reliably under
//!   any configuration.
//! * `Braille` (opt-in): packs 2×4 modules into one U+2800–U+28FF
//!   char. ≈ 23 cols × 12 rows — about half the size. But Braille
//!   is Unicode-Ambiguous width and gets stretched 2× horizontally
//!   on terminals that default ambiguous-width to double (iTerm2's
//!   "Treat ambiguous-width characters as double width", on by
//!   default in many profiles). Use only when you know your
//!   terminal renders braille at single cell width.
//!
//! There is no ASCII rendering: at typical 1:2 monospace cell
//! ratios, an ASCII QR with `module_dimensions(2, 1)` exceeds 90
//! columns for any usable URL — wider than every realistic terminal
//! window. Callers (`compose_login_chrome`) must short-circuit to
//! URL-only output when `TerminalCaps::unicode_symbols` is false.

use qrcode::render::unicode::Dense1x2;
use qrcode::{Color, QrCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QrStyle {
    Braille,
    Dense1x2,
}

/// Render `url` as a multi-line QR string. Returns `None` only when QR
/// encoding itself fails (URL exceeds version 40 capacity ~4 KB —
/// never the case for OAuth URLs in practice).
pub fn render_login_qr(url: &str, style: QrStyle) -> Option<String> {
    let code = QrCode::new(url.as_bytes()).ok()?;
    let s = match style {
        QrStyle::Braille => render_braille(&code),
        QrStyle::Dense1x2 => code
            .render::<Dense1x2>()
            .dark_color(Dense1x2::Dark)
            .light_color(Dense1x2::Light)
            .quiet_zone(true)
            .build(),
    };
    Some(s)
}

/// Visible column count of the widest line in `rendered`. Both styles
/// emit exactly one terminal cell per char, so `chars().count()` is
/// the correct cell count.
pub fn block_cols(rendered: &str) -> usize {
    rendered
        .lines()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
}

/// Pack a QR matrix into Braille glyphs (2 cols × 4 rows of modules
/// per char). Standard 4-module quiet zone is added before packing —
/// scanners need the white margin to find the corner finder patterns.
///
/// Braille bit layout (Unicode 6.0):
/// ```text
///   col 0 row 0 → 0x01     col 1 row 0 → 0x08
///   col 0 row 1 → 0x02     col 1 row 1 → 0x10
///   col 0 row 2 → 0x04     col 1 row 2 → 0x20
///   col 0 row 3 → 0x40     col 1 row 3 → 0x80
/// ```
fn render_braille(code: &QrCode) -> String {
    const BRAILLE_BITS: [[u32; 4]; 2] = [
        [0x01, 0x02, 0x04, 0x40], // left column
        [0x08, 0x10, 0x20, 0x80], // right column
    ];
    const QUIET: usize = 4;

    let width = code.width();
    let colors = code.to_colors();
    let total = width + 2 * QUIET;

    // Light (false) outside the data area, including the quiet zone.
    let is_dark = |x: usize, y: usize| -> bool {
        if x < QUIET || x >= QUIET + width || y < QUIET || y >= QUIET + width {
            return false;
        }
        matches!(colors[(x - QUIET) + (y - QUIET) * width], Color::Dark)
    };

    let cols = total.div_ceil(2);
    let rows = total.div_ceil(4);
    let mut out = String::with_capacity(rows * (cols * 3 + 1));

    for cy in 0..rows {
        for cx in 0..cols {
            let mut bits: u32 = 0;
            for dx in 0..2usize {
                for dy in 0..4usize {
                    if is_dark(cx * 2 + dx, cy * 4 + dy) {
                        bits |= BRAILLE_BITS[dx][dy];
                    }
                }
            }
            // 0x2800 + bits is always within U+2800..=U+28FF (8-bit
            // payload), so unwrap is safe.
            out.push(char::from_u32(0x2800 + bits).unwrap());
        }
        if cy + 1 < rows {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_URL: &str =
        "https://acs.atomgit.com/auth/login?state=abcd1234efgh5678ijkl9012mnop3456";

    #[test]
    fn braille_path_emits_braille_chars() {
        let rendered = render_login_qr(SAMPLE_URL, QrStyle::Braille).expect("qr encodes");
        assert!(
            rendered
                .chars()
                .any(|c| (c as u32) >= 0x2800 && (c as u32) <= 0x28FF),
            "expected at least one braille char in QR, got: {:?}",
            rendered.chars().take(40).collect::<String>()
        );
    }

    #[test]
    fn dense1x2_path_emits_block_glyphs() {
        let rendered = render_login_qr(SAMPLE_URL, QrStyle::Dense1x2).expect("qr encodes");
        assert!(
            rendered
                .chars()
                .any(|c| matches!(c, '\u{2588}' | '\u{2580}' | '\u{2584}')),
            "expected at least one block glyph in dense1x2 QR, got: {:?}",
            rendered.chars().take(40).collect::<String>()
        );
    }

    #[test]
    fn rendered_lines_have_uniform_width() {
        for style in [QrStyle::Braille, QrStyle::Dense1x2] {
            let rendered = render_login_qr(SAMPLE_URL, style).expect("qr encodes");
            let widths: Vec<usize> = rendered.lines().map(|l| l.chars().count()).collect();
            let first = widths[0];
            assert!(
                widths.iter().all(|w| *w == first),
                "rendered QR ({:?}) has uneven line widths: {:?}",
                style,
                widths
            );
        }
    }

    #[test]
    fn braille_is_about_half_size_of_dense1x2() {
        // Braille packs 2×4 modules per char, Dense1x2 packs 1×2.
        // So braille should be ~½ width and ~½ height of Dense1x2.
        let braille = render_login_qr(SAMPLE_URL, QrStyle::Braille).unwrap();
        let dense = render_login_qr(SAMPLE_URL, QrStyle::Dense1x2).unwrap();
        let (bw, bh) = (block_cols(&braille), braille.lines().count());
        let (dw, dh) = (block_cols(&dense), dense.lines().count());
        // Allow ±1 cell slack from ceiling rounding.
        assert!(
            bw * 2 <= dw + 1,
            "braille width {} should be ~½ of dense1x2 width {}",
            bw,
            dw
        );
        assert!(
            bh * 2 <= dh + 1,
            "braille height {} should be ~½ of dense1x2 height {}",
            bh,
            dh
        );
    }

    #[test]
    fn block_cols_matches_widest_line() {
        let rendered = "###\n#####\n##";
        assert_eq!(block_cols(rendered), 5);
    }
}
