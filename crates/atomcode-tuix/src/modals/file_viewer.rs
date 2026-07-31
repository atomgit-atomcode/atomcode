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
//                 listed (you can't `/view` a directory). Like the `/diff` file
//                 list it fits its content and shows at most `MAX_VISIBLE_FILES`
//                 rows, scrolling the window for larger result sets. Typing a
//                 path-like query (`/…`, `~/…`, `./…`) that points to a real
//                 file surfaces an "open external file" row, so files OUTSIDE
//                 the project are reachable straight from the picker.
//   - `Content` : the selected file (or `/view <path>`) rendered with line
//                 numbers in a taller panel (~75%, matching the `/diff` detail
//                 view) so a long file has room to scroll. ↑↓/PgUp/Home/End
//                 scroll; Esc backs out to the picker (or closes for a direct
//                 `/view <path>`).
//
// `/view <path>` also accepts absolute, `~`-prefixed, and project-relative
// paths (resolved by the caller in `commands.rs`), so files OUTSIDE the project
// open fine that way too.

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
/// The picker shows at most this many file rows at once (mirrors
/// `DiffViewer::MAX_VISIBLE_FILES`); larger result sets scroll the window and a
/// "more files" indicator appears so the panel stays compact.
const MAX_VISIBLE_FILES: usize = 5;

/// Locale-picked literal (matches `DiffViewer`'s `l` helper).
fn l(en: &'static str, zh: &'static str) -> &'static str {
    match crate::i18n::current_locale() {
        crate::i18n::Locale::ZhCn => zh,
        _ => en,
    }
}

/// Fixed geometry for the file-content view: `(screen_w, panel_height,
/// content_height)`. A FIXED-height panel drawn inline where the input box sits
/// (covering it), NOT a full-screen takeover, using the ~75% height matching
/// `DiffViewer`'s file-detail view so a long file has room to scroll. The picker
/// instead fits its content (see `Picker::build_panel`). `content_height` is the
/// body area — `panel_height` minus four chrome rows (top rule, title, blank
/// spacer, hint).
fn geometry() -> (u16, u16, usize) {
    let (screen_w, screen_h) = crossterm::terminal::size().unwrap_or((80, 24));
    let h = screen_h as usize;
    // `.max(lo).min(h)` rather than `clamp(lo, h)` — the latter PANICS when the
    // terminal is shorter than `lo` (min > max).
    let panel_height = (h * 3 / 4).max(10).min(h);
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
    /// When the query is a path-like string (`/…`, `~/…`, `./…`) resolving to a
    /// real file OUTSIDE (or inside) the project, the resolved path is stored
    /// here and offered as a synthetic top row so external files are openable
    /// straight from the picker. `None` for ordinary fuzzy queries.
    external: Option<PathBuf>,
    selected: usize,
    /// Transient error from a failed open (e.g. binary file), shown in-panel.
    error: Option<String>,
}

impl Picker {
    /// Re-run the index filter for the current query, dropping directories, and
    /// recompute the optional external-file row.
    fn refresh(&mut self) {
        self.matches = self
            .index
            .filter("", &self.query)
            .into_iter()
            .filter(|e| !e.is_dir)
            .collect();
        self.external = resolve_external(&self.query, &self.working_dir);
        let len = self.len();
        if self.selected >= len {
            self.selected = len.saturating_sub(1);
        }
    }

    /// Total selectable rows: the external-file row (if any) plus the matches.
    fn len(&self) -> usize {
        self.external.is_some() as usize + self.matches.len()
    }

    /// Resolve the current selection to an on-disk path. The external row (when
    /// present) sits at index 0; project matches follow.
    fn selected_path(&self) -> Option<PathBuf> {
        match &self.external {
            Some(external) => {
                if self.selected == 0 {
                    Some(external.clone())
                } else {
                    self.matches
                        .get(self.selected - 1)
                        .map(|e| self.working_dir.join(&e.rel_path))
                }
            }
            None => self
                .matches
                .get(self.selected)
                .map(|e| self.working_dir.join(&e.rel_path)),
        }
    }

    fn move_up(&mut self, n: usize) {
        self.selected = self.selected.saturating_sub(n);
    }

    fn move_down(&mut self, n: usize) {
        let max = self.len().saturating_sub(1);
        self.selected = (self.selected + n).min(max);
    }

    /// Display label + special-row flag for combined selection index `i`
    /// (external row at 0 when present, then project matches).
    fn row_label(&self, i: usize) -> (String, bool) {
        if let Some(external) = &self.external {
            if i == 0 {
                let shown = crate::platform::collapse_home(&external.display().to_string());
                return (
                    format!("{}{shown}", l("open external: ", "打开外部文件: ")),
                    true,
                );
            }
            return (self.matches[i - 1].rel_path.clone(), false);
        }
        (self.matches[i].rel_path.clone(), false)
    }

    /// Build the picker panel. Like `/diff`, it fits its content (no fixed
    /// slab): at most `MAX_VISIBLE_FILES` rows show at once, the window scrolls
    /// to keep the selection visible, and a "more files" row appears when the
    /// result set is larger. `screen_h` only bounds the panel on tiny terminals.
    fn build_panel(&self, screen_w: u16, screen_h: u16) -> UiLine {
        let title = DiffPanelRow::new(vec![
            DiffPanelSpan::new(l("Select file", "选择文件"), DiffPanelTone::Brand),
            DiffPanelSpan::new(format!("  {}\u{258f}", self.query), DiffPanelTone::Muted),
        ]);

        let mut rows: Vec<DiffPanelRow> = Vec::new();
        let total = self.len();
        if let Some(err) = &self.error {
            rows.push(DiffPanelRow::new(vec![DiffPanelSpan::new(
                err.clone(),
                DiffPanelTone::Warning,
            )]));
        } else if total == 0 {
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
            // Cap the window at MAX_VISIBLE_FILES, bounded by what the screen can
            // hold once chrome (4 rows) + a possible "more files" row are taken.
            let max_files = MAX_VISIBLE_FILES
                .min((screen_h as usize).saturating_sub(5))
                .max(1);
            let window = max_files.min(total).max(1);
            let start = self
                .selected
                .saturating_add(1)
                .saturating_sub(window)
                .min(total.saturating_sub(window));
            for i in start..(start + window) {
                let selected = i == self.selected;
                let tone = if selected {
                    DiffPanelTone::Highlight
                } else {
                    DiffPanelTone::Default
                };
                let marker = if selected { "› " } else { "  " };
                let (label, _is_external) = self.row_label(i);
                rows.push(DiffPanelRow::new(vec![
                    DiffPanelSpan::new(marker, tone),
                    DiffPanelSpan::new(label, tone),
                ]));
            }
            if total > window {
                rows.push(super::more_files_row(start, total - (start + window)));
            }
        }

        let win_height = ((rows.len() + 4).min(screen_h as usize)).max(1) as u16;
        UiLine::DiffPanel {
            title,
            rows,
            footer: l(
                "↑↓ select · Enter open · type /~ path for external file · Esc cancel",
                "↑↓ 选择 · Enter 打开 · 输入 /~ 路径打开外部文件 · Esc 取消",
            )
            .to_string(),
            win_width: screen_w,
            win_height,
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
            external: None,
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
        let (_, _, page) = geometry();
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
        // PgUp/PgDn jump a windowful; the picker shows at most MAX_VISIBLE_FILES.
        let page = MAX_VISIBLE_FILES;
        // Enter is handled specially (mutates `self.content`), so keep the
        // picker borrow scoped to the navigation/edit keys only.
        if let KeyCode::Enter = code {
            let chosen = self.picker.as_ref().and_then(|p| p.selected_path());
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
    let meta =
        std::fs::metadata(path).with_context(|| format!("Failed to read {}", path.display()))?;
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
    for (idx, line) in c
        .lines
        .iter()
        .enumerate()
        .skip(c.scroll)
        .take(content_height)
    {
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

/// Resolve a path-like picker query to an existing regular file, so files the
/// fuzzy `FileIndex` can't reach (gitignored dirs like `dist/`, `target/`, or
/// anything OUTSIDE the project) are openable straight from the picker.
/// Returns `None` for ordinary fuzzy terms — a query is only probed on disk
/// when it clearly denotes a path: an absolute / `~` / `./…` / `../…` form, OR a
/// bare relative path containing a separator (`dist/bundle.js`). A separatorless
/// term (`main`) is never statted. Relative forms resolve against `working_dir`,
/// matching the reach of `commands.rs::resolve_view_path`.
fn resolve_external(query: &str, working_dir: &Path) -> Option<PathBuf> {
    let q = query.trim();
    if q.is_empty() {
        return None;
    }
    let has_separator = q.contains('/') || q.contains(std::path::MAIN_SEPARATOR);
    let expanded: PathBuf = if q == "~" {
        crate::platform::home_dir()?
    } else if let Some(rest) = q.strip_prefix("~/") {
        crate::platform::home_dir()?.join(rest)
    } else if q.starts_with('/') {
        PathBuf::from(q)
    } else if q == "." || q == ".." || q.starts_with("./") || q.starts_with("../") {
        working_dir.join(q)
    } else if has_separator {
        // Bare relative path with a separator (`dist/bundle.js`): the gitignored
        // files the FileIndex omits are reachable this way, matching what
        // `/view <path>` already accepts.
        working_dir.join(q)
    } else {
        return None;
    };
    match std::fs::metadata(&expanded) {
        Ok(meta) if meta.is_file() => Some(expanded),
        _ => None,
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
        let line = if let Some(c) = &self.content {
            // Content view uses the taller (~75%) fixed detail geometry.
            let (win_w, win_h, content_height) = geometry();
            build_content_panel(c, self.picker.is_some(), content_height, win_w, win_h)
        } else if let Some(p) = &self.picker {
            // Picker fits its content (max MAX_VISIBLE_FILES rows), like `/diff`.
            let (screen_w, screen_h) = crossterm::terminal::size().unwrap_or((80, 24));
            p.build_panel(screen_w, screen_h)
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
    fn content_geometry_is_a_bounded_fixed_panel() {
        // The content view is a fixed ~75% panel, never a full-screen takeover,
        // reserving four chrome rows (top rule, title, blank spacer, hint).
        let (_, panel_h, content_body) = geometry();
        let (_, screen_h) = crossterm::terminal::size().unwrap_or((80, 24));
        assert!(panel_h >= 1 && panel_h <= screen_h.max(1));
        assert_eq!(content_body, (panel_h as usize).saturating_sub(4).max(1));
    }

    fn picker_from(root: &Path, entries: Vec<Entry>, query: &str) -> Picker {
        let index = FileIndex::from_entries(root.to_path_buf(), entries);
        let mut picker = Picker {
            index,
            working_dir: root.to_path_buf(),
            query: query.to_string(),
            matches: Vec::new(),
            external: None,
            selected: 0,
            error: None,
        };
        picker.refresh();
        picker
    }

    #[test]
    fn external_row_surfaces_for_a_real_path_query() {
        // A path-like query pointing at a real file becomes the index-0 row and
        // Enter would open exactly that path — even with zero project matches.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let picker = picker_from(
            &PathBuf::from("/proj"),
            Vec::new(),
            &tmp.path().display().to_string(),
        );
        assert_eq!(picker.external.as_deref(), Some(tmp.path()));
        assert_eq!(picker.len(), 1);
        assert_eq!(picker.selected_path().as_deref(), Some(tmp.path()));
    }

    #[test]
    fn bare_relative_path_with_separator_reaches_gitignored_files() {
        // A gitignored file (omitted from the FileIndex) is unreachable by fuzzy
        // match but must surface as the external row when typed as a relative
        // path with a separator — matching what `/view dist/x` already opens.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("dist")).unwrap();
        let file = dir.path().join("dist/bundle.js");
        std::fs::write(&file, b"//").unwrap();
        // No FileIndex entry for it (simulating gitignore omission).
        let picker = picker_from(dir.path(), Vec::new(), "dist/bundle.js");
        assert_eq!(picker.external.as_deref(), Some(file.as_path()));
        assert_eq!(picker.selected_path().as_deref(), Some(file.as_path()));
    }

    #[test]
    fn ordinary_query_has_no_external_row() {
        // A plain fuzzy query (not path-like) must never trigger a filesystem
        // probe / external row.
        let picker = picker_from(
            &PathBuf::from("/proj"),
            vec![Entry {
                rel_path: "main.rs".into(),
                is_dir: false,
                depth: 1,
            }],
            "main",
        );
        assert!(picker.external.is_none());
        assert_eq!(picker.len(), 1);
    }

    #[test]
    fn nonexistent_path_query_has_no_external_row() {
        let picker = picker_from(&PathBuf::from("/proj"), Vec::new(), "/no/such/file/xyzzy");
        assert!(picker.external.is_none());
        assert_eq!(picker.len(), 0);
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
            external: None,
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
            external: None,
            selected: 0,
            error: None,
        };
        picker.refresh();
        assert_eq!(picker.matches.len(), 1);
        assert_eq!(picker.matches[0].rel_path, "alpha.rs");
    }
}
