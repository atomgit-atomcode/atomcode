use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use tokio::sync::mpsc::Sender;

use super::{Modal, ModalAction};
use crate::event_loop::{Buffer, LoopCtx};
use crate::git_diff::{
    capture_diff_snapshot, display_path, DiffBase, DiffContent, DiffFile, DiffFileStatus,
    DiffScope, DiffSnapshot,
};
use crate::i18n::{current_locale, Locale};
use crate::render::diff::{diff_gutter_width, diff_row_text};
use crate::render::{DiffKind, DiffPanelRow, DiffPanelSpan, DiffPanelTone, Renderer, UiLine};
use crate::state::UiState;

enum View {
    Loading,
    List { selected: usize },
    Detail { file: usize, scroll: usize },
    Error(String),
}

pub struct DiffViewer {
    receiver: Option<Receiver<Result<DiffSnapshot, String>>>,
    snapshot: Option<DiffSnapshot>,
    view: View,
}

impl DiffViewer {
    pub fn open(working_dir: PathBuf, wake_tx: Sender<()>) -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = capture_diff_snapshot(&working_dir).map_err(|error| {
                crate::i18n::t(crate::i18n::Msg::DiffFailed { error: &error }).into_owned()
            });
            if result_tx.send(result).is_ok() {
                let _ = wake_tx.try_send(());
            }
        });
        Self {
            receiver: Some(result_rx),
            snapshot: None,
            view: View::Loading,
        }
    }

    fn close(renderer: &mut dyn Renderer) -> ModalAction {
        renderer.render(UiLine::ModalOverlayClear);
        renderer.flush();
        ModalAction::Close
    }

    fn dimensions() -> (u16, u16, usize) {
        let (screen_w, screen_h) = crossterm::terminal::size().unwrap_or((80, 24));
        let max_w = screen_w.saturating_sub(4).max(20);
        let max_h = screen_h.saturating_sub(4).max(6);
        let win_w = (screen_w.saturating_mul(9) / 10).clamp(20, max_w);
        let win_h = (screen_h.saturating_mul(4) / 5).clamp(6, max_h);
        (win_w, win_h, win_h.saturating_sub(5).max(1) as usize)
    }

    fn list_rows(&self, selected: usize, height: usize, width: usize) -> Vec<DiffPanelRow> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        if snapshot.files.is_empty() {
            return vec![DiffPanelRow::new(vec![DiffPanelSpan::new(
                l("No uncommitted changes", "没有未提交变更"),
                DiffPanelTone::Muted,
            )])];
        }

        let mut rows = vec![
            DiffPanelRow::new(vec![DiffPanelSpan::new(
                match snapshot.base {
                    DiffBase::Head => l(
                        "Uncommitted changes  (git diff HEAD)",
                        "未提交变更  (git diff HEAD)",
                    ),
                    DiffBase::Unborn => l(
                        "Initial changes  (repository has no HEAD)",
                        "初始变更  (仓库还没有 HEAD)",
                    ),
                },
                DiffPanelTone::Brand,
            )
            .bold()]),
            DiffPanelRow::new(vec![
                DiffPanelSpan::new(
                    match current_locale() {
                        Locale::ZhCn => format!("{} 个文件有变更  ", snapshot.files_changed),
                        Locale::En => format!("{} files changed  ", snapshot.files_changed),
                    },
                    DiffPanelTone::Muted,
                ),
                DiffPanelSpan::new(format!("+{}", snapshot.additions), DiffPanelTone::Add),
                DiffPanelSpan::new("  ", DiffPanelTone::Default),
                DiffPanelSpan::new(format!("-{}", snapshot.deletions), DiffPanelTone::Remove),
            ]),
            DiffPanelRow::new(Vec::new()),
        ];
        if snapshot.truncated {
            rows.push(DiffPanelRow::new(vec![DiffPanelSpan::new(
                l(
                    "Showing a bounded snapshot; some files or lines were truncated",
                    "快照超过展示上限，部分文件或行已截断",
                ),
                DiffPanelTone::Warning,
            )]));
        }
        let available = height.saturating_sub(rows.len()).max(1);
        let start = selected
            .saturating_add(1)
            .saturating_sub(available)
            .min(snapshot.files.len().saturating_sub(available));
        for (index, file) in snapshot
            .files
            .iter()
            .enumerate()
            .skip(start)
            .take(available)
        {
            rows.push(file_list_row(file, index == selected, width));
        }
        rows
    }

    fn detail_rows(&self, file_index: usize) -> Vec<DiffPanelRow> {
        let Some(file) = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.files.get(file_index))
        else {
            return Vec::new();
        };
        let mut rows = vec![DiffPanelRow::new(file_summary_spans(file))];
        if let Some(old_path) = &file.old_path {
            rows.push(DiffPanelRow::new(vec![DiffPanelSpan::new(
                match current_locale() {
                    Locale::ZhCn => format!("重命名前：{}", display_path(old_path)),
                    Locale::En => format!("renamed from {}", display_path(old_path)),
                },
                DiffPanelTone::Muted,
            )]));
        }
        rows.push(DiffPanelRow::new(Vec::new()));

        match file.content {
            DiffContent::Binary => rows.push(notice_row(l(
                "Binary file; content diff is unavailable",
                "二进制文件，无法展示内容差异",
            ))),
            DiffContent::Untracked => rows.push(notice_row(l(
                "Untracked file; add it to the index to view a patch",
                "未跟踪文件；加入暂存区后可查看补丁",
            ))),
            DiffContent::Truncated if file.sections.is_empty() => rows.push(notice_row(l(
                "Patch exceeded the display limit",
                "补丁超过展示上限",
            ))),
            DiffContent::Text | DiffContent::Truncated => {
                for section in &file.sections {
                    if section.scope != DiffScope::Combined {
                        rows.push(DiffPanelRow::new(vec![DiffPanelSpan::new(
                            match section.scope {
                                DiffScope::Staged => l("Staged", "已暂存"),
                                DiffScope::Unstaged => l("Unstaged", "未暂存"),
                                DiffScope::Combined => "",
                            },
                            DiffPanelTone::Brand,
                        )
                        .bold()]));
                    }
                    let gutter = diff_gutter_width(&section.entries);
                    for entry in &section.entries {
                        rows.push(DiffPanelRow::new(vec![DiffPanelSpan::new(
                            diff_row_text(entry, gutter),
                            match entry.kind {
                                DiffKind::Add => DiffPanelTone::Add,
                                DiffKind::Del => DiffPanelTone::Remove,
                                DiffKind::Context => DiffPanelTone::Default,
                                DiffKind::Separator => DiffPanelTone::Muted,
                            },
                        )]));
                    }
                }
                if file.sections.is_empty() {
                    rows.push(notice_row(l(
                        "Metadata changed; no text hunks",
                        "文件元数据已变更，没有文本块",
                    )));
                }
                if file.truncated || file.content == DiffContent::Truncated {
                    rows.push(notice_row(l("… diff truncated", "… 差异已截断")));
                }
            }
        }
        rows
    }

    fn redraw(&self, renderer: &mut dyn Renderer) {
        let (win_width, win_height, content_height) = Self::dimensions();
        let (title, rows, footer) = match &self.view {
            View::Loading => (
                l("Diff", "差异").to_string(),
                vec![DiffPanelRow::new(vec![DiffPanelSpan::new(
                    l("Loading repository changes…", "正在读取仓库变更…"),
                    DiffPanelTone::Muted,
                )])],
                l("Esc/q close", "Esc/q 关闭").to_string(),
            ),
            View::Error(error) => (
                l("Diff", "差异").to_string(),
                vec![DiffPanelRow::new(vec![DiffPanelSpan::new(
                    error,
                    DiffPanelTone::Warning,
                )])],
                l("Esc/q close", "Esc/q 关闭").to_string(),
            ),
            View::List { selected } => {
                let title = self
                    .snapshot
                    .as_ref()
                    .map(|snapshot| {
                        format!(
                            "{} · {}",
                            l("Diff", "差异"),
                            display_path(&snapshot.repo_root)
                        )
                    })
                    .unwrap_or_else(|| l("Diff", "差异").to_string());
                (
                    title,
                    self.list_rows(*selected, content_height, win_width as usize),
                    l(
                        "↑/↓ select · Enter view · Esc/q close",
                        "↑/↓ 选择 · Enter 查看 · Esc/q 关闭",
                    )
                    .to_string(),
                )
            }
            View::Detail { file, scroll } => {
                let all_rows = self.detail_rows(*file);
                let visible = all_rows
                    .iter()
                    .skip(*scroll)
                    .take(content_height)
                    .cloned()
                    .collect();
                let title = self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.files.get(*file))
                    .map(|file| format!("{} · {}", l("Diff", "差异"), display_path(&file.path)))
                    .unwrap_or_else(|| l("Diff", "差异").to_string());
                (
                    title,
                    visible,
                    format!(
                        "{}  ({}/{})",
                        l(
                            "↑/↓ scroll · PgUp/PgDn · Esc back · q close",
                            "↑/↓ 滚动 · PgUp/PgDn · Esc 返回 · q 关闭",
                        ),
                        scroll.saturating_add(1).min(all_rows.len().max(1)),
                        all_rows.len().max(1)
                    ),
                )
            }
        };
        renderer.render(UiLine::DiffPanel {
            title,
            rows,
            footer,
            win_width,
            win_height,
        });
        renderer.flush();
    }
}

impl Modal for DiffViewer {
    fn handle_key(
        &mut self,
        code: KeyCode,
        _mods: KeyModifiers,
        _buf: &mut Buffer,
        _state: &mut UiState,
        _ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        if matches!(code, KeyCode::Char('q')) {
            return Ok(Self::close(renderer));
        }
        let (_, _, page) = Self::dimensions();
        let detail_total = match &self.view {
            View::Detail { file, .. } => self.detail_rows(*file).len(),
            _ => 0,
        };
        match &mut self.view {
            View::Loading | View::Error(_) => {
                if code == KeyCode::Esc {
                    return Ok(Self::close(renderer));
                }
            }
            View::List { selected } => {
                let len = self
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.files.len())
                    .unwrap_or(0);
                match code {
                    KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        *selected = selected.saturating_add(1).min(len.saturating_sub(1))
                    }
                    KeyCode::Home | KeyCode::Char('g') => *selected = 0,
                    KeyCode::End | KeyCode::Char('G') => *selected = len.saturating_sub(1),
                    KeyCode::Enter if len > 0 => {
                        self.view = View::Detail {
                            file: *selected,
                            scroll: 0,
                        }
                    }
                    KeyCode::Esc => return Ok(Self::close(renderer)),
                    _ => {}
                }
            }
            View::Detail { file, scroll } => {
                let max_scroll = detail_total.saturating_sub(page);
                match code {
                    KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll = scroll.saturating_add(1).min(max_scroll)
                    }
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(page),
                    KeyCode::PageDown => *scroll = scroll.saturating_add(page).min(max_scroll),
                    KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => *scroll = max_scroll,
                    KeyCode::Esc | KeyCode::Left => self.view = View::List { selected: *file },
                    _ => {}
                }
            }
        }
        self.redraw(renderer);
        Ok(ModalAction::Continue)
    }

    fn draw(&self, _buf: &Buffer, _state: &UiState, _ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        self.redraw(renderer);
    }

    fn handle_paste(
        &mut self,
        _text: &str,
        _buf: &mut Buffer,
        _state: &mut UiState,
        _ctx: &mut LoopCtx,
        _renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        Ok(ModalAction::Continue)
    }

    fn poll_background(&mut self) -> bool {
        let Some(receiver) = &self.receiver else {
            return false;
        };
        match receiver.try_recv() {
            Ok(Ok(snapshot)) => {
                self.snapshot = Some(snapshot);
                self.view = View::List { selected: 0 };
                self.receiver = None;
                true
            }
            Ok(Err(error)) => {
                self.view = View::Error(error);
                self.receiver = None;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.view = View::Error(
                    l("Diff worker stopped unexpectedly", "差异读取线程意外停止").to_string(),
                );
                self.receiver = None;
                true
            }
        }
    }
}

fn file_list_row(file: &DiffFile, selected: bool, width: usize) -> DiffPanelRow {
    let prefix = if selected { "› " } else { "  " };
    let status = status_label(file.status);
    let path = display_path(&file.path);
    let add = file
        .additions
        .map(|value| format!("+{value}"))
        .unwrap_or_default();
    let del = file
        .deletions
        .map(|value| format!("-{value}"))
        .unwrap_or_default();
    let occupied = prefix.chars().count()
        + status.chars().count()
        + crate::width::display_width(&path)
        + add.len()
        + del.len()
        + 6;
    let padding = " ".repeat(width.saturating_sub(occupied).max(2));
    DiffPanelRow::new(vec![
        DiffPanelSpan::new(prefix, DiffPanelTone::Brand).bold(),
        DiffPanelSpan::new(format!("{status} "), DiffPanelTone::Muted),
        DiffPanelSpan::new(
            path,
            if selected {
                DiffPanelTone::Brand
            } else {
                DiffPanelTone::Default
            },
        )
        .bold(),
        DiffPanelSpan::new(padding, DiffPanelTone::Default),
        DiffPanelSpan::new(add, DiffPanelTone::Add),
        DiffPanelSpan::new(" ", DiffPanelTone::Default),
        DiffPanelSpan::new(del, DiffPanelTone::Remove),
    ])
    .selected(selected)
}

fn file_summary_spans(file: &DiffFile) -> Vec<DiffPanelSpan> {
    let mut spans = vec![DiffPanelSpan::new(
        format!(
            "{}  {}",
            status_label(file.status),
            display_path(&file.path)
        ),
        DiffPanelTone::Brand,
    )
    .bold()];
    if let Some(additions) = file.additions {
        spans.push(DiffPanelSpan::new(
            format!("  +{additions}"),
            DiffPanelTone::Add,
        ));
    }
    if let Some(deletions) = file.deletions {
        spans.push(DiffPanelSpan::new(
            format!("  -{deletions}"),
            DiffPanelTone::Remove,
        ));
    }
    spans
}

fn notice_row(text: &str) -> DiffPanelRow {
    DiffPanelRow::new(vec![DiffPanelSpan::new(text, DiffPanelTone::Muted)])
}

fn status_label(status: DiffFileStatus) -> &'static str {
    match status {
        DiffFileStatus::Modified => "M",
        DiffFileStatus::Added => "A",
        DiffFileStatus::Deleted => "D",
        DiffFileStatus::Renamed => "R",
        DiffFileStatus::ModeChanged => "T",
        DiffFileStatus::Untracked => "?",
    }
}

fn l(en: &'static str, zh: &'static str) -> &'static str {
    match current_locale() {
        Locale::En => en,
        Locale::ZhCn => zh,
    }
}
