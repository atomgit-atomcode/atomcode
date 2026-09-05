//! Turning a [`Frame`] into bytes a terminal understands.
//!
//! Kept apart from the surface so the same encoder feeds both the real terminal
//! and the test that reads those bytes back through a terminal emulator. If the
//! encoder and the assertion shared no code the test would prove nothing about
//! what ships; if they shared *all* of it the test would prove nothing at all.
//! The split is: this produces bytes, and the oracle interprets them by the
//! terminal's rules rather than by ours.

use std::fmt::Write as _;

use crate::frame::{Color, Frame, Line, Style};

/// Move the cursor home and clear.
pub const CLEAR: &str = "\x1b[H\x1b[2J";
/// Enter the alternate screen and hide the cursor.
pub const ENTER: &str = "\x1b[?1049h\x1b[?25l";
/// The exact inverse of [`ENTER`].
pub const LEAVE: &str = "\x1b[?25h\x1b[?1049l";

fn sgr(style: &Style) -> String {
    let mut parts: Vec<String> = Vec::new();
    if style.bold {
        parts.push("1".into());
    }
    if style.dim {
        parts.push("2".into());
    }
    if style.italic {
        parts.push("3".into());
    }
    if style.underline {
        parts.push("4".into());
    }
    if style.reverse {
        parts.push("7".into());
    }
    match style.fg {
        Some(Color::Ansi(n)) => parts.push(format!("38;5;{n}")),
        Some(Color::Rgb(r, g, b)) => parts.push(format!("38;2;{r};{g};{b}")),
        None => {}
    }
    match style.bg {
        Some(Color::Ansi(n)) => parts.push(format!("48;5;{n}")),
        Some(Color::Rgb(r, g, b)) => parts.push(format!("48;2;{r};{g};{b}")),
        None => {}
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", parts.join(";"))
    }
}

fn write_line(out: &mut String, line: &Line, width: u16, caps: crate::caps::Caps) {
    let clipped = line.truncate(width as usize);
    for span in &clipped.spans {
        // Swap first, then measure nothing: every substitution is the same
        // number of columns, so clipping above stays correct.
        let text = caps.text(&span.text);
        let codes = if caps.colors == crate::caps::Colors::None {
            String::new()
        } else {
            sgr(&span.style)
        };
        if codes.is_empty() {
            out.push_str(&text);
        } else {
            out.push_str(&codes);
            out.push_str(&text);
            out.push_str("\x1b[0m");
        }
    }
}

/// Encode a whole frame: absolute positioning per part, so a module can never
/// shift another one by miscounting its own height.
///
/// Positioning is absolute *by construction*, which is the encoder's half of
/// the containment guarantee: even a module that returns too many lines cannot
/// push its neighbour down the screen — it is clipped at its own rect.
/// Paint a frame for a terminal with these capabilities.
///
/// This is where the OS shield actually bites: glyphs the terminal cannot show
/// are swapped for ASCII of the same width, and colours it does not have are
/// dropped. Doing it here rather than at every call site is what makes the
/// layering enforceable — a module has no opportunity to forget, because it was
/// never asked.
pub fn encode_with(frame: &Frame, caps: crate::caps::Caps) -> String {
    let mut out = String::with_capacity(1024);
    out.push_str(CLEAR);
    for part in &frame.parts {
        for (dy, line) in part.lines.iter().enumerate() {
            if dy >= part.rect.h as usize {
                break;
            }
            let row = part.rect.y as usize + dy + 1; // ANSI is 1-based
            let col = part.rect.x as usize + 1;
            let _ = write!(out, "\x1b[{row};{col}H");
            write_line(&mut out, line, part.rect.w, caps);
        }
    }
    match frame.cursor {
        Some((x, y)) => {
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", y + 1, x + 1);
        }
        None => out.push_str("\x1b[?25l"),
    }
    out
}

/// Paint for a fully capable terminal. Tests and callers that do not care.
pub fn encode(frame: &Frame) -> String {
    encode_with(frame, crate::caps::Caps::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Rect, Span};

    #[test]
    fn a_plain_frame_positions_every_part_absolutely() {
        let mut f = Frame::new(20, 3);
        f.place("a", Rect::new(0, 0, 20, 1), vec![Line::raw("top")]);
        f.place("b", Rect::new(2, 2, 18, 1), vec![Line::raw("bottom")]);
        let s = encode(&f);
        assert!(s.starts_with(CLEAR));
        assert!(s.contains("\x1b[1;1Htop"));
        assert!(s.contains("\x1b[3;3Hbottom"), "row 3, column 3");
    }

    #[test]
    fn a_module_returning_too_many_lines_is_clipped_not_allowed_to_push() {
        let mut f = Frame::new(10, 4);
        f.place(
            "greedy",
            Rect::new(0, 0, 10, 1),
            vec![Line::raw("one"), Line::raw("two"), Line::raw("three")],
        );
        f.place("after", Rect::new(0, 1, 10, 1), vec![Line::raw("safe")]);
        let s = encode(&f);
        assert!(s.contains("one"));
        assert!(!s.contains("two"), "clipped at its own rect");
        assert!(s.contains("\x1b[2;1Hsafe"), "the neighbour keeps its row");
    }

    #[test]
    fn styling_is_always_closed() {
        let mut f = Frame::new(10, 1);
        f.place(
            "s",
            Rect::new(0, 0, 10, 1),
            vec![Line::from_spans(vec![
                Span::styled("hi", Style::new().bold()),
                Span::raw("there"),
            ])],
        );
        let s = encode(&f);
        assert_eq!(
            s.matches("\x1b[0m").count(),
            1,
            "one open, one close; the unstyled span emits nothing"
        );
        assert!(s.contains("\x1b[1mhi\x1b[0m"));
    }

    #[test]
    fn leave_is_the_exact_inverse_of_enter() {
        // A UI that leaves a shell in the alternate screen is worse than no UI.
        assert!(ENTER.contains("1049h") && LEAVE.contains("1049l"));
        assert!(ENTER.contains("25l") && LEAVE.contains("25h"));
    }
}
