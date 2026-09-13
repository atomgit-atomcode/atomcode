//! What gets painted: geometry, a styled line, and a composed frame.
//!
//! A frame is a **value**. Nothing here writes to a terminal — that is the
//! `surface` seam's job. Keeping the frame a value is what lets a test assert
//! on what would be shown without a tty, and what lets the same frame be
//! checked against a real terminal emulator's cell grid (the external oracle).

use std::fmt;

/// A rectangle in cells. Origin is top-left, `(0, 0)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    /// A rect at the origin — the shape a module usually cares about, since a
    /// module must render the same regardless of where it sits.
    pub const fn sized(w: u16, h: u16) -> Self {
        Self::new(0, 0, w, h)
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub const fn right(&self) -> u16 {
        self.x + self.w
    }

    pub const fn bottom(&self) -> u16 {
        self.y + self.h
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Split vertically: `top` rows above, the rest below. Saturates rather
    /// than panicking — a layout op must never be able to crash the host.
    pub fn split_v(&self, top: u16) -> (Rect, Rect) {
        let top = top.min(self.h);
        (
            Rect::new(self.x, self.y, self.w, top),
            Rect::new(self.x, self.y + top, self.w, self.h - top),
        )
    }

    /// Split horizontally: `left` columns, then the rest.
    pub fn split_h(&self, left: u16) -> (Rect, Rect) {
        let left = left.min(self.w);
        (
            Rect::new(self.x, self.y, left, self.h),
            Rect::new(self.x + left, self.y, self.w - left, self.h),
        )
    }
}

/// How a run of text is drawn. Deliberately small: a theme maps meaning to
/// colour, so modules speak in roles rather than in ANSI.
///
/// Attributes only — and deliberately not SGR 2 (`faint`). "Darker than
/// whatever the terminal's foreground is" is a contrast the terminal picks,
/// after the palette has done arithmetic to guarantee one, and nothing in this
/// tree can measure the result. That is the same failure the role palette exists
/// to remove. Metadata wants [`Role::Muted`]: a colour this tree chose, which
/// `--probe-terminal` reports and a test can hold to a floor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

impl Style {
    pub const fn new() -> Self {
        Self {
            fg: None,
            bg: None,
            bold: false,
            italic: false,
            underline: false,
            reverse: false,
        }
    }
    /// Fill unset fields from `base`. What this style states wins; what it
    /// leaves open is inherited — the same rule CSS uses, and what lets a
    /// container give every child a background without every child knowing.
    pub fn under(self, base: Style) -> Style {
        Style {
            fg: self.fg.or(base.fg),
            bg: self.bg.or(base.bg),
            bold: self.bold || base.bold,
            italic: self.italic || base.italic,
            underline: self.underline || base.underline,
            reverse: self.reverse || base.reverse,
        }
    }

    pub const fn fg(mut self, c: Color) -> Self {
        self.fg = Some(c);
        self
    }
    pub const fn bg(mut self, c: Color) -> Self {
        self.bg = Some(c);
        self
    }
    pub const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
    pub const fn reverse(mut self) -> Self {
        self.reverse = true;
        self
    }
    pub const fn italic(mut self) -> Self {
        self.italic = true;
        self
    }
    pub const fn underline(mut self) -> Self {
        self.underline = true;
        self
    }
}

/// A colour, in the terminal's own vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Color {
    /// One of the 256 indexed colours.
    Ansi(u8),
    Rgb(u8, u8, u8),
    /// What this text *is*, resolved to an actual colour at paint time.
    ///
    /// The same trick as the glyph fallback, for the same reason: a module has
    /// no idea whether the terminal is light or dark, and threading that answer
    /// through every `render` and every `Content::lines` would mean every one
    /// of them could get it wrong. Instead they state the role and
    /// [`crate::ansi::encode_with`] — which does know — resolves it.
    Role(crate::theme::Role),
}

impl Color {
    /// Shorthand, because this is how nearly every colour should be written.
    pub const fn role(r: crate::theme::Role) -> Color {
        Color::Role(r)
    }

    /// An exact colour, from a measured triple. Only the palette resolver
    /// produces these, and only when no slot in the user's scheme reads.
    pub fn rgb((r, g, b): crate::theme::Rgb) -> Color {
        Color::Rgb(r, g, b)
    }
}

/// A run of text sharing one style. Lines are made of these so a renderer can
/// emit one escape sequence per run rather than one per character.
///
/// `Hash` is derived rather than hand-written because it feeds the repaint
/// diff: a style field added later must change a row's fingerprint, or the row
/// it was added to would keep painting its old bytes. Deriving it makes that
/// the default instead of the thing someone has to remember.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

impl Span {
    pub fn raw(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::new(),
        }
    }
    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
    /// Display width in cells, which is not the byte length and not the char
    /// count — CJK and emoji occupy two.
    pub fn width(&self) -> usize {
        crate::width::str_width(&self.text)
    }
}

/// One rendered row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn raw(text: impl Into<String>) -> Self {
        Self {
            spans: vec![Span::raw(text)],
        }
    }

    pub fn styled(text: impl Into<String>, style: Style) -> Self {
        Self {
            spans: vec![Span::styled(text, style)],
        }
    }

    pub fn from_spans(spans: Vec<Span>) -> Self {
        Self { spans }
    }

    pub fn push(&mut self, span: Span) {
        self.spans.push(span);
    }

    pub fn width(&self) -> usize {
        self.spans.iter().map(Span::width).sum()
    }

    /// The text with styling dropped. For assertions and for the exit dump.
    pub fn plain(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// Cut to `w` cells, never mid-grapheme and never mid-wide-character.
    pub fn truncate(&self, w: usize) -> Line {
        if self.width() <= w {
            return self.clone();
        }
        let mut out = Vec::new();
        let mut used = 0usize;
        for span in &self.spans {
            if used >= w {
                break;
            }
            let room = w - used;
            let cut = crate::width::take_width(&span.text, room);
            used += crate::width::str_width(&cut);
            if !cut.is_empty() {
                out.push(Span::styled(cut, span.style));
            }
        }
        Line { spans: out }
    }
}

impl Line {
    /// A copy with the cells in `[from, to)` restyled.
    ///
    /// Spans are split at the boundaries and never mid-grapheme, so a selection
    /// that lands in the middle of a word — or in the middle of a CJK character
    /// — still highlights whole cells. Cells, not bytes and not chars: the
    /// selection is a rectangle on screen, and that is what the person drew.
    pub fn restyle(&self, from: usize, to: usize, f: impl Fn(Style) -> Style) -> Line {
        if from >= to {
            return self.clone();
        }
        let mut out: Vec<Span> = Vec::with_capacity(self.spans.len());
        let mut at = 0usize;
        for span in &self.spans {
            let w = span.width();
            let (lo, hi) = (at, at + w);
            at = hi;
            if hi <= from || lo >= to {
                out.push(span.clone());
                continue;
            }
            // Up to three pieces: before the range, inside it, after it.
            // The two slices below cut at the *byte length of a grapheme
            // prefix* `take_width` just returned, so the index is a boundary by
            // construction — not by anyone's arithmetic.
            let head = crate::width::take_width(&span.text, from.saturating_sub(lo));
            #[allow(
                clippy::string_slice,
                reason = "cut at the length of a take_width prefix, which is a grapheme boundary"
            )]
            let rest = &span.text[head.len()..];
            let inside =
                crate::width::take_width(rest, to.min(hi) - (lo + crate::width::str_width(&head)));
            #[allow(
                clippy::string_slice,
                reason = "cut at the length of a take_width prefix, which is a grapheme boundary"
            )]
            let tail = &rest[inside.len()..];
            for (text, style) in [
                (head.as_str(), span.style),
                (inside.as_str(), f(span.style)),
                (tail, span.style),
            ] {
                if !text.is_empty() {
                    out.push(Span::styled(text, style));
                }
            }
        }
        Line { spans: out }
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.plain())
    }
}

/// Lines placed at a rect, tagged with who produced them.
///
/// The tag is not decoration: the containment check ("every cell a module drew
/// is inside the rect it was given") is what makes spatial composability
/// verifiable per module, and it needs to know whose cell each one is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub owner: String,
    pub rect: Rect,
    pub lines: Vec<Line>,
}

/// A whole screen, composed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub size: Rect,
    pub parts: Vec<Placed>,
    /// Where the terminal cursor should end up, if anywhere.
    pub cursor: Option<(u16, u16)>,
}

impl Frame {
    pub fn new(w: u16, h: u16) -> Self {
        Self {
            size: Rect::sized(w, h),
            parts: Vec::new(),
            cursor: None,
        }
    }

    pub fn place(&mut self, owner: impl Into<String>, rect: Rect, lines: Vec<Line>) {
        self.parts.push(Placed {
            owner: owner.into(),
            rect,
            lines,
        });
    }

    /// What one module drew.
    pub fn part(&self, owner: &str) -> Option<&Placed> {
        self.parts.iter().find(|p| p.owner == owner)
    }

    /// Flatten to plain rows, for assertions and the exit dump. Later parts
    /// overwrite earlier ones, which is what `Region::Stack` means.
    pub fn rows(&self) -> Vec<String> {
        let mut grid: Vec<Vec<char>> = vec![vec![' '; self.size.w as usize]; self.size.h as usize];
        for part in &self.parts {
            for (dy, line) in part.lines.iter().enumerate() {
                let y = part.rect.y as usize + dy;
                if y >= grid.len() || dy >= part.rect.h as usize {
                    break;
                }
                let mut x = part.rect.x as usize;
                for ch in line.truncate(part.rect.w as usize).plain().chars() {
                    let cw = crate::width::char_width(ch);
                    if x >= grid[y].len() {
                        break;
                    }
                    grid[y][x] = ch;
                    for k in 1..cw {
                        if x + k < grid[y].len() {
                            grid[y][x + k] = '\0';
                        }
                    }
                    x += cw.max(1);
                }
            }
        }
        grid.into_iter()
            .map(|row| row.into_iter().filter(|c| *c != '\0').collect())
            .collect()
    }

    /// Mark the cells a selection covers, wherever they were drawn.
    ///
    /// Applied to the composed frame rather than by each module, because a
    /// selection is a rectangle on the *screen*: it crosses parts, and a module
    /// asked to highlight its own share would have to know where it sits and
    /// what its neighbours did.
    pub fn highlight(&mut self, sel: &crate::moment::Selection) {
        if sel.is_empty() {
            return;
        }
        let width = self.size.w;
        for part in &mut self.parts {
            for (dy, line) in part.lines.iter_mut().enumerate() {
                let row = part.rect.y as usize + dy;
                let Ok(row) = u16::try_from(row) else {
                    continue;
                };
                let Some((a, b)) = sel.on_row(row, width) else {
                    continue;
                };
                // Screen cells to this part's own, clipped to its rect.
                let a = a.max(part.rect.x) - part.rect.x;
                let b = b.min(part.rect.right()).saturating_sub(part.rect.x);
                if a < b {
                    *line = line.restyle(a as usize, b as usize, |st| Style {
                        reverse: !st.reverse,
                        ..st
                    });
                }
            }
        }
    }

    /// The text a selection covers, as a person would expect to paste it.
    ///
    /// Read back from the flattened frame rather than from the blocks behind
    /// it: what was selected is what was *on screen*, wrapped the way it was
    /// wrapped. Trailing blanks go, because a terminal's own selection drops
    /// them and pasting a rectangle of spaces is never what was meant.
    pub fn selected_text(&self, sel: &crate::moment::Selection) -> String {
        if sel.is_empty() {
            return String::new();
        }
        let rows = self.rows();
        let mut out: Vec<String> = Vec::new();
        for (y, row) in rows.iter().enumerate() {
            let Ok(y) = u16::try_from(y) else { continue };
            let Some((a, b)) = sel.on_row(y, self.size.w) else {
                continue;
            };
            let head = crate::width::take_width(row, a as usize);
            #[allow(
                clippy::string_slice,
                reason = "cut at the length of a take_width prefix, which is a grapheme boundary"
            )]
            let rest = &row[head.len()..];
            let piece = crate::width::take_width(rest, (b - a) as usize);
            out.push(piece.trim_end().to_string());
        }
        out.join("\n")
    }

    /// Every cell a module drew is inside the rect it was given.
    ///
    /// The machine-checkable form of "a module occupies its part of the window
    /// and no more" — the pixel-level verdict on spatial composability.
    pub fn containment_violations(&self) -> Vec<String> {
        let mut out = Vec::new();
        for part in &self.parts {
            if part.lines.len() > part.rect.h as usize {
                out.push(format!(
                    "`{}` drew {} lines into a rect {} tall",
                    part.owner,
                    part.lines.len(),
                    part.rect.h
                ));
            }
            for (i, line) in part.lines.iter().enumerate() {
                if line.width() > part.rect.w as usize {
                    out.push(format!(
                        "`{}` line {i} is {} cells wide in a rect {} wide",
                        part.owner,
                        line.width(),
                        part.rect.w
                    ));
                }
            }
            if part.rect.right() > self.size.w || part.rect.bottom() > self.size.h {
                out.push(format!(
                    "`{}` was given {:?}, which is outside the {}×{} screen",
                    part.owner, part.rect, self.size.w, self.size.h
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_saturates_instead_of_panicking() {
        let r = Rect::sized(10, 4);
        let (a, b) = r.split_v(99);
        assert_eq!(a.h, 4);
        assert_eq!(b.h, 0);
        let (a, b) = r.split_h(99);
        assert_eq!(a.w, 10);
        assert_eq!(b.w, 0);
    }

    #[test]
    fn width_counts_cells_not_bytes_or_chars() {
        assert_eq!(Span::raw("abc").width(), 3);
        assert_eq!(Span::raw("中文").width(), 4, "CJK is two cells each");
        assert_eq!(Span::raw("").width(), 0);
    }

    #[test]
    fn truncating_never_splits_a_wide_character() {
        let line = Line::raw("中文abc");
        assert_eq!(
            line.truncate(3).plain(),
            "中",
            "3 cells cannot hold two CJK"
        );
        assert_eq!(line.truncate(4).plain(), "中文");
        assert_eq!(line.truncate(99).plain(), "中文abc");
    }

    #[test]
    fn containment_catches_a_module_drawing_outside_its_box() {
        let mut f = Frame::new(10, 3);
        f.place("good", Rect::new(0, 0, 10, 1), vec![Line::raw("hi")]);
        assert!(f.containment_violations().is_empty());

        f.place(
            "greedy",
            Rect::new(0, 1, 4, 1),
            vec![Line::raw("far too wide"), Line::raw("and too tall")],
        );
        let v = f.containment_violations();
        // One for the line count, one for each over-wide line.
        assert_eq!(v.len(), 3, "{v:?}");
        assert!(v.iter().all(|m| m.contains("greedy")));
    }

    #[test]
    fn a_selection_marks_the_cells_it_covers_and_no_others() {
        use crate::moment::Selection;
        let mut f = Frame::new(10, 2);
        f.place("a", Rect::new(0, 0, 10, 1), vec![Line::raw("abcdefghij")]);
        f.place("b", Rect::new(0, 1, 10, 1), vec![Line::raw("klmnopqrst")]);
        f.highlight(&Selection {
            anchor: (2, 0),
            head: (4, 0),
        });
        let marked: String = f.parts[0].lines[0]
            .spans
            .iter()
            .filter(|s| s.style.reverse)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(marked, "cde", "the head's own cell is selected too");
        assert!(
            f.parts[1].lines[0].spans.iter().all(|s| !s.style.reverse),
            "a one-row selection reached the row below"
        );
    }

    #[test]
    fn dragging_upward_selects_the_same_text_as_dragging_down_over_it() {
        use crate::moment::Selection;
        let text = |sel: &Selection| {
            let mut f = Frame::new(6, 3);
            for (y, s) in ["one---", "two---", "three-"].iter().enumerate() {
                f.place(
                    format!("r{y}"),
                    Rect::new(0, y as u16, 6, 1),
                    vec![Line::raw(*s)],
                );
            }
            f.selected_text(sel)
        };
        let down = Selection {
            anchor: (1, 0),
            head: (2, 2),
        };
        let up = Selection {
            anchor: (2, 2),
            head: (1, 0),
        };
        assert_eq!(text(&down), text(&up));
        assert_eq!(
            text(&down),
            "ne---
two---
thr"
        );
    }

    #[test]
    fn what_is_copied_is_what_was_on_screen() {
        use crate::moment::Selection;
        let mut f = Frame::new(20, 2);
        // Two parts side by side: a selection crosses them, because it is a
        // rectangle on the screen and knows nothing about who drew what.
        f.place("left", Rect::new(0, 0, 10, 1), vec![Line::raw("hello")]);
        f.place("right", Rect::new(10, 0, 10, 1), vec![Line::raw("world")]);
        let all = Selection {
            anchor: (0, 0),
            head: (19, 0),
        };
        assert_eq!(f.selected_text(&all), "hello     world");

        // Trailing blanks go: a terminal drops them and a rectangle of spaces
        // is never what was meant.
        let tail = Selection {
            anchor: (5, 0),
            head: (19, 0),
        };
        assert_eq!(f.selected_text(&tail), "     world");
        assert_eq!(
            f.selected_text(&Selection::at(3, 0)),
            "",
            "a press is not a selection"
        );
    }

    #[test]
    fn a_selection_never_splits_a_wide_character() {
        use crate::moment::Selection;
        let mut f = Frame::new(8, 1);
        f.place("a", Rect::new(0, 0, 8, 1), vec![Line::raw("中文abc")]);
        // Cells 0..=2 cover 中 (two cells) and half of 文 — the half cannot be
        // taken, so it is not.
        let sel = Selection {
            anchor: (0, 0),
            head: (2, 0),
        };
        assert_eq!(f.selected_text(&sel), "中");
        let mut marked = f.clone();
        marked.highlight(&sel);
        let hot: String = marked.parts[0].lines[0]
            .spans
            .iter()
            .filter(|s| s.style.reverse)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(hot, "中");
    }

    #[test]
    fn rows_flatten_to_a_grid_the_size_of_the_screen() {
        let mut f = Frame::new(6, 2);
        f.place("a", Rect::new(0, 0, 6, 1), vec![Line::raw("abc")]);
        f.place("b", Rect::new(2, 1, 4, 1), vec![Line::raw("xy")]);
        assert_eq!(f.rows(), vec!["abc   ".to_string(), "  xy  ".to_string()]);
    }
}
