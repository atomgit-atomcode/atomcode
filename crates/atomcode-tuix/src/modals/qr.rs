//! Terminal QR rendering for the onboarding wizard.
//!
//! The compact path uses explicit black/white foreground and background
//! colours around a half-block. The legacy path uses only background-coloured
//! spaces, avoiding font glyph geometry completely. Both paths render standard
//! black-on-white polarity with a four-module quiet zone and reject output that
//! cannot fit in the available rectangle.

use qrcode::{Color as QrColor, QrCode};

const QUIET_ZONE: usize = 4;
const BLACK_FG_WHITE_BG: &str = "\x1b[38;5;0;48;5;15m";
const WHITE_FG_BLACK_BG: &str = "\x1b[38;5;15;48;5;0m";
const BLACK_CELL: &str = "\x1b[48;5;0m  ";
const WHITE_CELL: &str = "\x1b[48;5;15m  ";
const RESET: &str = "\x1b[0m";

/// Render `data` as a QR code suitable for terminal display.
/// Each returned row is one terminal line; callers can pad / centre
/// each row inside the wizard panel without re-splitting the string.
///
/// Rendering strategy:
///   - reliable half-blocks + colours: compact explicit-colour renderer;
///   - colours only: full-height background-space renderer;
///   - no colours, or insufficient bounds: `None` (show the URL instead).
pub(super) fn render_for_terminal(
    data: &str,
    reliable_half_blocks: bool,
    colors: bool,
    max_width: usize,
    max_height: usize,
) -> Option<Vec<String>> {
    if !colors {
        return None;
    }
    let code = QrCode::new(data.as_bytes()).ok()?;
    if reliable_half_blocks {
        render_compact(&code, max_width, max_height)
    } else {
        render_background_spaces(&code, max_width, max_height)
    }
}

fn module_is_dark(code: &QrCode, x: isize, y: isize) -> bool {
    if x < 0 || y < 0 || x >= code.width() as isize || y >= code.width() as isize {
        return false;
    }
    matches!(code[(x as usize, y as usize)], QrColor::Dark)
}

fn render_compact(code: &QrCode, max_width: usize, max_height: usize) -> Option<Vec<String>> {
    let modules = code.width() + QUIET_ZONE * 2;
    let rows = modules.div_ceil(2);
    if modules > max_width || rows > max_height {
        return None;
    }
    let mut output = Vec::with_capacity(rows);
    for cell_y in 0..rows {
        let mut line = String::with_capacity(modules * 12);
        let top_y = cell_y * 2;
        let bottom_y = top_y + 1;
        for cell_x in 0..modules {
            let qr_x = cell_x as isize - QUIET_ZONE as isize;
            let top = module_is_dark(code, qr_x, top_y as isize - QUIET_ZONE as isize);
            let bottom = module_is_dark(code, qr_x, bottom_y as isize - QUIET_ZONE as isize);
            match (top, bottom) {
                (true, true) => line.push_str("\x1b[48;5;0m "),
                (false, false) => line.push_str("\x1b[48;5;15m "),
                (true, false) => {
                    line.push_str(BLACK_FG_WHITE_BG);
                    line.push('▀');
                }
                (false, true) => {
                    line.push_str(WHITE_FG_BLACK_BG);
                    line.push('▀');
                }
            }
        }
        line.push_str(RESET);
        output.push(line);
    }
    Some(output)
}

fn render_background_spaces(
    code: &QrCode,
    max_width: usize,
    max_height: usize,
) -> Option<Vec<String>> {
    let modules = code.width() + QUIET_ZONE * 2;
    let columns = modules * 2;
    if columns > max_width || modules > max_height {
        return None;
    }
    let mut output = Vec::with_capacity(modules);
    for cell_y in 0..modules {
        let mut line = String::with_capacity(columns * 6);
        for cell_x in 0..modules {
            let dark = module_is_dark(
                code,
                cell_x as isize - QUIET_ZONE as isize,
                cell_y as isize - QUIET_ZONE as isize,
            );
            line.push_str(if dark { BLACK_CELL } else { WHITE_CELL });
        }
        line.push_str(RESET);
        output.push(line);
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_renderer_uses_only_background_colored_spaces() {
        let lines = render_for_terminal("https://example.com", false, true, 80, 80)
            .expect("background-space rendering must succeed");
        assert!(!lines.is_empty());
        assert!(lines.iter().all(|line| {
            !line.contains('█') && !line.contains('▀') && !line.contains('▄')
        }));
        assert!(lines.iter().all(|line| line.ends_with(RESET)));
    }

    #[test]
    fn render_produces_non_empty_block_for_short_url() {
        // Smoke test with the shape of an actual atomgit short link.
        // 32-char URL encodes to roughly a 25x25-module QR; the compact
        // renderer packs two rows per cell. Use 8 as a
        // safe floor — any non-trivial input should clear it.
        let lines = render_for_terminal("https://acs.atomgit.com/s/AbC123", true, true, 80, 24)
            .expect("Unicode-capable render must succeed for a short URL");
        assert!(
            lines.len() >= 8,
            "expected at least 8 rows, got {}: {:#?}",
            lines.len(),
            lines
        );
        for (i, row) in lines.iter().enumerate() {
            assert!(!row.is_empty(), "row {i} must not be empty");
        }
    }

    #[test]
    fn render_rows_have_uniform_char_width() {
        // The wizard panel centres each row by computing its display
        // width once and reusing the value across rows. Non-uniform
        // widths (e.g. trimmed trailing whitespace) would render the
        // QR as a parallelogram and break phone scanning. Pin
        // uniform width so any future renderer swap can't silently
        // regress this.
        let lines = render_for_terminal("https://example.com", true, true, 80, 24)
            .expect("render must succeed");
        let first = crate::width::display_width(&crate::sanitize::scrub_controls(&lines[0]));
        for (i, row) in lines.iter().enumerate() {
            assert_eq!(
                crate::width::display_width(&crate::sanitize::scrub_controls(row)),
                first,
                "row {i} visible width differs from row 0 ({first}): {row:?}"
            );
        }
    }

    #[test]
    fn compact_renderer_has_white_quiet_zone_and_black_finder_modules() {
        let lines = render_for_terminal("https://example.com", true, true, 80, 24).unwrap();
        assert!(
            lines[0].contains("\x1b[48;5;15m "),
            "top quiet zone must be explicit white"
        );
        assert!(
            lines.iter().any(|line| line.contains("\x1b[48;5;0m ")),
            "QR dark modules must be explicit black"
        );
    }

    #[test]
    fn compact_colors_survive_the_retained_cell_parser() {
        use crossterm::style::Color;

        let lines = render_for_terminal("https://example.com", true, true, 80, 24).unwrap();
        let mut cells = Vec::new();
        crate::render::cell::push_str_cells_sgr(
            &mut cells,
            &lines.join(""),
            crate::render::cell::CellStyle::default(),
        );

        assert!(cells
            .iter()
            .any(|cell| cell.style.bg == Some(Color::AnsiValue(0))));
        assert!(cells
            .iter()
            .any(|cell| cell.style.bg == Some(Color::AnsiValue(15))));
    }

    #[test]
    fn compact_renderer_round_trips_the_complete_qr_matrix() {
        use crossterm::style::Color;

        let data = "https://acs.atomgit.com/s/AbC123";
        let code = QrCode::new(data.as_bytes()).unwrap();
        let lines = render_for_terminal(data, true, true, 80, 24).unwrap();
        let modules = code.width() + QUIET_ZONE * 2;

        for (cell_y, line) in lines.iter().enumerate() {
            let mut cells = Vec::new();
            crate::render::cell::push_str_cells_sgr(
                &mut cells,
                line,
                crate::render::cell::CellStyle::default(),
            );
            assert_eq!(cells.len(), modules);
            for (cell_x, cell) in cells.iter().enumerate() {
                let black = Some(Color::AnsiValue(0));
                let white = Some(Color::AnsiValue(15));
                let (top, bottom) = match (cell.ch, cell.style.fg, cell.style.bg) {
                    (' ', _, bg) if bg == black => (true, true),
                    (' ', _, bg) if bg == white => (false, false),
                    ('▀', fg, bg) if fg == black && bg == white => (true, false),
                    ('▀', fg, bg) if fg == white && bg == black => (false, true),
                    other => panic!("unexpected compact QR cell: {other:?}"),
                };
                let qr_x = cell_x as isize - QUIET_ZONE as isize;
                let top_y = cell_y * 2;
                let bottom_y = top_y + 1;
                assert_eq!(
                    top,
                    module_is_dark(&code, qr_x, top_y as isize - QUIET_ZONE as isize)
                );
                if bottom_y < modules {
                    assert_eq!(
                        bottom,
                        module_is_dark(&code, qr_x, bottom_y as isize - QUIET_ZONE as isize)
                    );
                }
            }
        }
    }

    #[test]
    fn renderer_rejects_incomplete_or_uncolored_output() {
        assert!(render_for_terminal("https://example.com", true, false, 80, 24).is_none());
        assert!(render_for_terminal("https://example.com", true, true, 8, 24).is_none());
        assert!(render_for_terminal("https://example.com", false, true, 80, 8).is_none());
    }
}
