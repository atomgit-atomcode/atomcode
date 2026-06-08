// crates/atomcode-tuix/src/modals/file_viewer.rs
//
// `/view` modal — overlay file content viewer.
//
// Opens a centred floating window on top of the chat UI showing the
// contents of a single file.  Up/Down/PageUp/PageDown scroll;
// Esc/q close.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{Buffer, LoopCtx};
use crate::render::{Renderer, UiLine};
use crate::state::UiState;

/// Max lines displayed in one overlay (to keep memory reasonable).
const MAX_VIEW_LINES: usize = 1000;
/// Truncate individual lines to this display width.
const MAX_LINE_LEN: usize = 2000;
/// Hard cap on bytes pulled from disk. Sized to comfortably hold
/// `MAX_VIEW_LINES` × `MAX_LINE_LEN` of worst-case 4-byte UTF-8, so it
/// never clips a file the viewer would have shown in full, while bounding
/// the pathological (multi-GB / single-giant-line) case that would
/// otherwise blow up memory.
const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug)]
pub struct FileViewer {
    pub path: PathBuf,
    pub content: Vec<String>,
    pub scroll: usize,
    pub total_lines: usize,
    pub truncated: bool,
}

impl FileViewer {
    pub fn open(path: &Path) -> Result<Self> {
        // Reject anything that isn't a regular file *before* reading a
        // byte. A FIFO or char device (e.g. /dev/zero) would otherwise
        // make the blocking read below hang or stream forever and freeze
        // the whole event loop, since `open` runs synchronously on it.
        let meta = std::fs::metadata(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        if !meta.is_file() {
            anyhow::bail!("Not a regular file: {}", path.display());
        }

        // Read at most MAX_READ_BYTES. The viewer only ever shows the
        // first MAX_VIEW_LINES lines, so a multi-GB (or single-giant-line)
        // file must not be pulled fully into memory just to discard the
        // tail. `take` also bounds the worst case for any file type.
        use std::io::Read;
        let mut sample = Vec::new();
        std::fs::File::open(path)
            .with_context(|| format!("Failed to read {}", path.display()))?
            .take(MAX_READ_BYTES)
            .read_to_end(&mut sample)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let byte_truncated = meta.len() > MAX_READ_BYTES;

        // 1. Binary sniff: scan the first 8 KB for NUL bytes.
        let sample_len = sample.len().min(8192);
        let nul_count = sample[..sample_len].iter().filter(|&&b| b == 0).count();
        if nul_count > 0 {
            anyhow::bail!("File appears to be binary (contains NUL bytes)");
        }

        // 2. Decode as UTF-8. When we stopped at the byte cap the cut may
        // have split a multi-byte char; tolerate only that trailing
        // incomplete sequence (`error_len() == None`), never a genuine
        // invalid byte.
        let text = match std::str::from_utf8(&sample) {
            Ok(s) => s.to_string(),
            Err(e) if byte_truncated && e.error_len().is_none() => {
                // `valid_up_to()` is guaranteed-valid UTF-8.
                std::str::from_utf8(&sample[..e.valid_up_to()])
                    .unwrap()
                    .to_string()
            }
            Err(_) => anyhow::bail!("File is not valid UTF-8"),
        };

        // 3. Split into lines, truncate long ones, cap total count.
        let mut content: Vec<String> = text
            .lines()
            .map(|l| {
                if l.chars().count() > MAX_LINE_LEN {
                    let mut s: String = l.chars().take(MAX_LINE_LEN).collect();
                    s.push_str(" …");
                    s
                } else {
                    l.to_string()
                }
            })
            .collect();

        let total_lines = content.len();
        let line_capped = content.len() > MAX_VIEW_LINES;
        if line_capped {
            content.truncate(MAX_VIEW_LINES);
        }
        let truncated = byte_truncated || line_capped;

        Ok(Self {
            path: path.to_path_buf(),
            content,
            scroll: 0,
            total_lines,
            truncated,
        })
    }

    fn scroll_up(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    fn scroll_down(&mut self, n: usize) {
        let max = self.content.len().saturating_sub(1);
        self.scroll = (self.scroll + n).min(max);
    }

    fn visible_lines(&self, height: usize) -> Vec<String> {
        self.content
            .iter()
            .skip(self.scroll)
            .take(height)
            .cloned()
            .collect()
    }

    fn build_title(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());
        if self.truncated {
            format!("{} (truncated)", name)
        } else {
            name
        }
    }
}

impl Modal for FileViewer {
    fn handle_key(
        &mut self,
        code: KeyCode,
        _mods: KeyModifiers,
        _buf: &mut Buffer,
        _state: &mut UiState,
        _ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // Determine content height for page-scroll: screen height * 0.8 - 5 (borders + chrome).
        let (_, screen_h) = crossterm::terminal::size().unwrap_or((80, 24));
        let win_h = ((screen_h as usize) * 4 / 5).max(6);
        let page = (win_h as usize).saturating_sub(5).max(1);

        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_up(1);
                self.draw(_buf, _state, _ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_down(1);
                self.draw(_buf, _state, _ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::PageUp => {
                self.scroll_up(page);
                self.draw(_buf, _state, _ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::PageDown => {
                self.scroll_down(page);
                self.draw(_buf, _state, _ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.scroll = 0;
                self.draw(_buf, _state, _ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.scroll = self.content.len().saturating_sub(1);
                self.draw(_buf, _state, _ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                renderer.render(UiLine::ModalOverlayClear);
                renderer.flush();
                Ok(ModalAction::Close)
            }
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, _buf: &Buffer, _state: &UiState, _ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let (screen_w, screen_h) = crossterm::terminal::size().unwrap_or((80, 24));
        let win_w = ((screen_w as usize) * 4 / 5)
            .max(40)
            .min(screen_w as usize - 4) as u16;
        let win_h = ((screen_h as usize) * 4 / 5)
            .max(10)
            .min(screen_h as usize - 4) as u16;
        let content_height = (win_h as usize).saturating_sub(5).max(1);

        let lines = self.visible_lines(content_height);

        renderer.render(UiLine::ModalOverlay {
            title: self.build_title(),
            lines,
            scroll: self.scroll,
            total: self.total_lines,
            win_width: win_w,
            win_height: win_h,
        });
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn open_normal_text_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "line1").unwrap();
        writeln!(tmp, "line2").unwrap();
        writeln!(tmp, "line3").unwrap();
        let viewer = FileViewer::open(tmp.path()).unwrap();
        assert_eq!(viewer.content, vec!["line1", "line2", "line3"]);
        assert_eq!(viewer.total_lines, 3);
        assert!(!viewer.truncated);
        assert_eq!(viewer.scroll, 0);
    }

    #[test]
    fn open_empty_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let viewer = FileViewer::open(tmp.path()).unwrap();
        assert!(viewer.content.is_empty());
        assert_eq!(viewer.total_lines, 0);
        assert!(!viewer.truncated);
    }

    #[test]
    fn open_binary_file_rejected() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&[0x48, 0x00, 0x65, 0x00]).unwrap();
        let err = FileViewer::open(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("binary"),
            "expected binary error, got: {err}"
        );
    }

    #[test]
    fn open_non_utf8_file_rejected() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        // Invalid UTF-8 with NO NUL byte, so the binary sniff passes it
        // through to the UTF-8 decode (0xFF is never valid in UTF-8).
        tmp.write_all(&[0xFF, 0xFE, 0x41, 0x42]).unwrap();
        let err = FileViewer::open(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("UTF-8"),
            "expected UTF-8 error, got: {err}"
        );
    }

    #[test]
    fn open_directory_rejected() {
        // A directory is not a regular file — must be rejected up front,
        // not read (the guard that also protects against FIFOs / devices).
        let dir = tempfile::tempdir().unwrap();
        let err = FileViewer::open(dir.path()).unwrap_err();
        assert!(
            err.to_string().contains("regular file"),
            "expected non-regular-file error, got: {err}"
        );
    }

    #[test]
    fn open_nonexistent_file() {
        let err = FileViewer::open(Path::new("/nonexistent/path")).unwrap_err();
        assert!(err.to_string().contains("Failed to read"));
    }

    #[test]
    fn long_lines_are_truncated() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        let long_line = "a".repeat(MAX_LINE_LEN + 500);
        writeln!(tmp, "{long_line}").unwrap();
        let viewer = FileViewer::open(tmp.path()).unwrap();
        // Count chars, not bytes: " …" is 2 chars but 4 bytes (U+2026 is
        // 3-byte UTF-8), so a byte-length assert is off by 2.
        assert_eq!(viewer.content[0].chars().count(), MAX_LINE_LEN + 2); // +2 for " …"
        assert!(viewer.content[0].ends_with(" …"));
    }

    #[test]
    fn many_lines_are_capped() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 0..MAX_VIEW_LINES + 200 {
            writeln!(tmp, "line{i}").unwrap();
        }
        let viewer = FileViewer::open(tmp.path()).unwrap();
        assert_eq!(viewer.content.len(), MAX_VIEW_LINES);
        assert!(viewer.truncated);
        assert_eq!(viewer.total_lines, MAX_VIEW_LINES + 200);
    }

    #[test]
    fn scroll_up_clamps_at_zero() {
        let mut viewer = FileViewer {
            path: PathBuf::new(),
            content: vec!["a".into(), "b".into(), "c".into()],
            scroll: 0,
            total_lines: 3,
            truncated: false,
        };
        viewer.scroll_up(1);
        assert_eq!(viewer.scroll, 0);
    }

    #[test]
    fn scroll_down_clamps_at_max() {
        let mut viewer = FileViewer {
            path: PathBuf::new(),
            content: vec!["a".into(), "b".into(), "c".into()],
            scroll: 2,
            total_lines: 3,
            truncated: false,
        };
        viewer.scroll_down(5);
        assert_eq!(viewer.scroll, 2);
    }

    #[test]
    fn scroll_up_and_down() {
        let mut viewer = FileViewer {
            path: PathBuf::new(),
            content: vec!["a".into(), "b".into(), "c".into()],
            scroll: 2,
            total_lines: 3,
            truncated: false,
        };
        viewer.scroll_up(1);
        assert_eq!(viewer.scroll, 1);
        viewer.scroll_up(1);
        assert_eq!(viewer.scroll, 0);
        viewer.scroll_down(1);
        assert_eq!(viewer.scroll, 1);
        viewer.scroll_down(1);
        assert_eq!(viewer.scroll, 2);
    }

    #[test]
    fn visible_lines_returns_correct_subset() {
        let viewer = FileViewer {
            path: PathBuf::new(),
            content: vec!["a".into(), "b".into(), "c".into(), "d".into()],
            scroll: 1,
            total_lines: 4,
            truncated: false,
        };
        let lines = viewer.visible_lines(2);
        assert_eq!(lines, vec!["b", "c"]);
    }

    #[test]
    fn visible_lines_scroll_beyond_content() {
        let viewer = FileViewer {
            path: PathBuf::new(),
            content: vec!["a".into()],
            scroll: 0,
            total_lines: 1,
            truncated: false,
        };
        let lines = viewer.visible_lines(5);
        assert_eq!(lines, vec!["a"]);
    }

    #[test]
    fn build_title_shows_filename() {
        let viewer = FileViewer {
            path: PathBuf::from("src/main.rs"),
            content: vec![],
            scroll: 0,
            total_lines: 0,
            truncated: false,
        };
        assert_eq!(viewer.build_title(), "main.rs");
    }

    #[test]
    fn build_title_appends_truncated() {
        let viewer = FileViewer {
            path: PathBuf::from("big.log"),
            content: vec![],
            scroll: 0,
            total_lines: 2000,
            truncated: true,
        };
        assert_eq!(viewer.build_title(), "big.log (truncated)");
    }
}
