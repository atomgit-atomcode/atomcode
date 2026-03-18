pub mod chat_panel;
pub mod input_box;
pub mod markdown;
pub mod status_bar;
pub mod welcome;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::Frame;

use crate::app::App;

pub fn render(frame: &mut Frame, app: &App) {
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
        chat_panel::render(frame, chunks[1], &app.conversation, app.scroll_offset);
    }

    input_box::render(frame, chunks[2], &app.input, app.mode == crate::app::AppMode::Streaming);
}
