use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use atomcode_core::conversation::Conversation;
use atomcode_core::conversation::message::{MessageContent, Role};
use atomcode_core::tool::{ToolCall, ToolResult};

use super::markdown::render_markdown;

/// Colors / styles
const USER_LABEL_COLOR: Color = Color::Rgb(100, 180, 255);
const ASSISTANT_LABEL_COLOR: Color = Color::Rgb(180, 140, 255);
const SEPARATOR_COLOR: Color = Color::Rgb(50, 50, 50);
const USER_TEXT_COLOR: Color = Color::White;
const TOOL_BORDER_COLOR: Color = Color::Rgb(80, 80, 80);
const TOOL_SUCCESS_COLOR: Color = Color::Green;
const TOOL_ERROR_COLOR: Color = Color::Red;
const TOOL_WARN_COLOR: Color = Color::Yellow;

pub fn render(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    scroll_offset: usize,
    at_bottom: bool,
    waiting_approval: Option<&ToolCall>,
) {
    let mut all_lines: Vec<Line<'static>> = Vec::new();

    for msg in &conversation.messages {
        match &msg.content {
            MessageContent::Text(text) => {
                match msg.role {
                    Role::User => render_user_message(&mut all_lines, text),
                    Role::Assistant => render_assistant_message(&mut all_lines, text),
                    _ => {}
                }
            }
            MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                // Show assistant label
                all_lines.push(Line::from(Span::styled(
                    "  assistant",
                    Style::default().fg(ASSISTANT_LABEL_COLOR).add_modifier(Modifier::BOLD),
                )));
                all_lines.push(Line::default());

                // Render text part if present
                if let Some(t) = text {
                    let md_lines = render_markdown(t);
                    for line in md_lines {
                        let mut spans = vec![Span::raw("  ".to_string())];
                        spans.extend(line.spans);
                        all_lines.push(Line::from(spans));
                    }
                    all_lines.push(Line::default());
                }

                // Render tool call boxes
                for call in tool_calls {
                    render_tool_call_box(&mut all_lines, call, false);
                }
            }
            MessageContent::ToolResult(result) => {
                render_tool_result_line(&mut all_lines, result);
            }
        }
    }

    // Render streaming buffer
    if let Some(ref buffer) = conversation.stream_buffer {
        all_lines.push(Line::from(Span::styled(
            "  assistant",
            Style::default().fg(ASSISTANT_LABEL_COLOR).add_modifier(Modifier::BOLD),
        )));
        all_lines.push(Line::default());

        let md_lines = render_markdown(buffer);
        for line in md_lines {
            let mut spans = vec![Span::raw("  ".to_string())];
            spans.extend(line.spans);
            all_lines.push(Line::from(spans));
        }

        all_lines.push(Line::from(Span::styled(
            "  \u{2588}",
            Style::default().fg(ASSISTANT_LABEL_COLOR),
        )));
    }

    // Render approval prompt if waiting
    if let Some(call) = waiting_approval {
        render_tool_call_box(&mut all_lines, call, true);
        all_lines.push(Line::default());
        all_lines.push(Line::from(vec![
            Span::styled("  Allow ", Style::default().fg(TOOL_WARN_COLOR)),
            Span::styled(
                call.name.clone(),
                Style::default().fg(TOOL_WARN_COLOR).add_modifier(Modifier::BOLD),
            ),
            Span::styled("?  ", Style::default().fg(TOOL_WARN_COLOR)),
            Span::styled("[y]", Style::default().fg(TOOL_SUCCESS_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled("es  ", Style::default().fg(Color::Gray)),
            Span::styled("[n]", Style::default().fg(TOOL_ERROR_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled("o", Style::default().fg(Color::Gray)),
        ]));
        all_lines.push(Line::default());
    }

    // Calculate auto-scroll
    let total = all_lines.len();
    let viewport_h = area.height as usize;
    let effective_scroll = if at_bottom {
        total.saturating_sub(viewport_h)
    } else {
        scroll_offset.min(total.saturating_sub(viewport_h))
    };

    let paragraph = Paragraph::new(all_lines)
        .scroll((effective_scroll as u16, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(paragraph, area);
}

fn render_tool_call_box(lines: &mut Vec<Line<'static>>, call: &ToolCall, is_approval: bool) {
    let border = Style::default().fg(TOOL_BORDER_COLOR);
    let icon = if is_approval { "\u{26a0}" } else { "\u{1f527}" }; // ⚠ or 🔧
    let icon_color = if is_approval { TOOL_WARN_COLOR } else { Color::Cyan };

    // Top border with icon + name
    lines.push(Line::from(vec![
        Span::styled("  \u{256d}\u{2500} ", border),
        Span::styled(
            format!("{} {} ", icon, call.name),
            Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2500}".repeat(30), border),
    ]));

    // Parse and show arguments
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let val = match v {
                    serde_json::Value::String(s) => {
                        if s.len() > 60 {
                            let preview: String = s.chars().take(57).collect();
                            format!("{}...", preview)
                        } else {
                            s.clone()
                        }
                    }
                    other => other.to_string(),
                };
                lines.push(Line::from(vec![
                    Span::styled("  \u{2502} ", border),
                    Span::styled(format!("{}: ", k), Style::default().fg(Color::Gray)),
                    Span::styled(val, Style::default().fg(Color::White)),
                ]));
            }
        }
    }

    // If approval and it's write_file, show content preview
    if is_approval && call.name == "write_file" {
        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(content) = args.get("content").and_then(|v| v.as_str()) {
                lines.push(Line::from(Span::styled(
                    "  \u{2502} \u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}\u{2504}",
                    border,
                )));
                for line in content.lines().take(8) {
                    let display: String = line.chars().take(50).collect();
                    lines.push(Line::from(vec![
                        Span::styled("  \u{2502} ", border),
                        Span::styled(display, Style::default().fg(Color::Rgb(150, 150, 150))),
                    ]));
                }
                let total_lines_count = content.lines().count();
                if total_lines_count > 8 {
                    lines.push(Line::from(vec![
                        Span::styled("  \u{2502} ", border),
                        Span::styled(
                            format!("... ({} lines total)", total_lines_count),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
        }
    }

    // Bottom border
    lines.push(Line::from(Span::styled(
        format!("  \u{2570}{}", "\u{2500}".repeat(44)),
        border,
    )));
}

fn render_tool_result_line(lines: &mut Vec<Line<'static>>, result: &ToolResult) {
    let (icon, color) = if result.success {
        ("\u{2713}", TOOL_SUCCESS_COLOR)
    } else {
        ("\u{2717}", TOOL_ERROR_COLOR)
    };

    let summary: String = if result.output.len() > 80 {
        result.output.chars().take(80).collect::<String>() + "..."
    } else {
        result.output.clone()
    };

    lines.push(Line::from(Span::styled(
        format!("  {} {}", icon, summary),
        Style::default().fg(color),
    )));
    lines.push(Line::default());
}

fn render_user_message(lines: &mut Vec<Line<'static>>, content: &str) {
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(
            "  \u{f007} you",
            Style::default().fg(USER_LABEL_COLOR).add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::default());
    for text_line in content.lines() {
        lines.push(Line::from(vec![
            Span::raw("  ".to_string()),
            Span::styled(text_line.to_string(), Style::default().fg(USER_TEXT_COLOR)),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  \u{2500}".repeat(1) + &"\u{2500}".repeat(40),
        Style::default().fg(SEPARATOR_COLOR),
    )));
    lines.push(Line::default());
}

fn render_assistant_message(lines: &mut Vec<Line<'static>>, content: &str) {
    lines.push(Line::from(Span::styled(
        "  assistant",
        Style::default().fg(ASSISTANT_LABEL_COLOR).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());
    let md_lines = render_markdown(content);
    for line in md_lines {
        let mut spans = vec![Span::raw("  ".to_string())];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
}

/// Calculate total line count for scroll calculations.
pub fn total_lines(conversation: &Conversation) -> usize {
    let mut count = 0;
    for msg in &conversation.messages {
        match &msg.content {
            MessageContent::Text(text) => match msg.role {
                Role::User => count += 6 + text.lines().count(),
                Role::Assistant => count += 3 + render_markdown(text).len(),
                _ => {}
            },
            MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                count += 3; // label + blanks
                if let Some(t) = text {
                    count += render_markdown(t).len() + 1;
                }
                count += tool_calls.len() * 5; // approx: border + args + border per call
            }
            MessageContent::ToolResult(_) => count += 2, // result line + blank
        }
    }
    if let Some(ref buffer) = conversation.stream_buffer {
        count += 3 + render_markdown(buffer).len();
    }
    count
}
