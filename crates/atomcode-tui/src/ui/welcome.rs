use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, WelcomeState};
use super::theme;

// ── Atom logo — 3-line orbital, compact & geeky ──
const ATOM: [&str; 3] = [
    r"╱·╲",
    r"( ◉ )",
    r"╲·╱",
];

/// Render the normal welcome screen (conversation empty, providers configured).
/// Design D: Hermes-style box layout with project context + recent sessions.
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let box_w = (area.width as usize).min(56);
    let inner_w = box_w.saturating_sub(4); // content width inside box

    let content_height = 22;
    let top_pad = if area.height as usize > content_height + 4 {
        (area.height as usize - content_height) / 3
    } else {
        1
    };

    for _ in 0..top_pad {
        lines.push(Line::default());
    }

    let b = Style::default().fg(theme::border());

    // ── Top border ──
    lines.push(Line::from(Span::styled(
        format!("  ┌{}┐", "─".repeat(box_w)),
        b,
    )));

    // ── Empty line ──
    lines.push(box_line("", box_w, b));

    // ── Logo + brand ──
    lines.push(box_line_spans(vec![
        Span::styled(format!("  {} ", ATOM[0]), Style::default().fg(theme::accent_dim())),
    ], box_w, b));

    lines.push(box_line_spans(vec![
        Span::styled(format!(" {} ", ATOM[1]), Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)),
        Span::styled("  Atom", Style::default().fg(theme::text_primary()).add_modifier(Modifier::BOLD)),
        Span::styled("Code", Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" v{}", env!("CARGO_PKG_VERSION")), Style::default().fg(theme::text_muted())),
    ], box_w, b));

    lines.push(box_line_spans(vec![
        Span::styled(format!("  {} ", ATOM[2]), Style::default().fg(theme::accent_dim())),
        Span::styled("  AI Coding Agent", Style::default().fg(theme::text_muted())),
    ], box_w, b));

    // ── Empty line ──
    lines.push(box_line("", box_w, b));

    // ── Middle separator ──
    lines.push(Line::from(Span::styled(
        format!("  ├{}┤", "─".repeat(box_w)),
        b,
    )));

    // ── Empty line ──
    lines.push(box_line("", box_w, b));

    // ── Project info ──
    let git = git_info(&app.working_dir);

    lines.push(box_line_spans(vec![
        Span::styled(" ● ", Style::default().fg(theme::accent())),
        Span::styled("Model   ", Style::default().fg(theme::text_muted())),
        Span::styled(app.model_name.clone(), Style::default().fg(theme::text_primary())),
    ], box_w, b));

    lines.push(box_line_spans(vec![
        Span::styled(" ● ", Style::default().fg(theme::info())),
        Span::styled("Path    ", Style::default().fg(theme::text_muted())),
        Span::styled(short_home(&app.working_dir), Style::default().fg(theme::text_primary())),
    ], box_w, b));

    if let Some((branch, dirty)) = &git {
        let mut spans = vec![
            Span::styled(" ● ", Style::default().fg(theme::warning())),
            Span::styled("Git     ", Style::default().fg(theme::text_muted())),
            Span::styled(format!(" {}", branch), Style::default().fg(theme::info())),
        ];
        if *dirty {
            spans.push(Span::styled(" ✱", Style::default().fg(theme::warning())));
        }
        lines.push(box_line_spans(spans, box_w, b));
    }

    // ── Empty line ──
    lines.push(box_line("", box_w, b));

    // ── Recent sessions ──
    let recent = load_recent_sessions(&app.working_dir);
    if !recent.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("  ├{}┤", "─".repeat(box_w)),
            b,
        )));
        lines.push(box_line("", box_w, b));

        lines.push(box_line_spans(vec![
            Span::styled(" Recent", Style::default().fg(theme::text_muted())),
        ], box_w, b));

        for (task, turns, ago) in recent.iter().take(3) {
            let max_task_w = inner_w.saturating_sub(20);
            let task_short = truncate_to_width(&task, max_task_w);
            lines.push(box_line_spans(vec![
                Span::styled(" ▸ ", Style::default().fg(theme::text_muted())),
                Span::styled(task_short, Style::default().fg(theme::text_secondary())),
                Span::styled(format!(" · {}T · {}", turns, ago), Style::default().fg(theme::border())),
            ], box_w, b));
        }

        lines.push(box_line("", box_w, b));
    }

    // ── Bottom border ──
    lines.push(Line::from(Span::styled(
        format!("  └{}┘", "─".repeat(box_w)),
        b,
    )));

    lines.push(Line::default());

    // ── Shortcuts (outside box, compact) ──
    let k = Style::default().fg(theme::text_secondary());
    let d = Style::default().fg(theme::text_muted());
    lines.push(Line::from(vec![
        Span::styled("  Enter ", k), Span::styled("send  ", d),
        Span::styled("↑↓ ", k), Span::styled("history  ", d),
        Span::styled("/ ", k), Span::styled("commands  ", d),
        Span::styled("/model ", k), Span::styled("switch  ", d),
        Span::styled("Ctrl+C ", k), Span::styled("exit", d),
    ]));

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Ask anything to get started.",
        Style::default().fg(theme::text_muted()),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

/// Full-screen first-run setup screen shown when no providers are configured.
pub fn render_setup(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let box_w = (area.width as usize).min(56);
    let b = Style::default().fg(theme::border());

    let content_height = 18;
    let top_pad = if area.height as usize > content_height + 4 {
        (area.height as usize - content_height) / 3
    } else {
        1
    };

    for _ in 0..top_pad {
        lines.push(Line::default());
    }

    // ── Top border ──
    lines.push(Line::from(Span::styled(
        format!("  ┌{}┐", "─".repeat(box_w)),
        b,
    )));

    lines.push(box_line("", box_w, b));

    // ── Logo ──
    lines.push(box_line_spans(vec![
        Span::styled(format!("  {} ", ATOM[0]), Style::default().fg(theme::accent_dim())),
    ], box_w, b));

    lines.push(box_line_spans(vec![
        Span::styled(format!(" {} ", ATOM[1]), Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)),
        Span::styled("  Atom", Style::default().fg(theme::text_primary()).add_modifier(Modifier::BOLD)),
        Span::styled("Code", Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" v{}", env!("CARGO_PKG_VERSION")), Style::default().fg(theme::text_muted())),
    ], box_w, b));

    lines.push(box_line_spans(vec![
        Span::styled(format!("  {} ", ATOM[2]), Style::default().fg(theme::accent_dim())),
        Span::styled("  AI Coding Agent", Style::default().fg(theme::text_muted())),
    ], box_w, b));

    lines.push(box_line("", box_w, b));

    // ── Separator ──
    lines.push(Line::from(Span::styled(
        format!("  ├{}┤", "─".repeat(box_w)),
        b,
    )));

    lines.push(box_line("", box_w, b));

    lines.push(box_line_spans(vec![
        Span::styled(" Get started:", Style::default().fg(theme::text_secondary())),
    ], box_w, b));

    lines.push(box_line("", box_w, b));

    let options = [
        ("Login with AtomGit", "OAuth · recommended"),
        ("Configure manually", "API key"),
        ("Skip for now",       "explore first"),
    ];
    for (i, (label, hint)) in options.iter().enumerate() {
        let is_sel = i == state.selected;
        if is_sel {
            lines.push(box_line_spans(vec![
                Span::styled(" ▸ ", Style::default().fg(theme::accent())),
                Span::styled(
                    label.to_string(),
                    Style::default().fg(theme::text_primary()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", hint),
                    Style::default().fg(theme::text_muted()),
                ),
            ], box_w, b));
        } else {
            lines.push(box_line_spans(vec![
                Span::styled("   ", Style::default()),
                Span::styled(label.to_string(), Style::default().fg(theme::text_secondary())),
            ], box_w, b));
        }
    }

    lines.push(box_line("", box_w, b));

    if let Some(ref err) = state.error {
        lines.push(box_line_spans(vec![
            Span::styled(format!(" {}", err), Style::default().fg(theme::error())),
        ], box_w, b));
        lines.push(box_line("", box_w, b));
    }

    // ── Bottom border ──
    lines.push(Line::from(Span::styled(
        format!("  └{}┘", "─".repeat(box_w)),
        b,
    )));

    lines.push(Line::default());

    lines.push(Line::from(Span::styled(
        "  ↑↓ select · Enter confirm · Esc skip",
        Style::default().fg(theme::text_muted()),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(Clear, area);
    frame.render_widget(paragraph, area);
}

// ─── Helpers ───────────────────────────────────────────────────────

/// Build a box line with plain text content, padded to box width.
fn box_line(text: &str, box_w: usize, border_style: Style) -> Line<'static> {
    let pad = box_w.saturating_sub(display_width(text) + 1); // +1 for leading space
    Line::from(vec![
        Span::styled("  │", border_style),
        Span::styled(format!(" {}", text), Style::default().fg(theme::text_primary())),
        Span::styled(" ".repeat(pad), Style::default()),
        Span::styled("│", border_style),
    ])
}

/// Build a box line with styled spans, padded to box width.
fn box_line_spans(spans: Vec<Span<'static>>, box_w: usize, border_style: Style) -> Line<'static> {
    // Use display width (CJK = 2, ASCII = 1) for correct alignment
    let content_width: usize = spans.iter().map(|s| display_width(&s.content)).sum();
    let pad = box_w.saturating_sub(content_width + 1); // +1 for leading space

    let mut result = Vec::with_capacity(spans.len() + 3);
    result.push(Span::styled("  │", border_style));
    result.push(Span::raw(" "));
    result.extend(spans);
    result.push(Span::raw(" ".repeat(pad)));
    result.push(Span::styled("│", border_style));
    Line::from(result)
}

/// Get current git branch + dirty status. Returns None if not a git repo.
fn git_info(wd: &std::path::Path) -> Option<(String, bool)> {
    let branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(wd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())?;

    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(wd)
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    Some((branch, dirty))
}

/// Display width accounting for CJK characters (width 2).
fn display_width(s: &str) -> usize {
    s.chars().map(|c| {
        let cp = c as u32;
        if (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp)
            || (0x20000..=0x2A6DF).contains(&cp) || (0xF900..=0xFAFF).contains(&cp)
            || (0xFF01..=0xFF60).contains(&cp) || (0xFFE0..=0xFFE6).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp) || (0x3000..=0x303F).contains(&cp)
            || (0x3040..=0x309F).contains(&cp) || (0x30A0..=0x30FF).contains(&cp)
        { 2 } else { 1 }
    }).sum()
}

/// Truncate string to fit within `max_width` display columns, appending '…' if truncated.
fn truncate_to_width(s: &str, max_width: usize) -> String {
    let mut width = 0;
    let mut result = String::new();
    for c in s.chars() {
        let cw = {
            let cp = c as u32;
            if (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp)
                || (0x20000..=0x2A6DF).contains(&cp) || (0xF900..=0xFAFF).contains(&cp)
                || (0xFF01..=0xFF60).contains(&cp) || (0xFFE0..=0xFFE6).contains(&cp)
                || (0xAC00..=0xD7AF).contains(&cp) || (0x3000..=0x303F).contains(&cp)
                || (0x3040..=0x309F).contains(&cp) || (0x30A0..=0x30FF).contains(&cp)
            { 2 } else { 1 }
        };
        if width + cw > max_width.saturating_sub(1) {
            result.push('…');
            return result;
        }
        width += cw;
        result.push(c);
    }
    result
}

/// Replace home directory prefix with ~
fn short_home(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

/// Load recent session summaries from datalog directory.
/// Returns Vec of (task_description, turn_count, time_ago_string).
fn load_recent_sessions(wd: &std::path::Path) -> Vec<(String, usize, String)> {
    let datalog_dir = wd.join("datalog");
    if !datalog_dir.exists() {
        return Vec::new();
    }

    let mut entries: Vec<_> = std::fs::read_dir(&datalog_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "md"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((e.path(), meta.modified().ok()?))
        })
        .collect();

    entries.sort_by(|a, b| b.1.cmp(&a.1));

    let now = std::time::SystemTime::now();
    let mut results = Vec::new();

    for (path, modified) in entries.iter().take(5) {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Extract user task (first ``` block)
        let task = content
            .split("```\n")
            .nth(1)
            .and_then(|block| block.split("\n```").next())
            .map(|s| {
                let first_line = s.lines().next().unwrap_or(s).trim();
                first_line.to_string()
            })
            .unwrap_or_default();

        if task.is_empty() {
            continue;
        }

        // Extract turn count from Stats line
        let turns: usize = content
            .rfind("**Stats:**")
            .and_then(|pos| {
                let stats = &content[pos..];
                stats.split_whitespace()
                    .find(|w| w.parse::<usize>().is_ok())
                    .and_then(|w| w.parse().ok())
            })
            .unwrap_or(0);

        // Time ago
        let ago = if let Ok(duration) = now.duration_since(*modified) {
            let mins = duration.as_secs() / 60;
            if mins < 1 {
                "just now".to_string()
            } else if mins < 60 {
                format!("{}m ago", mins)
            } else if mins < 1440 {
                format!("{}h ago", mins / 60)
            } else {
                format!("{}d ago", mins / 1440)
            }
        } else {
            String::new()
        };

        results.push((task, turns, ago));
    }

    results
}
