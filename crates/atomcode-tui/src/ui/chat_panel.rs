use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
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

const SPINNER: &[&str] = &["\u{25dc}", "\u{25dd}", "\u{25de}", "\u{25df}"];

/// Fun thinking labels, rotated each time
const THINKING_LABELS: &[&str] = &[
    "Thinking...",
    "Pondering...",
    "Reasoning...",
    "Contemplating...",
    "Analyzing...",
    "Processing...",
    "Cogitating...",
    "Deliberating...",
];

pub fn render(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    scroll_offset: usize,
    at_bottom: bool,
    mode: &AppMode,
    tick: usize,
    render_cache: &mut Vec<Line<'static>>,
    render_cache_msg_count: &mut usize,
) {
    let width = area.width as usize;
    let vh = area.height as usize;
    if vh == 0 || width == 0 {
        return;
    }

    // Rebuild cache only when message count changes
    let msg_count = conversation.messages.len();
    if msg_count != *render_cache_msg_count {
        render_cache.clear();
        for msg in &conversation.messages {
            match &msg.content {
                MessageContent::Text(text) => match msg.role {
                    Role::User => render_user(render_cache, text),
                    Role::Assistant => render_assistant(render_cache, text),
                    _ => {}
                },
                MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                    if let Some(t) = text {
                        render_assistant(render_cache, t);
                    }
                    for call in tool_calls {
                        render_tool_call(render_cache, call);
                    }
                }
                MessageContent::ToolResult(result) => {
                    render_tool_result(render_cache, result);
                }
            }
        }
        *render_cache_msg_count = msg_count;
    }

    // Start with cached lines (clone is cheap — Line is just Vec<Span> with Cow<str>)
    let mut logical_lines: Vec<Line<'static>> = render_cache.clone();

    // Streaming buffer
    if let Some(ref buffer) = conversation.stream_buffer {
        if !buffer.is_empty() {
            let md = render_markdown(buffer);
            for line in md {
                let mut spans = vec![Span::raw("    ".to_string())];
                spans.extend(line.spans);
                logical_lines.push(Line::from(spans));
            }
        }
    }

    // State indicators — only show when there's nothing else visible
    let spinner = SPINNER[tick % SPINNER.len()];
    let has_text = conversation.stream_buffer.as_ref().map_or(false, |b| !b.is_empty());
    match mode {
        AppMode::Streaming if !has_text => {
            // Only show thinking label when LLM hasn't produced any text yet
            let label = THINKING_LABELS[(tick / 8) % THINKING_LABELS.len()];
            logical_lines.push(Line::from(Span::styled(
                format!("    {} {}", spinner, label),
                Style::default().fg(ACCENT),
            )));
        }
        AppMode::ToolExecuting => {
            logical_lines.push(Line::from(Span::styled(
                format!("    {} Executing...", spinner),
                Style::default().fg(WARN),
            )));
        }
        AppMode::WaitingApproval(call) => {
            render_approval(&mut logical_lines, call);
        }
        _ => {}
    }

    // Estimate wrapped line count: each logical line may wrap to multiple display lines
    let display_line_count: usize = logical_lines
        .iter()
        .map(|line| {
            let line_width: usize = line.spans.iter().map(|s| s.content.len()).sum();
            if line_width == 0 {
                1 // empty line still takes 1 row
            } else {
                (line_width + width - 1) / width // ceil division
            }
        })
        .sum();

    // Calculate scroll: use Paragraph's native scroll but with correct total
    let scroll = if at_bottom {
        display_line_count.saturating_sub(vh) as u16
    } else {
        (scroll_offset.min(display_line_count.saturating_sub(vh))) as u16
    };

    // Add padding lines at the bottom to ensure content can scroll up enough
    // This ensures the last line of content can appear at the top of viewport
    let padding_needed = vh.saturating_sub(2); // leave some room
    for _ in 0..padding_needed {
        logical_lines.push(Line::default());
    }

    // Clear the area first to prevent previous terminal content from showing through
    frame.render_widget(Clear, area);

    let bg = Block::default().style(Style::default().bg(Color::Reset));
    frame.render_widget(bg, area);

    let paragraph = Paragraph::new(logical_lines).scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

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

fn render_assistant(lines: &mut Vec<Line<'static>>, content: &str) {
    let md = render_markdown(content);
    for line in md {
        let mut spans = vec![Span::raw("    ".to_string())];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
}

fn render_tool_call(lines: &mut Vec<Line<'static>>, call: &ToolCall) {
    let border = Style::default().fg(TOOL_BORDER);
    let name = capitalize(&call.name);

    lines.push(Line::from(vec![
        Span::styled("    \u{2502} ", border),
        Span::styled(
            format!("\u{25b8} {}", name),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", format_args_oneline(&call.arguments)),
            Style::default().fg(DIM),
        ),
    ]));
}

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

fn render_approval(lines: &mut Vec<Line<'static>>, call: &ToolCall) {
    let name = capitalize(&call.name);
    let border = Style::default().fg(WARN);

    lines.push(Line::from(vec![
        Span::styled("    \u{256d}\u{2500} ", border),
        Span::styled(
            format!("\u{26a0} {} ", name),
            Style::default().fg(WARN).add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2500}".repeat(30), border),
    ]));

    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let val = match v {
                    serde_json::Value::String(s) => {
                        if k == "content" {
                            let lc = s.lines().count();
                            let preview: String = s.lines().take(5).collect::<Vec<_>>().join("\n");
                            if lc > 5 {
                                format!("{}\n    \u{2502}   ... ({} lines)", preview, lc)
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
                            Span::styled(
                                vline.to_string(),
                                Style::default().fg(Color::Rgb(150, 150, 150)),
                            ),
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

    lines.push(Line::from(vec![
        Span::raw("    ".to_string()),
        Span::styled(
            "[Y]",
            Style::default()
                .fg(SUCCESS)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Allow  ", Style::default().fg(Color::Gray)),
        Span::styled(
            "[N]",
            Style::default()
                .fg(ERROR)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" Deny", Style::default().fg(Color::Gray)),
    ]));
}

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

fn format_args_oneline(args_json: &str) -> String {
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) {
        if let Some(obj) = args.as_object() {
            let parts: Vec<String> = obj
                .iter()
                .map(|(k, v)| {
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
                            if s.len() > 30 {
                                s[..27].to_string() + "..."
                            } else {
                                s
                            }
                        }
                    };
                    format!("{}={}", k, val)
                })
                .collect();
            return parts.join(" ");
        }
    }
    String::new()
}

pub fn total_lines(conversation: &Conversation) -> usize {
    let mut count = 0;
    for msg in &conversation.messages {
        match &msg.content {
            MessageContent::Text(text) => match msg.role {
                Role::User => count += 2 + text.lines().count(),
                Role::Assistant => count += render_markdown(text).len(),
                _ => {}
            },
            MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                if let Some(t) = text {
                    count += render_markdown(t).len();
                }
                count += tool_calls.len();
            }
            MessageContent::ToolResult(_) => count += 1,
        }
    }
    if let Some(ref buffer) = conversation.stream_buffer {
        count += render_markdown(buffer).len() + 1;
    }
    count
}
