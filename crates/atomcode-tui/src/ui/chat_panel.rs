use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::Frame;

use atomcode_core::conversation::Conversation;
use atomcode_core::conversation::message::{MessageContent, Role};
use atomcode_core::tool::{ToolCall, ToolResult};

use crate::app::AppMode;

use super::markdown::{render_markdown, wrap_lines};
use super::theme;

// Braille spinner
const SPINNER: &[&str] = &["\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}", "\u{2807}", "\u{280f}"];

#[allow(dead_code)]
const THINKING_LABELS: &[&str] = &["Thinking", "Pondering", "Reasoning", "Contemplating", "Analyzing", "Processing"];

// Left indent for content
const INDENT: &str = " ";
// Tool indent inside accent bar
#[allow(dead_code)]
const TOOL_INDENT: &str = "   ";
pub fn render(
    frame: &mut Frame,
    area: Rect,
    conversation: &Conversation,
    scroll_offset: usize,
    at_bottom: bool,
    mode: &AppMode,
    tick: usize,
    _turn_tokens: usize,
    _turn_elapsed_secs: Option<u64>,
    turn_label_seed: usize,
    step_count: usize,
    tool_info: &str,
    first_token_ms: Option<u64>,
    llm_wait_ms: Option<u64>,
    last_completed_tool: &str,
    last_turn_duration: Option<std::time::Duration>,
    finished_step_count: usize,
    render_cache: &mut Vec<Line<'static>>,
    render_cache_msg_count: &mut usize,
) -> (usize, usize) {
    let right_margin: u16 = 2;
    let area = Rect {
        width: area.width.saturating_sub(right_margin),
        ..area
    };

    let vh = (area.height as usize).saturating_sub(1);
    if vh == 0 { return (0, 0); }

    let term_width = area.width as usize;

    // Encode msg_count + width into cache key so width changes invalidate cache.
    // Use upper 16 bits for width, lower bits for count.
    let msg_count = conversation.messages.len();
    let cache_key = msg_count | ((term_width & 0xFFFF) << 48);
    if cache_key != *render_cache_msg_count {
        render_cache.clear();

        // Count total tool results to determine which are "recent" (last 3
        // in the current turn get expanded view by default).
        let total_tool_results = conversation.messages.iter().filter(|m| {
            matches!(&m.content, MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_))
        }).count();
        let mut tool_result_idx: usize = 0;

        // Find the index of the last user message to scope "recent" to current turn.
        let last_user_idx = conversation.messages.iter().rposition(|m| {
            matches!(m.role, Role::User) && matches!(&m.content, MessageContent::Text(..))
        }).unwrap_or(0);
        // Count tool results after the last user message
        let results_in_current_turn = conversation.messages[last_user_idx..].iter().filter(|m| {
            matches!(&m.content, MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_))
        }).count();
        // The last 3 tool results in the current turn should be expanded
        let expand_threshold = total_tool_results.saturating_sub(results_in_current_turn.min(3));

        for msg in &conversation.messages {
            match &msg.content {
                MessageContent::Text(text) => match msg.role {
                    Role::User => render_user(render_cache, text),
                    Role::Assistant => render_assistant(render_cache, text, term_width),
                    _ => {}
                },
                MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                    if let Some(t) = text {
                        if !t.trim().is_empty() {
                            render_assistant(render_cache, t, term_width);
                            render_cache.push(Line::default());
                        }
                    }
                    for call in tool_calls {
                        render_tool_call(render_cache, call);
                    }
                }
                MessageContent::ToolResult(result) => {
                    let expanded = tool_result_idx >= expand_threshold;
                    render_tool_result(render_cache, result, expanded);
                    render_cache.push(Line::default());
                    tool_result_idx += 1;
                }
                MessageContent::ToolResultRef(r) => {
                    // Render ref as a synthetic ToolResult using its summary.
                    let synthetic = atomcode_core::tool::ToolResult {
                        call_id: r.call_id.clone(),
                        output: r.summary.clone(),
                        success: r.success,
                    };
                    let expanded = tool_result_idx >= expand_threshold;
                    render_tool_result(render_cache, &synthetic, expanded);
                    render_cache.push(Line::default());
                    tool_result_idx += 1;
                }
            }
        }
        *render_cache_msg_count = cache_key;
    }

    let cached_len = render_cache.len();
    let mut dynamic: Vec<Line<'static>> = Vec::new();

    // Streaming content — accent bar + indented markdown (same as completed assistant text)
    let bar_style = Style::default().fg(theme::ACCENT_DIM);
    if let Some(ref buffer) = conversation.stream_buffer {
        if !buffer.is_empty() {
            let content_w = term_width.saturating_sub(3);
            // Trim trailing incomplete markdown tokens before rendering.
            // During streaming, the buffer may end with partial backtick fences
            // (`, ``, ```) that cause the parser to misinterpret subsequent text.
            let safe_buf = trim_incomplete_markdown(buffer);
            let md = wrap_lines(render_markdown(&safe_buf), content_w);
            for line in md {
                let mut spans = vec![Span::styled(format!("{}\u{2502} ", INDENT), bar_style)];
                spans.extend(line.spans);
                dynamic.push(Line::from(spans));
            }
        }
    }

    // (Spinner variables moved to the dedicated spinner_area renderer at the top)

    // ── Turn finished separator ──
    // Fun completion message like Claude Code's "Brewed for 47s"
    if let Some(dur) = last_turn_duration {
        let secs = dur.as_secs();
        let time_str = if secs >= 60 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}s", secs)
        };

        let verbs = [
            "Crafted", "Forged", "Conjured", "Assembled", "Brewed",
            "Synthesized", "Woven", "Sculpted", "Composed", "Distilled",
        ];
        let verb = verbs[turn_label_seed % verbs.len()];

        dynamic.push(Line::default());

        let mut parts: Vec<Span> = Vec::new();
        parts.push(Span::styled(
            format!("{}  \u{2713} ", INDENT),
            Style::default().fg(theme::SUCCESS),
        ));
        parts.push(Span::styled(
            format!("{} in {}", verb, time_str),
            Style::default().fg(theme::TEXT_SECONDARY),
        ));
        if finished_step_count > 0 {
            parts.push(Span::styled(
                format!(" \u{00b7} {} turns", finished_step_count),
                Style::default().fg(theme::TEXT_MUTED),
            ));
        }

        dynamic.push(Line::from(parts));
    }

    // Approval prompt — render when waiting for user approval
    if let AppMode::WaitingApproval(ref call) = mode {
        render_approval(&mut dynamic, call);
    }

    let total = cached_len + dynamic.len();
    let scroll = if at_bottom {
        total.saturating_sub(vh) as u16
    } else {
        (scroll_offset.min(total.saturating_sub(vh))) as u16
    };
    let scroll_usize = scroll as usize;

    let mut visible: Vec<Line<'static>> = Vec::with_capacity(vh + 10);
    if scroll_usize < cached_len {
        let cache_start = scroll_usize;
        let cache_end = (scroll_usize + vh + 5).min(cached_len);
        visible.extend_from_slice(&render_cache[cache_start..cache_end]);
        let remaining = (vh + 5).saturating_sub(cache_end - cache_start);
        if remaining > 0 {
            let dyn_end = remaining.min(dynamic.len());
            visible.extend(dynamic[..dyn_end].iter().cloned());
        }
    } else {
        let dyn_start = scroll_usize.saturating_sub(cached_len);
        let dyn_end = (dyn_start + vh + 5).min(dynamic.len());
        if dyn_start < dynamic.len() {
            visible.extend(dynamic[dyn_start..dyn_end].iter().cloned());
        }
    }

    while visible.len() < vh {
        visible.push(Line::default());
    }

    frame.render_widget(Clear, area);
    let bg = Block::default().style(Style::default().bg(Color::Reset));
    frame.render_widget(bg, area);
    let paragraph = Paragraph::new(visible).wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);

    // ── Spinner overlay: rendered AFTER the Paragraph, at the last row of chat area ──
    // This overwrites whatever the Paragraph put there. Immune to Wrap-induced
    // scroll miscalculations because it doesn't depend on the Paragraph's content.
    let turn_active = last_turn_duration.is_none() && (
        matches!(mode, AppMode::Streaming | AppMode::ToolExecuting)
        || step_count > 0
    );
    if turn_active && area.height >= 2 {
        let spin_y = area.y + area.height - 1;
        let spin_rect = Rect::new(area.x, spin_y, area.width, 1);
        frame.render_widget(Clear, spin_rect);

        let spinner = SPINNER[tick % SPINNER.len()];
        let bar_style = Style::default().fg(theme::ACCENT_DIM);
        let wait_ms = llm_wait_ms.unwrap_or(0);
        let spin_color = if first_token_ms.is_some() {
            theme::ACCENT
        } else if wait_ms < 10_000 {
            theme::WAIT_FAST
        } else if wait_ms < 60_000 {
            theme::WAIT_NORMAL
        } else if wait_ms < 120_000 {
            theme::WAIT_SLOW
        } else {
            theme::WAIT_VERY_SLOW
        };
        let step_prefix = if step_count > 0 { format!("[turn {}] ", step_count) } else { String::new() };
        let has_tokens = conversation.stream_buffer.as_ref().map_or(false, |b| !b.is_empty());
        let label = if matches!(mode, AppMode::ToolExecuting) && !tool_info.is_empty() {
            tool_info.to_string()
        } else if has_tokens {
            "Generating...".to_string()
        } else if step_count > 0 && !last_completed_tool.is_empty() {
            format!("{}", last_completed_tool)
        } else {
            "Thinking...".to_string()
        };
        let time_str = if wait_ms >= 60_000 {
            format!("  {:.0}m{:.0}s", wait_ms / 60_000, (wait_ms % 60_000) / 1000)
        } else if wait_ms >= 1000 {
            format!("  {:.1}s", wait_ms as f64 / 1000.0)
        } else {
            String::new()
        };

        let spin_line = Line::from(vec![
            Span::styled(format!("{}\u{2502} ", INDENT), bar_style),
            Span::styled(format!("{} ", spinner), Style::default().fg(spin_color)),
            Span::styled(step_prefix, Style::default().fg(theme::TEXT_SECONDARY)),
            Span::styled(label, Style::default().fg(spin_color)),
            Span::styled(time_str, Style::default().fg(spin_color)),
        ]);
        frame.render_widget(Paragraph::new(vec![spin_line]), spin_rect);
    }
    // Return (total_lines, scroll_offset) for scroll handling
    (total, scroll_usize)
}
// ── User Message ──
// Clean chevron prefix, bright text, separator after
// Always show full content (like Claude Code) so users can copy-paste
fn render_user(lines: &mut Vec<Line<'static>>, content: &str) {
    lines.push(Line::default());
    let text_lines: Vec<&str> = content.lines().collect();

    // Show all lines - no truncation
    for (i, text_line) in text_lines.iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(format!("{}\u{276f} ", INDENT), Style::default().fg(theme::USER_CHEVRON).add_modifier(Modifier::BOLD)),
                Span::styled(text_line.to_string(), Style::default().fg(theme::TEXT_PRIMARY)),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("{}  ", INDENT), Style::default()),
                Span::styled(text_line.to_string(), Style::default().fg(theme::TEXT_PRIMARY)),
            ]));
        }
    }
    lines.push(Line::default());
}

// ── Assistant Message ──
// Thin accent bar on the left — visual anchor that groups the response.
// Claude Code uses this exact pattern: subtle colored bar + indented content.
fn render_assistant(lines: &mut Vec<Line<'static>>, content: &str, max_width: usize) {
    let bar = Span::styled(format!("{}\u{2502} ", INDENT), Style::default().fg(theme::ACCENT_DIM));
    // Content width = terminal width minus the bar prefix (3 columns: " │ ")
    let content_w = max_width.saturating_sub(3);
    let md = wrap_lines(render_markdown(content), content_w);
    for line in md {
        let mut spans = vec![bar.clone()];
        spans.extend(line.spans);
        lines.push(Line::from(spans));
    }
}

// ── Tool Call ──
// Accent bar continues through tool calls for visual grouping within a turn.
fn render_tool_call(lines: &mut Vec<Line<'static>>, call: &ToolCall) {
    let bar = Span::styled(format!("{}\u{2502} ", INDENT), Style::default().fg(theme::ACCENT_DIM));
    let name = capitalize(&call.name);
    let detail = format_tool_detail(&call.name, &call.arguments);

    // Per-tool icon + color
    let (icon, icon_color) = match call.name.as_str() {
        "read_file" => ("\u{25b8}", theme::INFO),           // ▸
        "edit_file" => ("\u{25b8}", theme::TOOL_EDIT),  // ▸ green
        "write_file" => ("\u{25b8}", theme::TOOL_EDIT),
        "bash" => ("\u{25b8}", theme::TOOL_BASH),       // ▸ gold
        "grep" | "glob" | "web_search" => ("\u{25b8}", theme::TOOL_SEARCH),
        "web_fetch" => ("\u{25b8}", theme::TOOL_SEARCH),
        _ => ("\u{25b8}", theme::INFO),
    };

    lines.push(Line::from(vec![
        bar.clone(),
        Span::styled(format!("  {} ", icon), Style::default().fg(icon_color)),
        Span::styled(name, Style::default().fg(theme::INFO)),
        Span::styled(format!("  {}", detail), Style::default().fg(theme::TEXT_MUTED)),
    ]));

    // edit_file: show old_string preview
    if call.name == "edit_file" {
        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(old) = args.get("old_string").and_then(|v| v.as_str()) {
                let preview = old.lines().next().unwrap_or("");
                let display = if preview.chars().count() > 55 {
                    format!("{}...", preview.chars().take(52).collect::<String>())
                } else { preview.to_string() };
                if !display.trim().is_empty() {
                    lines.push(Line::from(vec![
                        bar.clone(),
                        Span::styled(format!("    \u{2192} {}", display.trim()), Style::default().fg(theme::TEXT_MUTED)),
                    ]));
                }
            }
        }
    }
}

// ── Tool Result ──
// Claude Code style: compact one-line summary with duration.
// Diff lines shown for edit_file only. `expanded` shows full output for recent calls.
fn render_tool_result(lines: &mut Vec<Line<'static>>, result: &ToolResult, expanded: bool) {
    let bar = Span::styled(format!("{}\u{2502} ", INDENT), Style::default().fg(theme::ACCENT_DIM));
    let (icon, color) = if result.success {
        ("\u{2713}", theme::SUCCESS)
    } else {
        ("\u{2717}", theme::ERROR)
    };

    let output = &result.output;

    // Extract duration if present (e.g., "(12ms)" or "(2.1s)" at the end)
    let duration = extract_duration(output);

    // Build a one-line summary based on output content.
    let summary = if output.is_empty() || output.trim().is_empty() {
        "(no output)".to_string()
    } else if output.starts_with("Edited ") {
        // Edit result: "Edited file (-3 +4 lines)." → keep as-is (already concise)
        output.lines().next().unwrap_or("").to_string()
    } else if output.starts_with("Created new file") {
        output.lines().next().unwrap_or("").to_string()
    } else if output.starts_with("Overwrote") {
        output.lines().next().unwrap_or("").to_string()
    } else if output.starts_with("Error:") || output.starts_with("error:") {
        // Error: show first line
        let first = output.lines().next().unwrap_or("Error");
        if first.chars().count() > 70 {
            format!("{}...", first.chars().take(67).collect::<String>())
        } else {
            first.to_string()
        }
    } else if output.starts_with("[BLOCKED") || output.starts_with("[Loop") || output.starts_with("[SKIPPED") {
        output.lines().next().unwrap_or("").to_string()
    } else {
        // Generic: count lines, show brief
        let line_count = output.lines().count();
        let first = output.lines().next().unwrap_or("");
        let first_short = if first.chars().count() > 50 {
            format!("{}...", first.chars().take(47).collect::<String>())
        } else {
            first.to_string()
        };
        if line_count > 1 {
            format!("{} ({} lines)", first_short, line_count)
        } else {
            first_short
        }
    };

    // Expand/collapse indicator: ▾ for expanded, ▸ for collapsed
    let expand_indicator = if expanded { "\u{25be}" } else { "\u{25b8}" };

    // Main result line: indicator + icon + summary + duration
    let mut spans = vec![
        bar.clone(),
        Span::styled(format!(" {} ", expand_indicator), Style::default().fg(theme::TEXT_MUTED)),
        Span::styled(format!("{} ", icon), Style::default().fg(color)),
        Span::styled(summary, Style::default().fg(theme::TEXT_MUTED)),
    ];
    if !duration.is_empty() {
        spans.push(Span::styled(format!(" {}", duration), Style::default().fg(theme::TEXT_MUTED)));
    }
    lines.push(Line::from(spans));

    // For edit results: show unified diff with line numbers, colored bg — max 15 lines
    if result.success && (output.contains("\n- ") || output.contains("\n+ ")) {
        // Collect diff lines with their original line numbers from the output.
        // The output format from edit_file is:
        //   "Edited file (-N +M lines).\n- old1\n- old2\n+ new1\n+ new2"
        let diff_lines: Vec<&str> = output.lines()
            .filter(|l| {
                let t = l.trim();
                t.starts_with("- ") || t.starts_with("+ ")
            })
            .collect();

        let total_diff = diff_lines.len();
        let max_diff_lines = 15;
        let show_count = total_diff.min(max_diff_lines);

        // Assign gutter line numbers: removed lines count from 1, added lines
        // count from 1 separately (like unified diff old/new line numbers).
        let mut old_lineno: usize = 1;
        let mut new_lineno: usize = 1;

        for (i, diff_line) in diff_lines.iter().enumerate() {
            if i >= show_count { break; }
            let trimmed = diff_line.trim();

            let (prefix, gutter_num, fg_color, bg_color) = if trimmed.starts_with("- ") {
                let n = old_lineno;
                old_lineno += 1;
                ("-", n, theme::DIFF_REMOVE, theme::DIFF_REMOVE_BG)
            } else {
                let n = new_lineno;
                new_lineno += 1;
                ("+", n, theme::DIFF_ADD, theme::DIFF_ADD_BG)
            };

            // Content after the "- " or "+ " prefix
            let content = &trimmed[2..];
            let display = if content.chars().count() > 60 {
                format!("{}...", content.chars().take(57).collect::<String>())
            } else {
                content.to_string()
            };

            // Gutter: right-aligned 4-char line number
            let gutter = format!("{:>4}", gutter_num);

            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled(
                    format!("  {}", gutter),
                    Style::default().fg(theme::DIFF_GUTTER),
                ),
                Span::styled(
                    format!(" {} ", prefix),
                    Style::default().fg(fg_color).bg(bg_color),
                ),
                Span::styled(
                    format!("{} ", display),
                    Style::default().fg(fg_color).bg(bg_color),
                ),
            ]));
        }

        if total_diff > max_diff_lines {
            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled(
                    format!("        ... ({} more lines)", total_diff - max_diff_lines),
                    Style::default().fg(theme::TEXT_MUTED),
                ),
            ]));
        }
    }

    // For errors: show a few detail lines
    if !result.success && output.lines().count() > 1 {
        let mut shown = 0;
        for line in output.lines().skip(1) {
            if shown >= 3 { break; }
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            let display = if trimmed.chars().count() > 65 {
                format!("{}...", trimmed.chars().take(62).collect::<String>())
            } else { trimmed.to_string() };
            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled(format!("    {}", display), Style::default().fg(theme::ERROR)),
            ]));
            shown += 1;
        }
    }

    // Expanded view: show up to 30 lines of full tool output for recent calls.
    // Skip if this is a diff result (already shown above) or error (shown above).
    let is_diff_output = result.success && (output.contains("\n- ") || output.contains("\n+ "));
    let is_error = !result.success;
    if expanded && !is_diff_output && !is_error {
        let output_lines: Vec<&str> = output.lines().collect();
        // Skip the first line (already shown in summary)
        if output_lines.len() > 1 {
            let max_expanded = 30;
            let show_lines = &output_lines[1..(output_lines.len()).min(1 + max_expanded)];
            for ol in show_lines {
                let trimmed = ol.trim_end();
                if trimmed.is_empty() { continue; }
                let display = if trimmed.chars().count() > 70 {
                    format!("{}...", trimmed.chars().take(67).collect::<String>())
                } else {
                    trimmed.to_string()
                };
                lines.push(Line::from(vec![
                    bar.clone(),
                    Span::styled(
                        format!("    {}", display),
                        Style::default().fg(theme::TEXT_SECONDARY),
                    ),
                ]));
            }
            if output_lines.len() > 1 + max_expanded {
                lines.push(Line::from(vec![
                    bar.clone(),
                    Span::styled(
                        format!("    ... ({} more lines)", output_lines.len() - 1 - max_expanded),
                        Style::default().fg(theme::TEXT_MUTED),
                    ),
                ]));
            }
        }
    }
}

/// Extract duration string from tool output (e.g., "(12ms)", "(2.1s)").
fn extract_duration(output: &str) -> String {
    // Look for (Nms) or (N.Ns) pattern near the end
    let last_line = output.lines().last().unwrap_or("");
    if let Some(start) = last_line.rfind('(') {
        let candidate = &last_line[start..];
        if candidate.contains("ms)") || candidate.contains("s)") {
            return candidate.trim_end().to_string();
        }
    }
    String::new()
}
// ── Approval ──
fn render_approval(lines: &mut Vec<Line<'static>>, call: &ToolCall) {
    let name = capitalize(&call.name);
    let border = Style::default().fg(theme::WARNING);
    let key_label = Style::default().fg(theme::TEXT_MUTED);

    lines.push(Line::default());
    // Top border
    lines.push(Line::from(vec![
        Span::styled(format!("{}\u{256d}\u{2500}\u{2500} ", TOOL_INDENT), border),
        Span::styled(format!("\u{26a1} {} ", name), Style::default().fg(theme::WARNING).add_modifier(Modifier::BOLD)),
        Span::styled("\u{2500}".repeat(24), Style::default().fg(theme::SEPARATOR)),
    ]));

    // Args
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let val = match v {
                    serde_json::Value::String(s) => {
                        if k == "content" {
                            let lc = s.lines().count();
                            let preview: String = s.lines().take(4).collect::<Vec<_>>().join("\n");
                            if lc > 4 { format!("{}\n{}\u{2502}   \u{2026} ({} lines)", preview, TOOL_INDENT, lc) }
                            else { preview }
                        } else if s.chars().count() > 45 {
                            s.chars().take(42).collect::<String>() + "..."
                        } else { s.clone() }
                    }
                    other => other.to_string(),
                };
                for (i, vline) in val.lines().enumerate() {
                    if i == 0 {
                        lines.push(Line::from(vec![
                            Span::styled(format!("{}\u{2502} ", TOOL_INDENT), border),
                            Span::styled(format!("{}: ", k), Style::default().fg(theme::TEXT_SECONDARY)),
                            Span::styled(vline.to_string(), Style::default().fg(theme::TEXT_PRIMARY)),
                        ]));
                    } else {
                        lines.push(Line::from(vec![
                            Span::styled(format!("{}\u{2502}   ", TOOL_INDENT), border),
                            Span::styled(vline.to_string(), Style::default().fg(theme::TEXT_SECONDARY)),
                        ]));
                    }
                }
            }
        }
    }

    // Bottom border
    lines.push(Line::from(Span::styled(
        format!("{}\u{2570}{}", TOOL_INDENT, "\u{2500}".repeat(32)),
        Style::default().fg(theme::SEPARATOR),
    )));

    // Action buttons with prominent prompt
    lines.push(Line::from(vec![
        Span::raw(format!("{}", TOOL_INDENT)),
        Span::styled(" \u{25b6} ", Style::default().fg(theme::WARNING).add_modifier(Modifier::BOLD)),
        Span::styled("Waiting for approval: ", Style::default().fg(theme::WARNING)),
        Span::styled(" Y ", Style::default().fg(theme::TEXT_ON_ACCENT).bg(theme::SUCCESS).add_modifier(Modifier::BOLD)),
        Span::styled(" Allow  ", key_label),
        Span::styled(" A ", Style::default().fg(theme::TEXT_ON_ACCENT).bg(theme::INFO).add_modifier(Modifier::BOLD)),
        Span::styled(" Always  ", key_label),
        Span::styled(" N ", Style::default().fg(theme::TEXT_ON_ACCENT).bg(theme::ERROR).add_modifier(Modifier::BOLD)),
        Span::styled(" Deny", key_label),
    ]));
    lines.push(Line::default());
}

// ── Helpers ──

fn capitalize(name: &str) -> String {
    name.split('_').map(|w| {
        let mut c = w.chars();
        match c.next() {
            None => String::new(),
            Some(ch) => ch.to_uppercase().to_string() + c.as_str(),
        }
    }).collect::<Vec<_>>().join(" ")
}

#[allow(dead_code)]
fn build_turn_stats(elapsed_secs: Option<u64>, tokens: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(secs) = elapsed_secs {
        if secs >= 60 { parts.push(format!("{}m{}s", secs / 60, secs % 60)); }
        else { parts.push(format!("{}s", secs)); }
    }
    if tokens > 0 {
        parts.push(format!("{} tokens", format_compact_tokens(tokens)));
        if let Some(secs) = elapsed_secs {
            if secs > 0 {
                let speed = tokens as f64 / secs as f64;
                if speed >= 1.0 { parts.push(format!("{:.0} t/s", speed)); }
            }
        }
    }
    if parts.is_empty() { String::new() }
    else { format!("  {}", parts.join(" \u{00b7} ")) }
}

#[allow(dead_code)]
fn format_compact_tokens(n: usize) -> String {
    if n < 1000 { format!("{}", n) }
    else if n < 1_000_000 { format!("{:.1}k", n as f64 / 1000.0) }
    else { format!("{:.1}M", n as f64 / 1_000_000.0) }
}

fn format_tool_detail(tool_name: &str, args_json: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    match tool_name {
        "read_file" => {
            let path = shorten_path(args.get("file_path").and_then(|v| v.as_str()).unwrap_or(""));
            let mut d = path;
            if let Some(offset) = args.get("offset").and_then(|v| v.as_u64()) {
                d.push_str(&format!(" L{}", offset));
                if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                    d.push_str(&format!("-{}", offset + limit));
                }
            }
            d
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
            format!("{} (-{} +{})", path, old_lines, new_lines)
        }
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let first = cmd.lines().next().unwrap_or(cmd);
            if first.chars().count() > 55 { first.chars().take(52).collect::<String>() + "..." }
            else { first.to_string() }
        }
        "list_directory" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            shorten_path(path)
        }
        "grep" | "glob" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("\"{}\" in {}", pattern, shorten_path(path))
        }
        "web_search" => {
            args.get("query").and_then(|v| v.as_str()).unwrap_or("").to_string()
        }
        "web_fetch" => {
            args.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string()
        }
        _ => format_args_oneline(args_json),
    }
}

fn shorten_path(path: &str) -> String {
    let sep = if path.contains('\\') { '\\' } else { '/' };
    let parts: Vec<&str> = path.rsplitn(3, sep).collect();
    match parts.len() {
        0 | 1 => path.to_string(),
        2 => format!("{}{}{}", parts[1], sep, parts[0]),
        _ => format!("...{}{}{}{}", sep, parts[1], sep, parts[0]),
    }
}

fn format_args_oneline(args_json: &str) -> String {
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(args_json) {
        if let Some(obj) = args.as_object() {
            let parts: Vec<String> = obj.iter().map(|(k, v)| {
                let val = match v {
                    serde_json::Value::String(s) => {
                        if s.chars().count() > 25 { s.chars().take(22).collect::<String>() + "..." }
                        else { s.clone() }
                    }
                    other => {
                        let s = other.to_string();
                        if s.chars().count() > 25 { s.chars().take(22).collect::<String>() + "..." } else { s }
                    }
                };
                format!("{}={}", k, val)
            }).collect();
            return parts.join(" ");
        }
    }
    String::new()
}

/// Trim trailing incomplete markdown tokens from a streaming buffer.
/// Handles: incomplete code fences (`, ``, ```), unclosed bold/italic (* or **),
/// and trailing backslash escapes.
fn trim_incomplete_markdown(buf: &str) -> String {
    let mut s = buf.to_string();

    // 1. Trailing backtick fence: if the buffer ends with 1-3 backticks on the
    //    last line (possibly after a newline), it's likely an incomplete code fence.
    //    Trim them so the parser doesn't misinterpret.
    let last_line = s.rsplit('\n').next().unwrap_or(&s);
    let trimmed = last_line.trim_end();
    if trimmed.ends_with("```") || trimmed.ends_with("``") {
        // Could be a complete closing fence or an incomplete opening.
        // If the number of ``` fences in the full text is odd, it's incomplete.
        let fence_count = s.matches("```").count();
        if fence_count % 2 != 0 {
            // Odd number — the last fence is unclosed. Remove it.
            if let Some(pos) = s.rfind("```") {
                s.truncate(pos);
            }
        }
    } else if trimmed.ends_with('`') && !trimmed.ends_with("``") {
        // Single trailing backtick — might be start of inline code.
        // Count all backticks; if odd, the last one is unclosed.
        let tick_count = s.chars().filter(|c| *c == '`').count();
        if tick_count % 2 != 0 {
            // Remove trailing backtick.
            s.truncate(s.trim_end_matches('`').len());
        }
    }

    // 2. Trailing unclosed bold/italic markers.
    let end = s.trim_end();
    if end.ends_with("**") || end.ends_with('*') {
        let star_count = s.chars().filter(|c| *c == '*').count();
        if star_count % 2 != 0 {
            s.truncate(s.trim_end_matches('*').len());
        }
    }

    s
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
                if let Some(t) = text { count += render_markdown(t).len(); }
                count += tool_calls.len();
            }
            MessageContent::ToolResult(_) => count += 1,
            MessageContent::ToolResultRef(_) => count += 1,
        }
    }
    if let Some(ref buffer) = conversation.stream_buffer {
        count += render_markdown(buffer).len() + 1;
    }
    count
}
