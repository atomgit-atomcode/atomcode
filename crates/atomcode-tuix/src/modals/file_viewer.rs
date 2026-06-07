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
        let sample = std::fs::read(path).with_context(|| {
            format!("Failed to read {}", path.display())
        })?;
        let sample_len = sample.len().min(8192);
        let nul_count = sample[..sample_len].iter().filter(|&&b| b == 0).count();
        if nul_count > 0 {
            anyhow::bail!("File appears to be binary (contains NUL bytes)");
        }

        // 2. Decode text as UTF-8.
        let text = String::from_utf8(sample)
            .map_err(|_| anyhow::anyhow!("File is not valid UTF-8"))?;

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
        let name = self.path.file_name()
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
        let win_w = ((screen_w as usize) * 4 / 5).max(40).min(screen_w as usize - 4) as u16;
        let win_h = ((screen_h as usize) * 4 / 5).max(10).min(screen_h as usize - 4) as u16;
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
