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
        // 1. Binary sniff: read first 8 KB and look for NUL bytes.
        let sample =
            std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))?;
        let sample_len = sample.len().min(8192);
        let nul_count = sample[..sample_len].iter().filter(|&&b| b == 0).count();
        if nul_count > 0 {
            anyhow::bail!("File appears to be binary (contains NUL bytes)");
        }

        // 2. Decode text as UTF-8.
        let text =
            String::from_utf8(sample).map_err(|_| anyhow::anyhow!("File is not valid UTF-8"))?;

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
        let truncated = if content.len() > MAX_VIEW_LINES {
            content.truncate(MAX_VIEW_LINES);
            true
        } else {
            false
        };

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
        tmp.write_all(&[0xFF, 0xFE, 0x00, 0x48]).unwrap();
        let err = FileViewer::open(tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("UTF-8"),
            "expected UTF-8 error, got: {err}"
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
        assert_eq!(viewer.content[0].len(), MAX_LINE_LEN + 2); // +2 for " …"
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
