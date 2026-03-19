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

const USER_BG: Color = Color::Rgb(35, 38, 52);
const DIM: Color = Color::Rgb(90, 90, 90);
const ACCENT: Color = Color::Rgb(130, 100, 255);
const TOOL_BORDER: Color = Color::Rgb(55, 55, 65);
const SUCCESS: Color = Color::Rgb(80, 200, 120);
const ERROR: Color = Color::Rgb(240, 80, 80);
const WARN: Color = Color::Rgb(240, 200, 60);

const SPINNER: &[&str] = &["\u{25dc}", "\u{25dd}", "\u{25de}", "\u{25df}"];

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

/// Full render — builds all lines, scrolls, draws.
/// Uses render_cache for completed messages (only rebuilt when msg count changes).
/// Dynamic parts (streaming, mode indicators) are appended fresh each frame.
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
    let vh = area.height as usize;
    if vh == 0 {
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

    // Count total lines = cached + dynamic
    let cached_len = render_cache.len();

    // Build dynamic lines (streaming + mode indicator) — small, cheap
    let mut dynamic: Vec<Line<'static>> = Vec::new();

    if let Some(ref buffer) = conversation.stream_buffer {
        if !buffer.is_empty() {
            dynamic.push(Line::from(Span::styled(
                "  \u{2502} ",
                Style::default().fg(ACCENT),
            )));
            let md = render_markdown(buffer);
            for line in md {
                let mut spans = vec![Span::raw("    ".to_string())];
                spans.extend(line.spans);
                dynamic.push(Line::from(spans));
            }
        }
    }

    let has_text = conversation.stream_buffer.as_ref().map_or(false, |b| !b.is_empty());
    let spinner = SPINNER[tick % SPINNER.len()];
    match mode {
        AppMode::Streaming if !has_text => {
            let label = THINKING_LABELS[conversation.messages.len() % THINKING_LABELS.len()];
            dynamic.push(Line::from(Span::styled(
                format!("    {} {}", spinner, label),
                Style::default().fg(ACCENT),
            )));
        }
        AppMode::Streaming => {
            dynamic.push(Line::from(Span::styled(
                format!("    {}", spinner),
                Style::default().fg(ACCENT),
            )));
        }
        AppMode::ToolExecuting => {
            dynamic.push(Line::from(Span::styled(
                format!("    {} Executing...", spinner),
                Style::default().fg(WARN),
            )));
        }
        AppMode::WaitingApproval(call) => {
            render_approval(&mut dynamic, call);
        }
        _ => {}
    }

    let total = cached_len + dynamic.len();

    // Scroll calculation
    let scroll = if at_bottom {
        total.saturating_sub(vh) as u16
    } else {
        (scroll_offset.min(total.saturating_sub(vh))) as u16
    };

    let scroll_usize = scroll as usize;

    // Build only the visible slice + some padding
    // Instead of cloning ALL cached lines, only take what's visible
    let mut visible: Vec<Line<'static>> = Vec::with_capacity(vh + 10);

    if scroll_usize < cached_len {
        // Visible range starts in cached lines
        let cache_start = scroll_usize;
        let cache_end = (scroll_usize + vh + 5).min(cached_len);
        visible.extend_from_slice(&render_cache[cache_start..cache_end]);

        // If we need dynamic lines too
        let remaining = (vh + 5).saturating_sub(cache_end - cache_start);
        if remaining > 0 {
            let dyn_end = remaining.min(dynamic.len());
            visible.extend(dynamic[..dyn_end].iter().cloned());
        }
    } else {
        // Scroll is past cached lines, only show dynamic
        let dyn_start = scroll_usize.saturating_sub(cached_len);
        let dyn_end = (dyn_start + vh + 5).min(dynamic.len());
        if dyn_start < dynamic.len() {
            visible.extend(dynamic[dyn_start..dyn_end].iter().cloned());
        }
    }

    // Pad to fill viewport
    while visible.len() < vh {
        visible.push(Line::default());
    }

    // Clear + render
    frame.render_widget(Clear, area);
    let bg = Block::default().style(Style::default().bg(Color::Reset));
    frame.render_widget(bg, area);

    // No scroll on Paragraph since we already sliced
    let paragraph = Paragraph::new(visible);
    frame.render_widget(paragraph, area);
}

fn render_user(lines: &mut Vec<Line<'static>>, content: &str) {
    lines.push(Line::default());
    let style = Style::default().fg(Color::White).bg(USER_BG);
    for text_line in content.lines() {
        lines.push(Line::from(vec![
            Span::styled("  > ", Style::default().fg(ACCENT).bg(USER_BG)),
            Span::styled(text_line.to_string(), style),
        ]));
    }
    lines.push(Line::default());
}

fn render_assistant(lines: &mut Vec<Line<'static>>, content: &str) {
    lines.push(Line::from(Span::styled(
        "  \u{2502} ",
        Style::default().fg(ACCENT),
    )));
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
            format!("> {}", name),
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
        ("+", SUCCESS)
    } else {
        ("x", ERROR)
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
            format!("! {} ", name),
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

    lines.push(Line::from(vec![
        Span::raw("    ".to_string()),
        Span::styled("[Y]", Style::default().fg(SUCCESS).add_modifier(Modifier::BOLD)),
        Span::styled(" Allow  ", Style::default().fg(Color::Gray)),
        Span::styled("[N]", Style::default().fg(ERROR).add_modifier(Modifier::BOLD)),
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
                            if s.len() > 30 { s[..27].to_string() + "..." } else { s }
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
                Role::Assistant => count += 1 + render_markdown(text).len(),
                _ => {}
            },
            MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                if let Some(t) = text {
                    count += 1 + render_markdown(t).len();
                }
                count += tool_calls.len();
            }
            MessageContent::ToolResult(_) => count += 1,
        }
    }
    if let Some(ref buffer) = conversation.stream_buffer {
        count += 1 + render_markdown(buffer).len() + 1;
    }
    count
}
