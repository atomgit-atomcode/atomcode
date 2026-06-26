use crate::render::cell::Cell;

const PAD_COL: usize = 2;

pub struct CopyMode {
    snapshot: Vec<Vec<Cell>>,
    width: u16,
    viewport_h: u16,
    top: usize,                        // first visible snapshot index
    anchor: Option<(usize, usize)>,    // (snapshot_row, col)
    cursor: Option<(usize, usize)>,
}

impl CopyMode {
    pub fn new(snapshot: Vec<Vec<Cell>>, width: u16, viewport_h: u16) -> Self {
        let vh = viewport_h.max(1) as usize;
        // Show the tail (most recent rows), like the live view.
        let top = snapshot.len().saturating_sub(vh);
        Self { snapshot, width, viewport_h, top, anchor: None, cursor: None }
    }

    pub fn top(&self) -> usize { self.top }

    pub fn scroll(&mut self, delta: i32) {
        let vh = self.viewport_h.max(1) as usize;
        let max_top = self.snapshot.len().saturating_sub(vh);
        let next = self.top as i64 + delta as i64;
        self.top = next.clamp(0, max_top as i64) as usize;
    }

    fn to_buf(&self, col: u16, row: u16) -> (usize, usize) {
        let r = (self.top + row as usize).min(self.snapshot.len().saturating_sub(1));
        (r, col as usize)
    }

    pub fn begin_selection(&mut self, col: u16, row: u16) {
        let p = self.to_buf(col, row);
        self.anchor = Some(p);
        self.cursor = Some(p);
    }

    pub fn update_selection(&mut self, col: u16, row: u16) {
        self.cursor = Some(self.to_buf(col, row));
    }

    pub fn has_selection(&self) -> bool {
        match (self.anchor, self.cursor) {
            (Some(a), Some(c)) => a != c,
            _ => false,
        }
    }

    /// Inclusive (start, end) ordered span, or None.
    fn ordered(&self) -> Option<((usize, usize), (usize, usize))> {
        let (a, c) = (self.anchor?, self.cursor?);
        Some(if a <= c { (a, c) } else { (c, a) })
    }

    pub fn selected_text(&self) -> String {
        let Some((start, end)) = self.ordered() else { return String::new() };
        let mut out: Vec<String> = Vec::new();
        for r in start.0..=end.0 {
            let Some(cells) = self.snapshot.get(r) else { continue };
            let begin = if r == start.0 { start.1 } else { 0 };
            // inclusive end column on the last row; whole row otherwise
            let stop = if r == end.0 { end.1.saturating_add(1) } else { cells.len() };
            let mut s: String = cells.iter()
                .enumerate()
                .filter(|(i, c)| *i >= begin && *i < stop && c.width != 0)
                .map(|(_, c)| c.ch)
                .collect();
            // de-indent: rows that begin at column 0 carry the PAD_COL indent
            if begin == 0 {
                let strip = s.chars().take(PAD_COL).take_while(|c| *c == ' ').count();
                s = s.chars().skip(strip).collect();
            }
            out.push(s.trim_end().to_string());
        }
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::CopyMode;
    use crate::render::cell::Cell;

    fn row(s: &str) -> Vec<Cell> {
        s.chars().map(|c| Cell { ch: c, style: Default::default(), width: 1 }).collect()
    }

    #[test]
    fn extracts_single_row_substring() {
        let snap = vec![row("  hello world")]; // PAD_COL=2 indent
        let mut cm = CopyMode::new(snap, 80, 10);
        cm.begin_selection(2, 0);   // 'h'
        cm.update_selection(6, 0);  // 'o' (inclusive)
        assert_eq!(cm.selected_text(), "hello");
    }

    #[test]
    fn extracts_multi_row_with_deindent_and_trim() {
        let snap = vec![row("  abc   "), row("  def")];
        let mut cm = CopyMode::new(snap, 80, 10);
        cm.begin_selection(2, 0);   // 'a'
        cm.update_selection(4, 1);  // 'f'
        // first row from 'a' to EOL (trailing spaces trimmed), middle/last de-indented
        assert_eq!(cm.selected_text(), "abc\ndef");
    }

    #[test]
    fn reversed_drag_is_normalised() {
        let snap = vec![row("  abcde")];
        let mut cm = CopyMode::new(snap, 80, 10);
        cm.begin_selection(6, 0);   // 'e'
        cm.update_selection(2, 0);  // 'a'
        assert_eq!(cm.selected_text(), "abcde");
    }
}
