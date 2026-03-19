use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use atomcode_core::conversation::Conversation;
use atomcode_core::conversation::message::{MessageContent, Role};
use atomcode_core::tool::{ToolCall, ToolResult};

use crate::app::AppMode;

use super::markdown::render_markdown;

/// Colors
const USER_BG: Color = Color::Rgb(35, 38, 52);
const DIM: Color = Color::Rgb(90, 90, 90);
const ACCENT: Color = Color::Rgb(130, 100, 255);
const TOOL_BORDER: Color = Color::Rgb(55, 55, 65);
const SUCCESS: Color = Color::Rgb(80, 200, 120);
const ERROR: Color = Color::Rgb(240, 80, 80);
const WARN: Color = Color::Rgb(240, 200, 60);

/// Spinner frames for thinking/executing animation
const SPINNER: &[&str] = &["\u{25dc}", "\u{25dd}", "\u{25de}", "\u{25df}"]; // ◜ ◝ ◞ ◟

pub fn render(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    scroll_offset: usize,
    at_bottom: bool,
    mode: &AppMode,
    tick: usize,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    for msg in &conversation.messages {
        match &msg.content {
            MessageContent::Text(text) => match msg.role {
                Role::User => render_user(&mut lines, text),
                Role::Assistant => render_assistant(&mut lines, text),
                _ => {}
            },
            MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                if let Some(t) = text {
                    render_assistant(&mut lines, t);
                }
                for call in tool_calls {
                    render_tool_call(&mut lines, call);
                }
            }
            MessageContent::ToolResult(result) => {
                render_tool_result(&mut lines, result);
            }
        }
    }

    // Streaming buffer
    if let Some(ref buffer) = conversation.stream_buffer {
        if !buffer.is_empty() {
            let md = render_markdown(buffer);
            for line in md {
                let mut spans = vec![Span::raw("    ".to_string())];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }
            // Blinking cursor
            let cursor_char = if tick % 2 == 0 { "\u{2588}" } else { " " };
            lines.push(Line::from(Span::styled(
                format!("    {}", cursor_char),
                Style::default().fg(ACCENT),
            )));
        }
    }

    // Mode-specific indicators
    match mode {
        AppMode::Streaming if conversation.stream_buffer.as_ref().map_or(true, |b| b.is_empty()) => {
            // LLM is thinking but hasn't produced text yet — show spinner
            let spinner = SPINNER[tick % SPINNER.len()];
            lines.push(Line::from(Span::styled(
                format!("    {} Thinking...", spinner),
                Style::default().fg(ACCENT),
            )));
        }
        AppMode::ToolExecuting => {
            let spinner = SPINNER[tick % SPINNER.len()];
            lines.push(Line::from(Span::styled(
                format!("    {} Executing...", spinner),
                Style::default().fg(WARN),
            )));
        }
        AppMode::WaitingApproval(call) => {
            render_approval(&mut lines, call);
        }
        _ => {}
    }

    // Auto-scroll
    let total = lines.len();
    let vh = area.height as usize;
    let scroll = if at_bottom {
        total.saturating_sub(vh)
    } else {
        scroll_offset.min(total.saturating_sub(vh))
    };

    let paragraph = Paragraph::new(lines)
        .scroll((scroll as u16, 0))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

/// User message — compact, subtle background tint, no label
fn render_user(lines: &mut Vec<Line<'static>>, content: &str) {
    lines.push(Line::default());
    let style = Style::default().fg(Color::White).bg(USER_BG);
    for text_line in content.lines() {
        lines.push(Line::from(vec![
            Span::styled("  \u{276f} ", Style::default().fg(ACCENT).bg(USER_BG)),
            Span::styled(text_line.to_string(), style),
        ]));
    }
    lines.push(Line::default());
}

/// Assistant text — clean markdown, no label
fn render_assistant(lines: &mut Vec<Line<'static>>, content: &str) {
    let md = render_markdown(content);
    for line in md {
        let mut spans = vec![Span::raw("    ".to_string())];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
}

/// Tool call box — compact, professional
fn render_tool_call(lines: &mut Vec<Line<'static>>, call: &ToolCall) {
    let border = Style::default().fg(TOOL_BORDER);
    let name = capitalize(&call.name);

    // Compact: icon + name on one line with border
    lines.push(Line::from(vec![
        Span::styled("    \u{2502} ", border),
        Span::styled(
            format!("\u{25b8} {}", name),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", format_args_oneline(&call.arguments)), Style::default().fg(DIM)),
    ]));
}

/// Tool result — single compact line
fn render_tool_result(lines: &mut Vec<Line<'static>>, result: &ToolResult) {
    let (icon, color) = if result.success {
        ("\u{2713}", SUCCESS)
    } else {
        ("\u{2717}", ERROR)
    };

    let summary: String = result.output.lines().next().unwrap_or("").to_string();
    let summary: String = if summary.chars().count() > 70 {
        summary.chars().take(67).collect::<String>() + "..."
    } else {
        summary
    };

    lines.push(Line::from(vec![
        Span::styled("    \u{2502} ", Style::default().fg(TOOL_BORDER)),
        Span::styled(format!("{} ", icon), Style::default().fg(color)),
        Span::styled(summary, Style::default().fg(DIM)),
    ]));
}

/// Approval prompt — inline, urgent
fn render_approval(lines: &mut Vec<Line<'static>>, call: &ToolCall) {
    let name = capitalize(&call.name);
    let border = Style::default().fg(WARN);

    // Show tool box with warning style
    lines.push(Line::from(vec![
        Span::styled("    \u{256d}\u{2500} ", border),
        Span::styled(
            format!("\u{26a0} {} ", name),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2500}".repeat(30), border),
    ]));

    // Show arguments
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let val = match v {
                    serde_json::Value::String(s) => {
                        if k == "content" {
                            // Show preview for write content
                            let line_count = s.lines().count();
                            let preview: String = s.lines().take(5).collect::<Vec<_>>().join("\n");
                            if line_count > 5 {
                                format!("{}\n    \u{2502}   ... ({} lines)", preview, line_count)
                            } else {
                                preview
                            }
                        } else if s.chars().count() > 50 {
                            s.chars().take(47).collect::<String>() + "..."
                        } else {
                            s.clone()
                        }
                    }
                    other => other.to_string(),
                };
                // Multi-line values (content preview)
                for (i, vline) in val.lines().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled("    \u{2502} ", border),
                            Span::styled(format!("{}: ", k), Style::default().fg(Color::Gray)),
                            Span::styled(vline.to_string(), Style::default().fg(Color::White)),
                        ]));
                    } else {
                        lines.push(Line::from(vec![
                            Span::styled("    \u{2502}   ", border),
                            Span::styled(vline.to_string(), Style::default().fg(Color::Rgb(150, 150, 150))),
                        ]));
                    }
                }
            }
        }
    }

    lines.push(Line::from(Span::styled(
        format!("    \u{2570}{}", "\u{2500}".repeat(40)),
        border,
    )));

    // Approval prompt
    lines.push(Line::from(vec![
        Span::raw("    ".to_string()),
        Span::styled("[Y]", Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD)),
        Span::styled(" Allow  ", Style::default().fg(Color::Gray)),
        Span::styled("[N]", Style::default().fg(ERROR).add_modifier(Modifier::BOLD)),
        Span::styled(" Deny", Style::default().fg(Color::Gray)),
    ]));
}

/// Capitalize tool name: "read_file" → "Read File", "bash" → "Bash"
fn capitalize(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Format tool arguments as a compact one-line summary
fn format_args_oneline(args_json: &str) -> String {
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) {
        if let Some(obj) = args.as_object() {
            let parts: Vec<String> = obj.iter().map(|(k, v)| {
                let val = match v {
                    serde_json::Value::String(s) => {
                        if s.chars().count() > 30 {
                            s.chars().take(27).collect::<String>() + "..."
                        } else {
                            s.clone()
                        }
                    }
                    other => {
                        let s = other.to_string();
                        if s.len() > 30 { s[..27].to_string() + "..." } else { s }
                    }
                };
                format!("{}={}", k, val)
            }).collect();
            return parts.join(" ");
        }
    }
    String::new()
}

/// Calculate total line count for scroll.
pub fn total_lines(conversation: &Conversation) -> usize {
    let mut count = 0;
    for msg in &conversation.messages {
        match &msg.content {
            MessageContent::Text(text) => match msg.role {
                Role::User => count += 2 + text.lines().count(), // blank + lines + blank
                Role::Assistant => count += render_markdown(text).len(),
                _ => {}
            },
            MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                if let Some(t) = text { count += render_markdown(t).len(); }
                count += tool_calls.len(); // 1 line per call
            }
            MessageContent::ToolResult(_) => count += 1,
        }
    }
    if let Some(ref buffer) = conversation.stream_buffer {
        count += render_markdown(buffer).len() + 1;
    }
    count
}
