use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, WelcomeState};
use super::theme;

// ── Atom logo — 3-line orbital, compact & geeky ──
const ATOM: [&str; 3] = [
    r"   ╱·╲   ",
    r"  ( ◉ )  ",
    r"   ╲·╱   ",
];

/// Render the normal welcome screen (conversation empty, providers configured).
pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let content_height = 18;
    let top_pad = if area.height as usize > content_height + 4 {
        (area.height as usize - content_height) / 3
    } else {
        1
    };

    for _ in 0..top_pad {
        lines.push(Line::default());
    }

    // ── Atom art + info panel (neofetch style) ──
    let git = git_info(&app.working_dir);

    // Gradient: top orbit → core → bottom orbit
    let c_orbit = theme::accent_dim();
    let c_core = theme::accent();

    // Line 1: top orbit + "atomcode"
    lines.push(Line::from(vec![
        Span::styled(format!("  {}", ATOM[0]), Style::default().fg(c_orbit)),
        Span::styled(
            "  atomcode",
            Style::default().fg(theme::text_primary()).add_modifier(Modifier::BOLD),
        ),
    ]));
    // Line 2: core + version · model
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}", ATOM[1]),
            Style::default().fg(c_core).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme::text_muted()),
        ),
        Span::styled(" · ", Style::default().fg(theme::border())),
        Span::styled(
            app.model_name.clone(),
            Style::default().fg(theme::text_secondary()),
        ),
    ]));
    // Line 3: bottom orbit + path (+ git)
    let mut path_spans = vec![
        Span::styled(format!("  {}", ATOM[2]), Style::default().fg(c_orbit)),
        Span::styled(
            format!("  {}", short_home(&app.working_dir)),
            Style::default().fg(theme::text_muted()),
        ),
    ];
    if let Some((branch, dirty)) = &git {
        path_spans.push(Span::styled("  ", Style::default()));
        path_spans.push(Span::styled(
            format!(" {}", branch),
            Style::default().fg(theme::info()),
        ));
        if *dirty {
            path_spans.push(Span::styled(
                " ✱",
                Style::default().fg(theme::warning()),
            ));
        }
    }
    lines.push(Line::from(path_spans));

    lines.push(Line::default());

    // ── Separator ──
    let sep_w = (area.width as usize).min(60);
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(sep_w.saturating_sub(4))),
        Style::default().fg(theme::border()),
    )));
    lines.push(Line::default());

    // ── Shortcuts — two-column ──
    let k = Style::default().fg(theme::text_secondary());
    let d = Style::default().fg(theme::text_muted());

    lines.push(two_col("Enter",       "Send",            "/",        "Commands", k, d));
    lines.push(two_col("Shift+Enter", "Newline",         "/model",   "Switch model", k, d));
    lines.push(two_col("Esc",         "Clear / cancel",  "/cd",      "Change dir", k, d));
    lines.push(two_col("↑ ↓",         "History",         "/provider","Providers", k, d));
    lines.push(two_col("Ctrl+C",      "Exit",            "Ctrl+L",   "Clear chat", k, d));

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Type a message to get started.",
        Style::default().fg(theme::text_muted()),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

/// Full-screen first-run setup screen shown when no providers are configured.
pub fn render_setup(frame: &mut Frame, area: Rect, state: &WelcomeState) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    let content_height = 16;
    let top_pad = if area.height as usize > content_height + 4 {
        (area.height as usize - content_height) / 3
    } else {
        1
    };

    for _ in 0..top_pad {
        lines.push(Line::default());
    }

    let c_orbit = theme::accent_dim();
    let c_core = theme::accent();

    // ── Atom + brand ──
    lines.push(Line::from(vec![
        Span::styled(format!("  {}", ATOM[0]), Style::default().fg(c_orbit)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            format!("  {}", ATOM[1]),
            Style::default().fg(c_core).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "  atomcode",
            Style::default().fg(theme::text_primary()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme::text_muted()),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(format!("  {}", ATOM[2]), Style::default().fg(c_orbit)),
    ]));

    lines.push(Line::default());

    let sep_w = (area.width as usize).min(48);
    lines.push(Line::from(Span::styled(
        format!("  {}", "─".repeat(sep_w.saturating_sub(4))),
        Style::default().fg(theme::border()),
    )));
    lines.push(Line::default());

    lines.push(Line::from(Span::styled(
        "  Get started:",
        Style::default().fg(theme::text_secondary()),
    )));
    lines.push(Line::default());

    let options = [
        ("Login with AtomGit", "OAuth · recommended"),
        ("Configure manually", "API key"),
        ("Skip for now",       "explore first"),
    ];
    for (i, (label, hint)) in options.iter().enumerate() {
        let is_sel = i == state.selected;
        if is_sel {
            lines.push(Line::from(vec![
                Span::styled("  ▸ ", Style::default().fg(theme::accent())),
                Span::styled(
                    label.to_string(),
                    Style::default().fg(theme::text_primary()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", hint),
                    Style::default().fg(theme::text_muted()),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                Span::styled(label.to_string(), Style::default().fg(theme::text_secondary())),
            ]));
        }
    }

    lines.push(Line::default());

    if let Some(ref err) = state.error {
        lines.push(Line::from(Span::styled(
            format!("  {}", err),
            Style::default().fg(theme::error()),
        )));
        lines.push(Line::default());
    }

    lines.push(Line::from(Span::styled(
        "  ↑↓ select · Enter confirm · Esc skip",
        Style::default().fg(theme::text_muted()),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

// ─── Helpers ───────────────────────────────────────────────────────

/// Two-column shortcut row.
fn two_col(k1: &str, d1: &str, k2: &str, d2: &str, ks: Style, ds: Style) -> Line<'static> {
    let kw: usize = 14;
    let dw: usize = 16;
    let gap: usize = 4;

    Line::from(vec![
        Span::styled("  ", ds),
        Span::styled(k1.to_string(), ks),
        Span::styled(" ".repeat(kw.saturating_sub(k1.len())), ds),
        Span::styled(d1.to_string(), ds),
        Span::styled(" ".repeat(dw.saturating_sub(d1.len()) + gap), ds),
        Span::styled(k2.to_string(), ks),
        Span::styled(" ".repeat(kw.saturating_sub(k2.len())), ds),
        Span::styled(d2.to_string(), ds),
    ])
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

/// Replace home directory prefix with ~
fn short_home(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}
