pub mod chat_panel;
pub mod input_box;
pub mod issue_input;
pub mod markdown;
pub mod model_selector;
pub mod provider_panel;
pub mod session_selector;
pub mod slash_menu;
pub mod status_bar;
pub mod theme;
pub mod welcome;

use std::path::PathBuf;
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, TextSelection};

/// Convert snake_case tool name to "Snake Case" for display.
fn format_tool_display_name(name: &str) -> String {
    name.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().to_string() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// Note: render takes &mut App to update render cache. This is fine since
// render is the only place that reads the cache, called from the main loop.

pub fn render(frame: &mut Frame, app: &mut App) {
    // Welcome screen takes over the full display (no status bar, no input)
    if app.mode.is_welcome() {
        welcome::render_setup(frame, frame.area(), &app.welcome_state);
        return;
    }

    // Provider manager takes over the full screen (except status bar)
    if app.mode.is_provider_manager() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
            ])
            .split(frame.area());

        status_bar::render(frame, chunks[0], app);
        if let Some(ref mgr) = app.provider_mgr {
            provider_panel::render(frame, chunks[1], mgr, &app.config);
        }
        return;
    }

    let terminal_height = frame.area().height;
    let terminal_width = frame.area().width;
    let input_height = input_box::height(&app.input, terminal_height, terminal_width, !app.attached_files.is_empty(), app.pasted_blocks.len());

    let show_welcome = app.conversation.messages.is_empty()
        && app.conversation.stream_buffer.is_none();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(input_height),
        ])
        .split(frame.area());

    status_bar::render(frame, chunks[0], app);

    // Session selector mode: render inline in chat area
    if matches!(app.mode, crate::app::AppMode::SessionSelector) {
        if let Some((ref sessions, selected)) = app.session_selector {
            session_selector::render(frame, chunks[1], sessions, selected, &app.session_selector_query);
        }
        input_box::render(frame, chunks[2], &app.input, false, None, &app.attached_files, &app.pasted_blocks);
        return;
    }

    // Issue input mode: render issue form in chat area
    if let crate::app::AppMode::IssueInput(ref state) = app.mode {
        issue_input::render(frame, chunks[1], state);
        input_box::render(frame, chunks[2], &app.input, false, None, &app.attached_files, &app.pasted_blocks);
        return;
    }

    if show_welcome && app.mode.is_normal() {
        welcome::render(frame, chunks[1], app);
    } else {
        let turn_elapsed = app.turn_start.map(|t| t.elapsed().as_secs());
        let turn_seed = app.conversation.messages.len();
        let llm_wait_ms = app.llm_call_start.map(|s| s.elapsed().as_millis() as u64);
        // Fallback: when ToolCallStreaming has fired but ToolCallStarted hasn't yet,
        // the args are still arriving. Show the tool name in the spinner label so the
        // UI reacts the moment streaming starts.
        let streaming_label;
        let tool_info_ref: &str = if !app.executing_tool_info.is_empty() {
            &app.executing_tool_info
        } else if let Some(ref name) = app.streaming_tool_name {
            let hint = if app.streaming_tool_hint.is_empty() {
                String::new()
            } else {
                format!(" {}", app.streaming_tool_hint)
            };
            streaming_label = format!("{}{}\u{2026}", format_tool_display_name(name), hint);
            &streaming_label
        } else {
            ""
        };
        let (total_lines, rendered_scroll) = chat_panel::render(
            frame, chunks[1], &app.conversation,
            app.scroll_offset, app.at_bottom, &app.mode, app.tick_count,
            app.turn_tokens, turn_elapsed, turn_seed,
            app.current_step_count, tool_info_ref,
            app.first_token_ms, llm_wait_ms, &app.last_completed_tool,
            app.last_turn_duration, app.current_step_count,
            app.streaming_tool_name.as_deref(),
            &app.streaming_tools,
            &app.streaming_tool_hint,
            &app.pending_appends,
            &mut app.render_cache, &mut app.render_cache_msg_count,
        );
        app.last_rendered_scroll = rendered_scroll;
        app.last_viewport_height = chunks[1].height;
        app.last_total_lines = total_lines;
    }

    input_box::render(frame, chunks[2], &app.input, app.mode.is_streaming_or_executing(), app.suggestion.as_deref(), &app.attached_files, &app.pasted_blocks);

    // Render slash menu as overlay above input box
    if app.slash_menu.visible {
        slash_menu::render(frame, chunks[2], &app.slash_menu);
    }

    // Approval overlay — rendered at bottom of chat area when waiting for user
    if let crate::app::AppMode::WaitingApproval(ref call) = app.mode {
        let mut lines: Vec<Line<'static>> = Vec::new();
        chat_panel::render_approval(&mut lines, call);
        let height = (lines.len() as u16).min(chunks[1].height);
        let y = chunks[1].y + chunks[1].height - height;
        let overlay_area = Rect::new(chunks[1].x, y, chunks[1].width, height);
        frame.render_widget(Clear, overlay_area);
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme::bg_surface())),
            overlay_area,
        );
    }

    // Model selector popup (overlay)
    if matches!(app.mode, crate::app::AppMode::ModelSelector) {
        model_selector::render(frame, frame.area(), app);
    }

    // Directory selector popup (overlay)
    if let Some(selected) = app.dir_selector {
        render_dir_selector(frame, frame.area(), &app.recent_dirs, selected, &app.working_dir);
    }

    // Render text selection highlight — clipped to chat area only
    if app.selection.has_selection || app.selection.dragging {
        render_selection_highlight(frame, &app.selection, chunks[1]);
    }
}

/// Render selection highlight strictly clipped to the chat area.
fn render_selection_highlight(frame: &mut Frame, sel: &TextSelection, chat_area: Rect) {
    let ((start_col, start_row), (end_col, end_row)) = sel.normalized();
    let buf = frame.buffer_mut();

    // Strict boundaries — nothing outside the chat rect
    let area_left = chat_area.x;
    let area_right = chat_area.x + chat_area.width;
    let area_top = chat_area.y;
    let area_bottom = chat_area.y + chat_area.height;

    let sel_bg = theme::rgb(40, 80, 160);  // Selection blue - works on both themes
    let sel_fg = theme::text_primary();

    // Clamp row range to chat area — leave 1-row gap before input box
    let row_start = start_row.max(area_top);
    let row_end = end_row.min(area_bottom.saturating_sub(2));

    for row in row_start..=row_end {
        let col_start = if row == start_row { start_col.max(area_left) } else { area_left };
        let col_end = if row == end_row { end_col.min(area_right.saturating_sub(1)) } else { area_right.saturating_sub(1) };

        for col in col_start..=col_end {
            if let Some(cell) = buf.cell_mut(Position::new(col, row)) {
                cell.set_fg(sel_fg);
                cell.set_bg(sel_bg);
            }
        }
    }
}

/// Render a floating directory picker popup.
fn render_dir_selector(frame: &mut Frame, area: Rect, dirs: &[PathBuf], selected: usize, current: &PathBuf) {
    let height = (dirs.len() as u16 + 4).min(area.height.saturating_sub(4));
    let width = 60u16.min(area.width.saturating_sub(8));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::accent_dim()))
        .title(Span::styled(" Recent Projects ", Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)))
        .style(Style::default().bg(theme::bg_elevated()));

    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let mut lines: Vec<Line> = Vec::new();
    // Current dir
    let cur_short = shorten_dir(&current.to_string_lossy());
    lines.push(Line::from(vec![
        Span::styled("  current: ", Style::default().fg(theme::text_muted())),
        Span::styled(cur_short, Style::default().fg(theme::info())),
    ]));
    lines.push(Line::default());

    for (i, dir) in dirs.iter().enumerate() {
        let display = shorten_dir(&dir.to_string_lossy());
        let is_selected = i == selected;
        let is_current = dir == current;

        let (prefix, style) = if is_selected {
            (" \u{25b8} ", Style::default().fg(theme::text_on_accent()).bg(theme::brand_bg()))
        } else {
            ("   ", Style::default().fg(theme::text_secondary()))
        };

        let mut spans = vec![Span::styled(prefix, style)];
        spans.push(Span::styled(display, style));
        if is_current {
            spans.push(Span::styled("  (current)", Style::default().fg(theme::success())));
        }
        lines.push(Line::from(spans));
    }

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

fn shorten_dir(path: &str) -> String {
    let home = dirs::home_dir()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    let shortened = if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    };
    if shortened.len() > 45 {
        let parts: Vec<&str> = shortened.rsplitn(3, '/').collect();
        if parts.len() >= 3 { format!(".../{}/{}", parts[1], parts[0]) }
        else { shortened }
    } else {
        shortened
    }
}
