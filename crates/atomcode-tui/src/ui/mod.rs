pub mod chat_panel;
pub mod input_box;
pub mod markdown;
pub mod provider_panel;
pub mod slash_menu;
pub mod status_bar;
pub mod welcome;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use crate::app::App;

pub fn render(frame: &mut Frame, app: &App) {
    use crate::app::AppMode;

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
    let input_height = input_box::height(&app.input, terminal_height);

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

    if show_welcome {
        welcome::render(frame, chunks[1], app);
    } else {
        let waiting_approval = match &app.mode {
            AppMode::WaitingApproval(call) => Some(call),
            _ => None,
        };
        chat_panel::render(frame, chunks[1], &app.conversation, app.scroll_offset, app.at_bottom, waiting_approval);
    }

    input_box::render(frame, chunks[2], &app.input, app.mode.is_streaming_or_executing());

    // Render slash menu as overlay above input box
    if app.slash_menu.visible {
        slash_menu::render(frame, chunks[2], &app.slash_menu);
    }
}
