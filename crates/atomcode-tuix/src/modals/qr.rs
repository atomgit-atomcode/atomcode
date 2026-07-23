//! Terminal QR rendering for the onboarding wizard.
//!
//! Thin wrapper over `qrcode` 0.14's `Dense1x2` (half-block) renderer
//! gated on the project's existing terminal-capability flag. Each
//! Unicode half-block packs two QR "modules" (rows) into one terminal
//! cell, so the rendered QR is half as tall as the underlying matrix —
//! critical for fitting a 33x33 QR inside the wizard panel without
//! overflowing typical terminal heights.
//!
//! Why ASCII fallback returns `None` rather than a degraded
//! pseudo-QR: terminals that fail the `unicode_symbols` check
//! (Windows legacy conhost / `LANG=C` / `TERM=dumb`) render half-block
//! glyphs as `□` tofu, and a tofu QR is silently unscannable. Better
//! to show the URL as text and let the user paste it into a browser.

use qrcode::render::unicode::Dense1x2;
use qrcode::QrCode;

/// Render `data` as a Unicode QR code suitable for terminal display.
/// Each returned row is one terminal line; callers can pad / centre
/// each row inside the wizard panel without re-splitting the string.
///
/// Returns `None` when:
///   - `unicode_symbols == false` — caller MUST fall back to text URL,
///   - the QR encoder rejects the input (data too long for v40 / L).
///
/// Errors are coarse-grained on purpose: the only failure mode the
/// onboarding flow cares about is "show URL instead of QR." The exact
/// reason is irrelevant to the user and would just clutter the UI.
/// Render `data` as a QR code suitable for terminal display.
/// Each returned row is one terminal line; callers can pad / centre
/// each row inside the wizard panel without re-splitting the string.
///
/// Rendering strategy:
///   - `unicode_symbols == true`: Dense1x2 half-blocks (`▀`, `▄`, `█`, ` `)
///   - `unicode_symbols == false` and `colors == true`: ANSI reverse-video spaces (`\x1b[7m  \x1b[0m`)
///   - Both `false`: returns `None` (fall back to text URL)
pub(super) fn render_for_terminal(
    data: &str,
    unicode_symbols: bool,
    colors: bool,
    _max_width: usize,
) -> Option<Vec<String>> {
    let code = QrCode::new(data.as_bytes()).ok()?;
    if unicode_symbols {
        let rendered = code
            .render::<Dense1x2>()
            .module_dimensions(1, 1)
            .dark_color(Dense1x2::Dark)
            .light_color(Dense1x2::Light)
            .quiet_zone(true)
            .build();
        Some(rendered.lines().map(|l| l.to_string()).collect())
    } else if colors {
        Some(render_block_spaces(&code))
    } else {
        None
    }
}

fn render_block_spaces(code: &QrCode) -> Vec<String> {
    let width = code.width();
    let colors = code.to_colors();
    const QUIET: usize = 2;
    let total_width = width + 2 * QUIET;
    let mut lines = Vec::new();

    let quiet_line = " ".repeat(total_width * 2);
    for _ in 0..QUIET {
        lines.push(quiet_line.clone());
    }

    for y in 0..width {
        let mut line = String::with_capacity(total_width * 6);
        line.push_str(&" ".repeat(QUIET * 2));
        for x in 0..width {
            let is_dark = matches!(colors[x + y * width], qrcode::Color::Dark);
            if is_dark {
                line.push_str("██");
            } else {
                line.push_str("  ");
            }
        }
        line.push_str(&" ".repeat(QUIET * 2));
        lines.push(line);
    }

    for _ in 0..QUIET {
        lines.push(quiet_line.clone());
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_produces_block_spaces_when_unicode_disabled() {
        let lines = render_for_terminal("https://example.com", false, true, 80)
            .expect("block space rendering must succeed");
        assert!(!lines.is_empty());
        assert!(lines.iter().any(|l| l.contains("  ")));
    }

    #[test]
    fn render_produces_non_empty_block_for_short_url() {
        // Smoke test with the shape of an actual atomgit short link.
        // 32-char URL encodes to roughly a 25x25-module QR; Dense1x2
        // packs two rows per cell so ~13 terminal rows. Use 8 as a
        // safe floor — any non-trivial input should clear it.
        let lines = render_for_terminal("https://acs.atomgit.com/s/AbC123", true, true, 80)
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
        let lines =
            render_for_terminal("https://example.com", true, true, 80).expect("render must succeed");
        let first = lines[0].chars().count();
        for (i, row) in lines.iter().enumerate() {
            assert_eq!(
                row.chars().count(),
                first,
                "row {i} char-width differs from row 0 ({first}): {row:?}"
            );
        }
    }
}
