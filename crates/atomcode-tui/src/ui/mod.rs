pub mod chat_panel;
pub mod input_box;
pub mod markdown;
pub mod model_selector;
pub mod provider_panel;
pub mod slash_menu;
pub mod status_bar;
pub mod welcome;

use ratatui::layout::{Constraint, Direction, Layout, Position};
use ratatui::style::Color;
use ratatui::Frame;

use crate::app::{App, TextSelection};

// Note: render takes &mut App to update render cache. This is fine since
// render is the only place that reads the cache, called from the main loop.

pub fn render(frame: &mut Frame, app: &mut App) {
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
    let input_height = input_box::height(&app.input, terminal_height, !app.attached_files.is_empty());

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

    if show_welcome && app.mode.is_normal() {
        welcome::render(frame, chunks[1], app);
    } else {
        let turn_elapsed = app.turn_start.map(|t| t.elapsed().as_secs());
        let turn_seed = app.conversation.messages.len();
        let rendered_scroll = chat_panel::render(
            frame, chunks[1], &app.conversation,
            app.scroll_offset, app.at_bottom, &app.mode, app.tick_count,
            app.turn_tokens, turn_elapsed, turn_seed,
            app.current_step_count, &app.executing_tool_info,
            &mut app.render_cache, &mut app.render_cache_msg_count,
        );
        app.last_rendered_scroll = rendered_scroll;
        app.last_viewport_height = chunks[1].height;
    }

    input_box::render(frame, chunks[2], &app.input, app.mode.is_streaming_or_executing(), app.suggestion.as_deref(), &app.attached_files);

    // Render slash menu as overlay above input box
    if app.slash_menu.visible {
        slash_menu::render(frame, chunks[2], &app.slash_menu);
    }

    // Model selector popup (overlay)
    if matches!(app.mode, crate::app::AppMode::ModelSelector) {
        model_selector::render(frame, frame.area(), app);
    }

    // Render text selection highlight (inverted colors overlay)
    if app.selection.has_selection || app.selection.dragging {
        render_selection_highlight(frame, &app.selection);
    }
}

/// Render the text selection highlight by inverting cell colors in the selected range.
fn render_selection_highlight(frame: &mut Frame, sel: &TextSelection) {
    let ((start_col, start_row), (end_col, end_row)) = sel.normalized();
    let buf = frame.buffer_mut();
    let width = buf.area.width;

    for row in start_row..=end_row {
        let col_start = if row == start_row { start_col } else { 0 };
        let col_end = if row == end_row { end_col } else { width };

        for col in col_start..col_end {
            if let Some(cell) = buf.cell_mut(Position::new(col, row)) {
                let fg = cell.fg;
                let bg = cell.bg;
                // Invert: swap fg/bg, using sensible defaults for Reset
                let new_bg = if fg == Color::Reset { Color::White } else { fg };
                let new_fg = if bg == Color::Reset { Color::Black } else { bg };
                cell.set_fg(new_fg);
                cell.set_bg(new_bg);
            }
        }
    }
}
