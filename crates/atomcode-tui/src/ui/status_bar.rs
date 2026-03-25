use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, AppMode};

// ── Bar background ──
const BAR_BG: Color = Color::Rgb(18, 18, 24);

// ── Brand pill ──
const BRAND_FG: Color = Color::Rgb(195, 185, 250);
const BRAND_BG: Color = Color::Rgb(55, 38, 120);

// ── Text tiers ──
const PATH_FG: Color = Color::Rgb(110, 112, 128);
const INFO_FG: Color = Color::Rgb(90, 92, 108);
const MODEL_FG: Color = Color::Rgb(175, 178, 200);
const SEP_FG: Color = Color::Rgb(42, 44, 55);

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let dir = shorten_path(&app.working_dir.to_string_lossy());
    let model = app.model_name.as_str();

    let mut left: Vec<Span> = Vec::new();
    let mut right: Vec<Span> = Vec::new();

    // ━━ LEFT SIDE ━━

    // 1. Brand pill
    left.push(Span::styled(
        " atomcode ",
        Style::default().fg(BRAND_FG).bg(BRAND_BG).add_modifier(Modifier::BOLD),
    ));

    // 2. Mode badge — colored bg pill per state, hidden when idle
    match &app.mode {
        AppMode::Normal => {
            left.push(Span::styled(" ", Style::default()));
        }
        AppMode::Streaming => {
            left.push(Span::styled(
                " STREAMING ",
                Style::default()
                    .fg(Color::Rgb(28, 24, 10))
                    .bg(Color::Rgb(215, 170, 40))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        AppMode::ToolExecuting => {
            left.push(Span::styled(
                " RUNNING ",
                Style::default()
                    .fg(Color::Rgb(12, 22, 38))
                    .bg(Color::Rgb(65, 160, 225))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        AppMode::WaitingApproval(_) => {
            left.push(Span::styled(
                " APPROVAL ",
                Style::default()
                    .fg(Color::Rgb(32, 18, 8))
                    .bg(Color::Rgb(230, 155, 45))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        AppMode::ModelSelector => {
            left.push(Span::styled(
                " MODEL ",
                Style::default()
                    .fg(Color::Rgb(225, 215, 255))
                    .bg(Color::Rgb(75, 55, 150))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        AppMode::ProviderManager => {
            left.push(Span::styled(
                " CONFIG ",
                Style::default()
                    .fg(Color::Rgb(225, 215, 255))
                    .bg(Color::Rgb(75, 55, 150))
                    .add_modifier(Modifier::BOLD),
            ));
        }
        AppMode::Exiting => {
            left.push(Span::styled(
                " EXIT ",
                Style::default()
                    .fg(Color::Rgb(255, 225, 225))
                    .bg(Color::Rgb(175, 45, 45))
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }

    // 3. Path
    left.push(Span::styled(" ", Style::default()));
    left.push(Span::styled(dir, Style::default().fg(PATH_FG)));

    // ━━ RIGHT SIDE ━━

    // Timer
    let (time_str, time_color) = if let Some(start) = app.turn_start {
        (format_duration(start.elapsed().as_secs()), Color::Rgb(215, 170, 45))
    } else if let Some(dur) = app.last_turn_duration {
        (format_duration(dur.as_secs()), Color::Rgb(75, 165, 95))
    } else {
        (String::new(), INFO_FG)
    };

    if !time_str.is_empty() {
        right.push(Span::styled(time_str, Style::default().fg(time_color)));
        right.push(Span::styled(" \u{2502} ", Style::default().fg(SEP_FG)));
    }

    // Speed indicator — t/s during streaming, tool elapsed during executing
    match &app.mode {
        AppMode::Streaming => {
            if app.turn_tokens > 0 {
                if let Some(start) = app.llm_call_start {
                    let secs = start.elapsed().as_secs_f64();
                    if secs > 0.5 {
                        let speed = app.turn_tokens as f64 / secs;
                        let speed_color = if speed >= 60.0 {
                            Color::Rgb(75, 165, 95)  // green — fast
                        } else if speed >= 25.0 {
                            Color::Rgb(215, 170, 45) // yellow — normal
                        } else {
                            Color::Rgb(210, 80, 70)  // red — slow
                        };
                        right.push(Span::styled(
                            format!("{:.0} t/s", speed),
                            Style::default().fg(speed_color).add_modifier(Modifier::BOLD),
                        ));
                        right.push(Span::styled(" \u{2502} ", Style::default().fg(SEP_FG)));
                    }
                }
            }
            // TTFT moved to chat panel spinner — not shown in status bar.
        }
        AppMode::ToolExecuting => {
            if let Some(start) = app.tool_start {
                let tool_ms = start.elapsed().as_millis();
                let tool_str = if tool_ms >= 1000 {
                    format!("tool {:.1}s", tool_ms as f64 / 1000.0)
                } else {
                    format!("tool {}ms", tool_ms)
                };
                right.push(Span::styled(tool_str, Style::default().fg(Color::Rgb(65, 160, 225))));
                right.push(Span::styled(" \u{2502} ", Style::default().fg(SEP_FG)));
            }
        }
        _ => {}
    }

    // Tokens
    if app.total_tokens > 0 {
        let token_text = if app.turn_tokens > 0 && app.turn_start.is_some() {
            format!("{}(+{})", format_tokens(app.total_tokens), format_tokens(app.turn_tokens))
        } else {
            format_tokens(app.total_tokens)
        };
        right.push(Span::styled(
            format!("{} tok", token_text),
            Style::default().fg(INFO_FG),
        ));
        right.push(Span::styled(" \u{2502} ", Style::default().fg(SEP_FG)));
    }

    // Model — bright, bold, rightmost
    right.push(Span::styled(
        format!("{} ", model),
        Style::default().fg(MODEL_FG).add_modifier(Modifier::BOLD),
    ));

    // ━━ LAYOUT — display-width aware ━━
    let left_w: usize = left.iter().map(|s| display_width(&s.content)).sum();
    let right_w: usize = right.iter().map(|s| display_width(&s.content)).sum();
    let pad = width.saturating_sub(left_w + right_w);

    let mut all = left;
    all.push(Span::styled(" ".repeat(pad), Style::default()));
    all.extend(right);

    let bar = Paragraph::new(Line::from(all))
        .style(Style::default().bg(BAR_BG));
    frame.render_widget(bar, area);
}

/// Display width — counts CJK/fullwidth chars as 2 columns.
fn display_width(s: &str) -> usize {
    s.chars().map(|c| {
        let cp = c as u32;
        if (0x4E00..=0x9FFF).contains(&cp) || (0x3400..=0x4DBF).contains(&cp)
            || (0x20000..=0x2A6DF).contains(&cp) || (0xF900..=0xFAFF).contains(&cp)
            || (0xFF01..=0xFF60).contains(&cp) || (0xFFE0..=0xFFE6).contains(&cp)
            || (0xAC00..=0xD7AF).contains(&cp) || (0x3000..=0x303F).contains(&cp)
            || (0x3040..=0x309F).contains(&cp) || (0x30A0..=0x30FF).contains(&cp)
        {
            2
        } else {
            1
        }
    }).sum()
}

fn shorten_path(path: &str) -> String {
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    let shortened = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    if shortened.len() > 50 {
        let parts: Vec<&str> = shortened.rsplitn(3, '/').collect();
        if parts.len() >= 3 {
            format!(".../{}/{}", parts[1], parts[0])
        } else {
            shortened
        }
    } else {
        shortened
    }
}

fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else {
        format!("{}m{}s", secs / 60, secs % 60)
    }
}

fn format_tokens(n: usize) -> String {
    if n < 1000 {
        format!("{}", n)
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}
