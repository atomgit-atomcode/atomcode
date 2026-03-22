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
/// Returns the actual scroll offset used (for text selection coordinate mapping).
pub fn render(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    scroll_offset: usize,
    at_bottom: bool,
    mode: &AppMode,
    tick: usize,
    turn_tokens: usize,
    turn_elapsed_secs: Option<u64>,
    turn_label_seed: usize,
    step_count: usize,    // Pre-computed, not scanned per frame
    tool_info: &str,       // Pre-computed, not parsed per frame
    render_cache: &mut Vec<Line<'static>>,
    render_cache_msg_count: &mut usize,
) -> usize {
    let vh = area.height as usize;
    if vh == 0 {
        return 0;
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
                    // Show text before tool calls as a "plan" with visual marker
                    if let Some(t) = text {
                        if !t.trim().is_empty() {
                            render_cache.push(Line::from(Span::styled(
                                "  \u{2502} Plan",
                                Style::default().fg(ACCENT).add_modifier(ratatui::style::Modifier::BOLD),
                            )));
                            let md = render_markdown(t);
                            for line in md {
                                let mut spans = vec![Span::raw("    ".to_string())];
                                spans.extend(line.spans);
                                render_cache.push(Line::from(spans));
                            }
                            render_cache.push(Line::default());
                        }
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

    // Active state indicator: spinner + label (fixed per turn) + stats
    let spinner = SPINNER[tick % SPINNER.len()];

    // Build stats string: elapsed | tokens | speed
    let stats = build_turn_stats(turn_elapsed_secs, turn_tokens);

    // Step indicator from pre-computed count (no per-frame scan)
    let step_prefix = if step_count > 0 {
        format!("[step {}] ", step_count + 1)
    } else {
        String::new()
    };

    match mode {
        AppMode::Streaming => {
            // Show a meaningful label based on context
            let label = if step_count > 0 && conversation.stream_buffer.as_ref().map_or(true, |b| b.is_empty()) {
                "Planning next step..."
            } else if step_count == 0 {
                THINKING_LABELS[turn_label_seed % THINKING_LABELS.len()]
            } else {
                "Generating..."
            };
            dynamic.push(Line::from(vec![
                Span::styled(
                    format!("    {} {}{}", spinner, step_prefix, label),
                    Style::default().fg(ACCENT),
                ),
                Span::styled(stats.clone(), Style::default().fg(DIM)),
            ]));
        }
        AppMode::ToolExecuting => {
            dynamic.push(Line::from(vec![
                Span::styled(
                    format!("    {} {}Running {}", spinner, step_prefix, tool_info),
                    Style::default().fg(WARN),
                ),
                Span::styled(stats.clone(), Style::default().fg(DIM)),
            ]));
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

    scroll_usize
}

fn render_user(lines: &mut Vec<Line<'static>>, content: &str) {
    lines.push(Line::default());
    let style = Style::default().fg(Color::White).bg(USER_BG);
    let text_lines: Vec<&str> = content.lines().collect();
    let total_chars: usize = content.len();

    if text_lines.len() <= 3 && total_chars <= 200 {
        // Short message — show in full
        for text_line in &text_lines {
            lines.push(Line::from(vec![
                Span::styled("  > ", Style::default().fg(ACCENT).bg(USER_BG)),
                Span::styled(text_line.to_string(), style),
            ]));
        }
    } else {
        // Long message — show first line + bracket-style indicator
        let first = text_lines.iter()
            .find(|l| !l.trim().is_empty())
            .unwrap_or(&text_lines[0]);
        let summary = if first.chars().count() > 70 {
            format!("{}...", first.chars().take(67).collect::<String>())
        } else {
            first.to_string()
        };

        lines.push(Line::from(vec![
            Span::styled("  > ", Style::default().fg(ACCENT).bg(USER_BG)),
            Span::styled(summary, style),
            Span::styled(
                format!("  [{} lines]", text_lines.len()),
                Style::default().fg(Color::Rgb(90, 100, 120)).bg(USER_BG),
            ),
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
    let detail = format_tool_detail(&call.name, &call.arguments);

    lines.push(Line::from(vec![
        Span::styled("    \u{2502} ", border),
        Span::styled(
            format!("> {}", name),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", detail),
            Style::default().fg(DIM),
        ),
    ]));

    // For edit_file: show old_string preview on a second line
    if call.name == "edit_file" {
        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(old) = args.get("old_string").and_then(|v| v.as_str()) {
                let preview = old.lines().next().unwrap_or("");
                let display = if preview.chars().count() > 60 {
                    format!("{}...", preview.chars().take(57).collect::<String>())
                } else {
                    preview.to_string()
                };
                if !display.trim().is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("    \u{2502}   ", border),
                        Span::styled(
                            format!("find: {}", display.trim()),
                            Style::default().fg(Color::Rgb(70, 70, 80)),
                        ),
                    ]));
                }
            }
        }
    }
    // For bash: show command on second line if it was truncated
    else if call.name == "bash" {
        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                if cmd.chars().count() > 60 {
                    // Show second line of multi-line commands
                    if let Some(second) = cmd.lines().nth(1) {
                        let display = if second.chars().count() > 60 {
                            format!("{}...", second.chars().take(57).collect::<String>())
                        } else {
                            second.to_string()
                        };
                        lines.push(Line::from(vec![
                            Span::styled("    \u{2502}   ", border),
                            Span::styled(
                                display.trim().to_string(),
                                Style::default().fg(Color::Rgb(70, 70, 80)),
                            ),
                        ]));
                    }
                }
            }
        }
    }
}

fn render_tool_result(lines: &mut Vec<Line<'static>>, result: &ToolResult) {
    let (icon, color) = if result.success {
        ("+", SUCCESS)
    } else {
        ("x", ERROR)
    };

    let output_lines: Vec<&str> = result.output.lines().collect();

    if output_lines.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("    \u{2502} ", Style::default().fg(TOOL_BORDER)),
            Span::styled(format!("{} ", icon), Style::default().fg(color)),
            Span::styled("(no output)", Style::default().fg(DIM)),
        ]));
        return;
    }

    // First line: status icon + summary
    let first = output_lines[0];
    let first_display = if first.chars().count() > 80 {
        first.chars().take(77).collect::<String>() + "..."
    } else {
        first.to_string()
    };
    lines.push(Line::from(vec![
        Span::styled("    \u{2502} ", Style::default().fg(TOOL_BORDER)),
        Span::styled(format!("{} ", icon), Style::default().fg(color)),
        Span::styled(first_display, Style::default().fg(DIM)),
    ]));

    // Show diff lines and additional detail (up to 8 more lines)
    let max_detail = 8;
    let mut shown = 0;
    for line in output_lines.iter().skip(1) {
        if shown >= max_detail { break; }
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }

        let (prefix_style, text_style) = if trimmed.starts_with("- ") {
            // Removed line — red
            (Style::default().fg(ERROR), Style::default().fg(Color::Rgb(200, 100, 100)))
        } else if trimmed.starts_with("+ ") {
            // Added line — green
            (Style::default().fg(SUCCESS), Style::default().fg(Color::Rgb(100, 200, 120)))
        } else if trimmed.starts_with("WARNING") || trimmed.starts_with("[IMPORTANT") {
            (Style::default().fg(WARN), Style::default().fg(WARN))
        } else {
            (Style::default().fg(DIM), Style::default().fg(DIM))
        };

        let display = if trimmed.chars().count() > 76 {
            trimmed.chars().take(73).collect::<String>() + "..."
        } else {
            trimmed.to_string()
        };

        lines.push(Line::from(vec![
            Span::styled("    \u{2502}   ", Style::default().fg(TOOL_BORDER)),
            Span::styled(display, text_style),
        ]));
        shown += 1;
    }

    // If more lines not shown
    let remaining = output_lines.len().saturating_sub(1 + max_detail);
    if remaining > 0 {
        lines.push(Line::from(vec![
            Span::styled("    \u{2502}   ", Style::default().fg(TOOL_BORDER)),
            Span::styled(
                format!("... {} more lines", remaining),
                Style::default().fg(Color::Rgb(60, 60, 70)),
            ),
        ]));
    }
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
        Span::styled("[A]", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(" Always  ", Style::default().fg(Color::Gray)),
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

/// Build stats string: "  12s | 1.2k tokens | 98 t/s"
fn build_turn_stats(elapsed_secs: Option<u64>, tokens: usize) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(secs) = elapsed_secs {
        if secs >= 60 {
            parts.push(format!("{}m{}s", secs / 60, secs % 60));
        } else {
            parts.push(format!("{}s", secs));
        }
    }

    if tokens > 0 {
        parts.push(format!("{} tokens", format_compact_tokens(tokens)));
        // Token speed
        if let Some(secs) = elapsed_secs {
            if secs > 0 {
                let speed = tokens as f64 / secs as f64;
                if speed >= 1.0 {
                    parts.push(format!("{:.0} t/s", speed));
                }
            }
        }
    }

    if parts.is_empty() {
        String::new()
    } else {
        format!("  {}", parts.join(" | "))
    }
}

fn format_compact_tokens(n: usize) -> String {
    if n < 1000 { format!("{}", n) }
    else if n < 1_000_000 { format!("{:.1}k", n as f64 / 1000.0) }
    else { format!("{:.1}M", n as f64 / 1_000_000.0) }
}

/// Format a human-readable one-line detail for a tool call, tailored per tool type.
/// Shows the most important info: file paths shortened, command previews, etc.
fn format_tool_detail(tool_name: &str, args_json: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    match tool_name {
        "read_file" => {
            let path = shorten_path(args.get("file_path").and_then(|v| v.as_str()).unwrap_or(""));
            let mut detail = path;
            if let Some(offset) = args.get("offset").and_then(|v| v.as_u64()) {
                detail.push_str(&format!(" L{}", offset));
                if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                    detail.push_str(&format!("-{}", offset + limit));
                }
            }
            detail
        }
        "write_file" => {
            let path = shorten_path(args.get("file_path").and_then(|v| v.as_str()).unwrap_or(""));
            let size = args.get("content").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0);
            format!("{} ({} bytes)", path, size)
        }
        "edit_file" => {
            let path = shorten_path(args.get("file_path").and_then(|v| v.as_str()).unwrap_or(""));
            let old_lines = args.get("old_string").and_then(|v| v.as_str()).map(|s| s.lines().count()).unwrap_or(0);
            let new_lines = args.get("new_string").and_then(|v| v.as_str()).map(|s| s.lines().count()).unwrap_or(0);
            format!("{} (-{} +{} lines)", path, old_lines, new_lines)
        }
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.chars().count() > 60 {
                cmd.chars().take(57).collect::<String>() + "..."
            } else {
                cmd.to_string()
            }
        }
        "list_directory" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(2);
            format!("{} (depth={})", shorten_path(path), depth)
        }
        "grep" | "glob" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("\"{}\" in {}", pattern, shorten_path(path))
        }
        _ => format_args_oneline(args_json),
    }
}

/// Shorten a file path for display: keep filename + parent dir, trim the rest.
fn shorten_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    match parts.len() {
        0 | 1 => path.to_string(),
        2 => format!("{}/{}", parts[1], parts[0]),
        _ => format!(".../{}/{}", parts[1], parts[0]),
    }
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
