// crates/atomcode-tuix/src/modals/file_viewer.rs
//
// `/view` — a borderless, fixed-height inline file viewer (the `/diff` house
// style): the panel is drawn where the input box sits, covering it, NOT a
// centred floating popup and NOT a full-screen takeover.
//
// Two internal views, mirroring `DiffViewer`'s List→Detail shape and sizing:
//   - `Picker`  : `/view` with no argument opens a files-only fuzzy selector,
//                 reusing the `@`-mention `FileIndex`. Type to filter, ↑↓ to
//                 select, Enter to open, Esc to cancel. Directories are never
//                 listed (you can't `/view` a directory). Given a stable
//                 fixed-height panel (~45%) rather than `/diff`'s content-fit
//                 list — a fuzzy search wants a steady size and many matches
//                 visible, not a panel that resizes as you type.
//   - `Content` : the selected file (or `/view <path>`) rendered with line
//                 numbers in a taller panel (~75%, matching the `/diff` detail
//                 view) so a long file has room to scroll. ↑↓/PgUp/Home/End
//                 scroll; Esc backs out to the picker (or closes for a direct
//                 `/view <path>`).
//
// `/view <path>` accepts absolute, `~`-prefixed, and project-relative paths
// (resolved by the caller in `commands.rs`), so files OUTSIDE the project open
// fine.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::file_index::{Entry, FileIndex};
use crate::event_loop::{Buffer, LoopCtx};
use crate::render::{DiffPanelRow, DiffPanelSpan, DiffPanelTone, Renderer, UiLine};
use crate::state::UiState;

/// Max lines displayed for one file (to keep memory reasonable).
const MAX_VIEW_LINES: usize = 1000;
/// Truncate individual lines to this display width.
const MAX_LINE_LEN: usize = 2000;
/// Hard cap on bytes pulled from disk. Sized to comfortably hold
/// `MAX_VIEW_LINES` × `MAX_LINE_LEN` of worst-case 4-byte UTF-8, so it never
/// clips a file the viewer would have shown in full, while bounding the
/// pathological (multi-GB / single-giant-line) case.
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

/// Locale-picked literal (matches `DiffViewer`'s `l` helper).
fn l(en: &'static str, zh: &'static str) -> &'static str {
    match crate::i18n::current_locale() {
        crate::i18n::Locale::ZhCn => zh,
        _ => en,
    }
}

/// Returns `(screen_w, panel_height, content_height)`: a FIXED-height panel
/// drawn inline where the input box sits (covering it), NOT a full-screen
/// takeover. The content view uses the ~75% height matching `DiffViewer`'s
/// file-detail view so a long file has room to scroll; the picker uses a
/// steadier ~45% so a fuzzy search doesn't resize as you type. `content_height`
/// is the body area — `panel_height` minus four chrome rows (top rule, title,
/// blank spacer, hint).
fn geometry(is_content: bool) -> (u16, u16, usize) {
    let (screen_w, screen_h) = crossterm::terminal::size().unwrap_or((80, 24));
    let h = screen_h as usize;
    // `.max(lo).min(h)` rather than `clamp(lo, h)` — the latter PANICS when the
    // terminal is shorter than `lo` (min > max).
    let panel_height = if is_content {
        (h * 3 / 4).max(10).min(h)
    } else {
        (h * 9 / 20).max(8).min(h)
    };
    let content_height = panel_height.saturating_sub(4).max(1);
    (screen_w, panel_height as u16, content_height)
}

/// Loaded file content backing the `Content` view.
struct Content {
    path: PathBuf,
    lines: Vec<String>,
    scroll: usize,
    total_lines: usize,
    truncated: bool,
}

/// Files-only fuzzy picker backing the `Picker` view.
struct Picker {
    index: FileIndex,
    working_dir: PathBuf,
    query: String,
    /// Matching FILE entries (directories filtered out).
    matches: Vec<Entry>,
    selected: usize,
    /// Transient error from a failed open (e.g. binary file), shown in-panel.
    error: Option<String>,
}

impl Picker {
    /// Re-run the index filter for the current query, dropping directories.
    fn refresh(&mut self) {
        self.matches = self
            .index
            .filter("", &self.query)
            .into_iter()
            .filter(|e| !e.is_dir)
            .collect();
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn move_up(&mut self, n: usize) {
        self.selected = self.selected.saturating_sub(n);
    }

    fn move_down(&mut self, n: usize) {
        let max = self.matches.len().saturating_sub(1);
        self.selected = (self.selected + n).min(max);
    }

    fn build_panel(&self, content_height: usize, win_w: u16, win_h: u16) -> UiLine {
        let title = DiffPanelRow::new(vec![
            DiffPanelSpan::new(l("Select file", "选择文件"), DiffPanelTone::Brand),
            DiffPanelSpan::new(format!("  {}\u{258f}", self.query), DiffPanelTone::Muted),
        ]);

        let mut rows: Vec<DiffPanelRow> = Vec::new();
        if let Some(err) = &self.error {
            rows.push(DiffPanelRow::new(vec![DiffPanelSpan::new(
                err.clone(),
                DiffPanelTone::Warning,
            )]));
        } else if self.matches.is_empty() {
            let msg = if self.query.is_empty() {
                l("Type to search files", "输入以搜索文件")
            } else {
                l("No matching files", "无匹配文件")
            };
            rows.push(DiffPanelRow::new(vec![DiffPanelSpan::new(
                msg,
                DiffPanelTone::Muted,
            )]));
        } else {
            // Scroll the window so the selection stays visible.
            let start = if self.selected >= content_height {
                self.selected + 1 - content_height
            } else {
                0
            };
            for (i, entry) in self
                .matches
                .iter()
                .enumerate()
                .skip(start)
                .take(content_height)
            {
                let selected = i == self.selected;
                let tone = if selected {
                    DiffPanelTone::Highlight
                } else {
                    DiffPanelTone::Default
                };
                let marker = if selected { "› " } else { "  " };
                rows.push(DiffPanelRow::new(vec![
                    DiffPanelSpan::new(marker, tone),
                    DiffPanelSpan::new(entry.rel_path.clone(), tone),
                ]));
            }
        }

        UiLine::DiffPanel {
            title,
            rows,
            footer: l(
                "↑↓ select · Enter open · Esc cancel",
                "↑↓ 选择 · Enter 打开 · Esc 取消",
            )
            .to_string(),
            win_width: win_w,
            win_height: win_h,
        }
    }
}

pub struct FileViewer {
    /// `Some` when opened via `/view` (no arg). Kept alongside `content` so
    /// Esc from the content panel can return to the picker.
    picker: Option<Picker>,
    /// `Some` while a file is displayed. Takes render/key precedence over the
    /// picker.
    content: Option<Content>,
}

impl FileViewer {
    /// `/view <path>`: open the content panel directly (no picker to return to).
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            picker: None,
            content: Some(load_content(path)?),
        })
    }

    /// `/view` (no arg): open the files-only fuzzy picker.
    pub fn open_picker(working_dir: PathBuf) -> Self {
        let index = FileIndex::new(working_dir.clone());
        let mut picker = Picker {
            index,
            working_dir,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
            error: None,
        };
        picker.refresh();
        Self {
            picker: Some(picker),
            content: None,
        }
    }

    fn content_scroll_up(&mut self, n: usize) {
        if let Some(c) = &mut self.content {
            c.scroll = c.scroll.saturating_sub(n);
        }
    }

    fn content_scroll_down(&mut self, n: usize) {
        if let Some(c) = &mut self.content {
            let max = c.lines.len().saturating_sub(1);
            c.scroll = (c.scroll + n).min(max);
        }
    }

    fn handle_content_key(
        &mut self,
        code: KeyCode,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        let (_, _, page) = geometry(true);
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.content_scroll_up(1),
            KeyCode::Down | KeyCode::Char('j') => self.content_scroll_down(1),
            KeyCode::PageUp => self.content_scroll_up(page),
            KeyCode::PageDown => self.content_scroll_down(page),
            KeyCode::Home | KeyCode::Char('g') => {
                if let Some(c) = &mut self.content {
                    c.scroll = 0;
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if let Some(c) = &mut self.content {
                    c.scroll = c.lines.len().saturating_sub(1);
                }
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                if self.picker.is_some() {
                    // Came from the picker — back out to it.
                    self.content = None;
                    self.draw(buf, state, ctx, renderer);
                    return Ok(ModalAction::Continue);
                }
                renderer.render(UiLine::ModalOverlayClear);
                renderer.flush();
                return Ok(ModalAction::Close);
            }
            _ => return Ok(ModalAction::Continue),
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn handle_picker_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        let (_, _, page) = geometry(false);
        // Enter is handled specially (mutates `self.content`), so keep the
        // picker borrow scoped to the navigation/edit keys only.
        if let KeyCode::Enter = code {
            let chosen = self.picker.as_ref().and_then(|p| {
                p.matches
                    .get(p.selected)
                    .map(|e| p.working_dir.join(&e.rel_path))
            });
            if let Some(path) = chosen {
                match load_content(&path) {
                    Ok(content) => self.content = Some(content),
                    Err(e) => {
                        if let Some(p) = &mut self.picker {
                            p.error = Some(format!("{e}"));
                        }
                    }
                }
            }
            self.draw(buf, state, ctx, renderer);
            return Ok(ModalAction::Continue);
        }

        let Some(picker) = &mut self.picker else {
            return Ok(ModalAction::Close);
        };
        match code {
            KeyCode::Up => picker.move_up(1),
            KeyCode::Down => picker.move_down(1),
            KeyCode::PageUp => picker.move_up(page),
            KeyCode::PageDown => picker.move_down(page),
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
                picker.error = None;
                picker.refresh();
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                picker.query.push(c);
                picker.selected = 0;
                picker.error = None;
                picker.refresh();
            }
            KeyCode::Esc => {
                renderer.render(UiLine::ModalOverlayClear);
                renderer.flush();
                return Ok(ModalAction::Close);
            }
            _ => return Ok(ModalAction::Continue),
        }
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }
}

/// Read + decode + cap a file into displayable lines. Shared by the direct
/// `/view <path>` open and the picker's Enter.
fn load_content(path: &Path) -> Result<Content> {
    // Reject anything that isn't a regular file *before* reading a byte. A FIFO
    // or char device (e.g. /dev/zero) would otherwise make the blocking read
    // below hang or stream forever and freeze the event loop.
    let meta = std::fs::metadata(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("Not a regular file: {}", path.display());
    }

    use std::io::Read;
    let mut sample = Vec::new();
    std::fs::File::open(path)
        .with_context(|| format!("Failed to read {}", path.display()))?
        .take(MAX_READ_BYTES)
        .read_to_end(&mut sample)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let byte_truncated = meta.len() > MAX_READ_BYTES;

    // Binary sniff: scan the first 8 KB for NUL bytes.
    let sample_len = sample.len().min(8192);
    let nul_count = sample[..sample_len].iter().filter(|&&b| b == 0).count();
    if nul_count > 0 {
        anyhow::bail!("File appears to be binary (contains NUL bytes)");
    }

    // Decode as UTF-8. When we stopped at the byte cap the cut may have split a
    // multi-byte char; tolerate only that trailing incomplete sequence.
    let text = match std::str::from_utf8(&sample) {
        Ok(s) => s.to_string(),
        Err(e) if byte_truncated && e.error_len().is_none() => {
            std::str::from_utf8(&sample[..e.valid_up_to()])
                .unwrap()
                .to_string()
        }
        Err(_) => anyhow::bail!("File is not valid UTF-8"),
    };

    // Split into lines, truncate long ones, cap total count.
    let mut lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.chars().count() > MAX_LINE_LEN {
                let mut s: String = line.chars().take(MAX_LINE_LEN).collect();
                s.push_str(" …");
                s
            } else {
                line.to_string()
            }
        })
        .collect();
    let total_lines = lines.len();
    let line_capped = lines.len() > MAX_VIEW_LINES;
    if line_capped {
        lines.truncate(MAX_VIEW_LINES);
    }

    Ok(Content {
        path: path.to_path_buf(),
        lines,
        scroll: 0,
        total_lines,
        truncated: byte_truncated || line_capped,
    })
}

/// Build the bottom-anchored content panel for a loaded file.
fn build_content_panel(
    c: &Content,
    has_picker: bool,
    content_height: usize,
    win_w: u16,
    win_h: u16,
) -> UiLine {
    let shown = c.lines.len();
    let first = if shown == 0 { 0 } else { c.scroll + 1 };
    let last = (c.scroll + content_height).min(shown);

    let mut title_spans = vec![
        DiffPanelSpan::new(
            crate::platform::collapse_home(&c.path.display().to_string()),
            DiffPanelTone::Brand,
        ),
        DiffPanelSpan::new(
            format!(" · {first}-{last}/{}", c.total_lines),
            DiffPanelTone::Muted,
        ),
    ];
    if c.truncated {
        title_spans.push(DiffPanelSpan::new(
            l(" · truncated", " · 已截断"),
            DiffPanelTone::Warning,
        ));
    }

    // Gutter wide enough for the largest line number (min 2 cols).
    let gutter = c.total_lines.to_string().len().max(2);
    let mut rows: Vec<DiffPanelRow> = Vec::new();
    for (idx, line) in c.lines.iter().enumerate().skip(c.scroll).take(content_height) {
        rows.push(DiffPanelRow::new(vec![
            DiffPanelSpan::new(format!("{:>gutter$} ", idx + 1), DiffPanelTone::Muted),
            DiffPanelSpan::new(line.clone(), DiffPanelTone::Default),
        ]));
    }

    let footer = if has_picker {
        l("↑↓/PgUp scroll · Esc back", "↑↓/PgUp 滚动 · Esc 返回")
    } else {
        l("↑↓/PgUp scroll · Esc close", "↑↓/PgUp 滚动 · Esc 关闭")
    }
    .to_string();

    UiLine::DiffPanel {
        title: DiffPanelRow::new(title_spans),
        rows,
        footer,
        win_width: win_w,
        win_height: win_h,
    }
}

impl Modal for FileViewer {
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        if self.content.is_some() {
            self.handle_content_key(code, buf, state, ctx, renderer)
        } else if self.picker.is_some() {
            self.handle_picker_key(code, mods, buf, state, ctx, renderer)
        } else {
            Ok(ModalAction::Close)
        }
    }

    fn draw(&self, _buf: &Buffer, _state: &UiState, _ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        // Content view uses the taller (~75%) detail geometry; the picker uses
        // the shorter (~45%) list geometry — the same split as `/diff`.
        let (win_w, win_h, content_height) = geometry(self.content.is_some());
        let line = if let Some(c) = &self.content {
            build_content_panel(c, self.picker.is_some(), content_height, win_w, win_h)
        } else if let Some(p) = &self.picker {
            p.build_panel(content_height, win_w, win_h)
        } else {
            return;
        };
        renderer.render(line);
        renderer.flush();
    }

    fn handle_paste(
        &mut self,
        _text: &str,
        _buf: &mut Buffer,
        _state: &mut UiState,
        _ctx: &mut LoopCtx,
        _renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // File viewer doesn't accept paste.
        Ok(ModalAction::Continue)
    }

    /// Like `DiffViewer`, the panel replaces the input box, so it must own every
    /// keystroke — otherwise (e.g. a background `/loop` turn flips the phase to
    /// Streaming) typed characters leak into the input buffer and pop a slash
    /// menu behind the panel.
    fn captures_all_keys(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn content_of(v: &FileViewer) -> &Content {
        v.content.as_ref().expect("content view active")
    }

    #[test]
    fn geometry_matches_diff_list_detail_split() {
        // The `/view` panel mirrors `/diff`: a fixed-height inline panel (never a
        // full-screen takeover), with the picker on the shorter file-list height
        // and file content on the taller detail height. Assert the relationship
        // rather than exact rows so it holds at any terminal size.
        let (_, picker_h, picker_body) = geometry(false);
        let (_, content_h, content_body) = geometry(true);
        let (_, screen_h) = crossterm::terminal::size().unwrap_or((80, 24));
        // Universal invariant: detail is never shorter than the list. (On a tiny
        // terminal both collapse to the screen height, so this is `>=`, not `>`.)
        assert!(
            content_h >= picker_h,
            "content panel ({content_h}) must be at least as tall as the picker ({picker_h})"
        );
        // On any realistic terminal the detail view is strictly taller.
        if screen_h >= 20 {
            assert!(
                content_h > picker_h,
                "at a normal size the detail panel must be strictly taller than the picker"
            );
        }
        assert!(
            content_h <= screen_h && picker_h <= screen_h,
            "neither panel may exceed the screen ({screen_h})"
        );
        // Four chrome rows (top rule, title, blank spacer, hint) are reserved.
        assert_eq!(picker_body, (picker_h as usize).saturating_sub(4).max(1));
        assert_eq!(content_body, (content_h as usize).saturating_sub(4).max(1));
    }

    #[test]
    fn open_normal_text_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "line1").unwrap();
        writeln!(tmp, "line2").unwrap();
        writeln!(tmp, "line3").unwrap();
        let viewer = FileViewer::open(tmp.path()).unwrap();
        let c = content_of(&viewer);
        assert_eq!(c.lines, vec!["line1", "line2", "line3"]);
        assert_eq!(c.total_lines, 3);
        assert!(!c.truncated);
        assert_eq!(c.scroll, 0);
        assert!(viewer.picker.is_none());
    }

    #[test]
    fn open_rejects_binary_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&[0u8, 1, 2, 3]).unwrap();
        assert!(FileViewer::open(tmp.path()).is_err());
    }

    #[test]
    fn open_rejects_invalid_utf8() {
        // Invalid UTF-8 with NO NUL byte → passes the binary sniff, fails decode.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&[0xff, 0xfe, b'h', b'i']).unwrap();
        assert!(FileViewer::open(tmp.path()).is_err());
    }

    #[test]
    fn open_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(FileViewer::open(dir.path()).is_err());
    }

    #[test]
    fn open_rejects_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.txt");
        assert!(FileViewer::open(&missing).is_err());
    }

    #[test]
    fn scroll_down_clamps_at_last_line() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "a").unwrap();
        writeln!(tmp, "b").unwrap();
        let mut viewer = FileViewer::open(tmp.path()).unwrap();
        viewer.content_scroll_down(100);
        assert_eq!(content_of(&viewer).scroll, 1); // len 2 → max index 1
        viewer.content_scroll_up(100);
        assert_eq!(content_of(&viewer).scroll, 0);
    }

    #[test]
    fn picker_lists_files_only_not_directories() {
        // Directories in the index must never appear as pickable rows.
        let root = PathBuf::from("/proj");
        let index = FileIndex::from_entries(
            root.clone(),
            vec![
                Entry {
                    rel_path: "src".into(),
                    is_dir: true,
                    depth: 1,
                },
                Entry {
                    rel_path: "src/main.rs".into(),
                    is_dir: false,
                    depth: 2,
                },
                Entry {
                    rel_path: "README.md".into(),
                    is_dir: false,
                    depth: 1,
                },
            ],
        );
        let mut picker = Picker {
            index,
            working_dir: root,
            query: "rs".into(),
            matches: Vec::new(),
            selected: 0,
            error: None,
        };
        picker.refresh();
        assert!(
            picker.matches.iter().all(|e| !e.is_dir),
            "picker must drop directories: {:?}",
            picker.matches
        );
        assert!(
            picker.matches.iter().any(|e| e.rel_path == "src/main.rs"),
            "matching file must be present: {:?}",
            picker.matches
        );
    }

    #[test]
    fn picker_query_filters_matches() {
        let root = PathBuf::from("/proj");
        let index = FileIndex::from_entries(
            root.clone(),
            vec![
                Entry {
                    rel_path: "alpha.rs".into(),
                    is_dir: false,
                    depth: 1,
                },
                Entry {
                    rel_path: "beta.rs".into(),
                    is_dir: false,
                    depth: 1,
                },
            ],
        );
        let mut picker = Picker {
            index,
            working_dir: root,
            query: "alpha".into(),
            matches: Vec::new(),
            selected: 0,
            error: None,
        };
        picker.refresh();
        assert_eq!(picker.matches.len(), 1);
        assert_eq!(picker.matches[0].rel_path, "alpha.rs");
    }
}
