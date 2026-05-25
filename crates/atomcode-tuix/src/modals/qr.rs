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
pub(super) fn render_for_terminal(data: &str, unicode_symbols: bool) -> Option<Vec<String>> {
    if !unicode_symbols {
        return None;
    }
    let code = QrCode::new(data.as_bytes()).ok()?;
    let rendered = code
        .render::<Dense1x2>()
        .module_dimensions(1, 1)
        .dark_color(Dense1x2::Dark)
        .light_color(Dense1x2::Light)
        .build();
    Some(rendered.lines().map(|l| l.to_string()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_returns_none_when_unicode_disabled() {
        // ASCII-only / dumb terminals get None so the caller falls
        // back to text URL rendering. Half-block glyphs (▀ / ▄ / █)
        // render as `□` tofu on Windows conhost / `TERM=dumb` /
        // `LANG=C` — a tofu QR would silently break the only thing
        // this screen exists to do (let the user scan and log in).
        assert!(render_for_terminal("https://example.com", false).is_none());
    }

    #[test]
    fn render_produces_non_empty_block_for_short_url() {
        // Smoke test with the shape of an actual atomgit short link.
        // 32-char URL encodes to roughly a 25x25-module QR; Dense1x2
        // packs two rows per cell so ~13 terminal rows. Use 8 as a
        // safe floor — any non-trivial input should clear it.
        let lines = render_for_terminal("https://acs.atomgit.com/s/AbC123", true)
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
            render_for_terminal("https://example.com", true).expect("render must succeed");
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
