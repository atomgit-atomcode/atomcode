use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use atomcode_core::config::Config;

use crate::provider_manager::{
    AddStep, EditField, ManagerState, ProviderManager, PROVIDER_TYPES,
};
use crate::ui::theme;

pub fn render(frame: &mut Frame, area: Rect, mgr: &ProviderManager, config: &Config) {
    // Clear the whole area
    frame.render_widget(Clear, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(5),    // Provider list / form
            Constraint::Length(3), // Status / input bar
        ])
        .split(area);

    match &mgr.state {
        ManagerState::List => render_list(frame, chunks[0], mgr, config),
        ManagerState::Adding(step) => render_add_form(frame, chunks[0], mgr, step),
        ManagerState::ConfirmDelete => {
            render_list(frame, chunks[0], mgr, config);
        }
        ManagerState::Editing(field) => render_edit_form(frame, chunks[0], mgr, config, field),
    }

    render_bottom_bar(frame, chunks[1], mgr);
}

fn render_list(frame: &mut Frame, area: Rect, mgr: &ProviderManager, config: &Config) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::default());

    if mgr.provider_names.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No providers configured.",
            Style::default().fg(theme::text_muted()),
        )));
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  Press [a] to add one.",
            Style::default().fg(theme::text_secondary()),
        )));
    } else {
        for (i, name) in mgr.provider_names.iter().enumerate() {
            let is_selected = i == mgr.selected;
            let is_default = *name == config.default_provider;

            let marker = if is_default { "●" } else { " " };
            let provider = config.providers.get(name);
            let ptype = provider.map(|p| p.provider_type.as_str()).unwrap_or("?");
            let model = provider.map(|p| p.model.as_str()).unwrap_or("?");

            if is_selected {
                // Selected: accent background, white text
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {} ", marker),
                        Style::default().fg(theme::text_on_accent()).bg(theme::accent()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<14}", name),
                        Style::default().fg(theme::text_on_accent()).bg(theme::accent()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<10}", ptype),
                        Style::default().fg(theme::text_on_accent()).bg(theme::accent()),
                    ),
                    Span::styled(
                        model.to_string(),
                        Style::default().fg(theme::text_on_accent()).bg(theme::accent()),
                    ),
                ]));
            } else {
                // Not selected: normal colors with explicit background
                let marker_style = if is_default {
                    Style::default().fg(theme::success()).bg(theme::bg_elevated()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::text_muted()).bg(theme::bg_elevated())
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {} ", marker), marker_style),
                    Span::styled(
                        format!("{:<14}", name),
                        Style::default().fg(theme::accent()).bg(theme::bg_elevated()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:<10}", ptype),
                        Style::default().fg(theme::text_secondary()).bg(theme::bg_elevated()),
                    ),
                    Span::styled(
                        model.to_string(),
                        Style::default().fg(theme::text_muted()).bg(theme::bg_elevated()),
                    ),
                ]));
            }
        }
    }

    lines.push(Line::default());

    // Help line
    if mgr.state == ManagerState::ConfirmDelete {
        if let Some(name) = mgr.selected_name() {
            lines.push(Line::from(vec![
                Span::styled("  Delete '", Style::default().fg(theme::error())),
                Span::styled(
                    name.to_string(),
                    Style::default().fg(theme::error()).add_modifier(Modifier::BOLD),
                ),
                Span::styled("'? ", Style::default().fg(theme::error())),
                Span::styled("[y]es / [n]o", Style::default().fg(theme::warning())),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("  [a]", Style::default().fg(theme::accent())),
            Span::styled("dd  ", Style::default().fg(theme::text_muted())),
            Span::styled("[e]", Style::default().fg(theme::accent())),
            Span::styled("dit  ", Style::default().fg(theme::text_muted())),
            Span::styled("[d]", Style::default().fg(theme::accent())),
            Span::styled("elete  ", Style::default().fg(theme::text_muted())),
            Span::styled("[s]", Style::default().fg(theme::accent())),
            Span::styled("et default  ", Style::default().fg(theme::text_muted())),
            Span::styled("[q]", Style::default().fg(theme::accent())),
            Span::styled("uit", Style::default().fg(theme::text_muted())),
        ]));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::border()))
        .title(Span::styled(
            " Provider Manager ",
            Style::default()
                .fg(theme::text_primary())
                .add_modifier(Modifier::BOLD),
        ));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn render_add_form(frame: &mut Frame, area: Rect, mgr: &ProviderManager, step: &AddStep) {
    // OAuthPending has its own full render — skip the normal field-by-field form.
    if *step == AddStep::OAuthPending {
        let mut lines: Vec<Line<'static>> = Vec::new();
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "  AtomGit Login",
            Style::default().fg(theme::text_primary()).add_modifier(Modifier::BOLD),
        )));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Opening browser for AtomGit authorization...",
        Style::default().fg(theme::success()),
    )));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Waiting for callback on port 8765",
        Style::default().fg(theme::info()),
    )));
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Press Esc to cancel",
        Style::default().fg(theme::text_muted()),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::accent()))
        .title(Span::styled(
            " Add Provider ",
            Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD),
        ));
    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
        return;
    }

    let mut lines: Vec<Line<'static>> = Vec::new();

    let dim = Style::default().fg(theme::text_muted());
    let label = Style::default().fg(theme::text_secondary());
    let value = Style::default().fg(theme::text_primary());
    let active_label = Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD);

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Add New Provider",
        Style::default()
            .fg(theme::text_primary())
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

    // Show completed fields
    let name_style = if *step == AddStep::Name { active_label } else { label };
    let name_val = if *step == AddStep::Name {
        format!("{}_", mgr.input_buf)
    } else if mgr.new_name.is_empty() {
        "...".to_string()
    } else {
        mgr.new_name.clone()
    };
    lines.push(Line::from(vec![
        Span::styled("  Name:     ", name_style),
        Span::styled(name_val, value),
    ]));

    if *step as usize >= AddStep::Type as usize {
        let type_style = if *step == AddStep::Type { active_label } else { label };
        if *step == AddStep::Type {
            lines.push(Line::from(Span::styled("  Type:     (select with Up/Down, Enter to confirm)", type_style)));
            for (i, (_, desc)) in PROVIDER_TYPES.iter().enumerate() {
                let is_sel = i == mgr.type_selected;
                let prefix = if is_sel { " ▸ " } else { "   " };
                let style = if is_sel {
                    Style::default().fg(theme::accent()).add_modifier(Modifier::BOLD)
                } else {
                    dim
                };
                lines.push(Line::from(Span::styled(
                    format!("    {}  {}", prefix, desc),
                    style,
                )));
            }
        } else {
            lines.push(Line::from(vec![
                Span::styled("  Type:     ", type_style),
                Span::styled(mgr.new_type.clone(), value),
            ]));
        }
    }

    if *step as usize >= AddStep::ApiKey as usize {
        let style = if *step == AddStep::ApiKey { active_label } else { label };
        let val = if *step == AddStep::ApiKey {
            let display = if mgr.input_buf.is_empty() {
                "(optional, press Enter to skip)".to_string()
            } else {
                format!("{}*", mgr.input_buf.chars().take(3).collect::<String>())
            };
            display
        } else if mgr.new_api_key.is_empty() {
            "(none)".to_string()
        } else {
            format!("{}***", mgr.new_api_key.chars().take(3).collect::<String>())
        };
        lines.push(Line::from(vec![
            Span::styled("  API Key:  ", style),
            Span::styled(val, if *step == AddStep::ApiKey { value } else { dim }),
        ]));
    }

    if *step as usize >= AddStep::BaseUrl as usize {
        let style = if *step == AddStep::BaseUrl { active_label } else { label };
        let val = if *step == AddStep::BaseUrl {
            format!("{}_", mgr.input_buf)
        } else if mgr.new_base_url.is_empty() {
            "(default)".to_string()
        } else {
            mgr.new_base_url.clone()
        };
        lines.push(Line::from(vec![
            Span::styled("  Base URL: ", style),
            Span::styled(val, if *step == AddStep::BaseUrl { value } else { dim }),
        ]));
    }

    if *step as usize >= AddStep::Model as usize {
        let style = if *step == AddStep::Model { active_label } else { label };
        let val = format!("{}_", mgr.input_buf);
        lines.push(Line::from(vec![
            Span::styled("  Model:    ", style),
            Span::styled(val, value),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "  Enter to continue · Esc to go back",
        dim,
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::accent()))
        .title(Span::styled(
            " Add Provider ",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn render_edit_form(
    frame: &mut Frame,
    area: Rect,
    mgr: &ProviderManager,
    config: &Config,
    field: &EditField,
) {
    let name = mgr.selected_name().unwrap_or("?");
    let provider = config.providers.get(name);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let dim = Style::default().fg(theme::text_muted());
    let label = Style::default().fg(theme::text_secondary());
    let value = Style::default().fg(theme::text_primary());

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("  Editing: ", Style::default().fg(theme::text_primary())),
        Span::styled(
            name.to_string(),
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::default());

    let fields = ["Type", "API Key", "Base URL", "Model"];
    let values: Vec<String> = if let Some(p) = provider {
        vec![
            PROVIDER_TYPES
                .iter()
                .find(|(t, _)| *t == p.provider_type)
                .map(|(_, d)| d.to_string())
                .unwrap_or(p.provider_type.clone()),
            p.api_key
                .as_ref()
                .map(|k| format!("{}***", k.chars().take(3).collect::<String>()))
                .unwrap_or("(none)".to_string()),
            p.base_url.clone().unwrap_or("(default)".to_string()),
            p.model.clone(),
        ]
    } else {
        vec!["?".into(); 4]
    };

    let is_field_input = matches!(field, EditField::ApiKey | EditField::BaseUrl | EditField::Model)
        && *field != EditField::Type;

    for (i, (f, v)) in fields.iter().zip(values.iter()).enumerate() {
        let is_sel = i == mgr.edit_field_selected;
        let editing_this = is_field_input && is_sel;

        let prefix = if is_sel { " ▸" } else { "  " };
        let f_style = if is_sel {
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD)
        } else {
            label
        };

        let display_val = if editing_this {
            format!("{}_", mgr.input_buf)
        } else if is_sel && i == 0 {
            // Type field - show current with hint
            format!("{} (Enter to cycle)", v)
        } else {
            v.clone()
        };

        lines.push(Line::from(vec![
            Span::styled(format!("{}  {:<10}", prefix, f), f_style),
            Span::styled(display_val, if is_sel { value } else { dim }),
        ]));
    }

    lines.push(Line::default());
    let hint = if is_field_input {
        "  Enter to save · Esc to cancel"
    } else {
        "  Up/Down to select · Enter to edit · Esc to go back"
    };
    lines.push(Line::from(Span::styled(hint, dim)));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(theme::warning()))
        .title(Span::styled(
            " Edit Provider ",
            Style::default()
                .fg(theme::warning())
                .add_modifier(Modifier::BOLD),
        ));

    let paragraph = Paragraph::new(lines).block(block);
    frame.render_widget(paragraph, area);
}

fn render_bottom_bar(frame: &mut Frame, area: Rect, mgr: &ProviderManager) {
    let msg = mgr
        .message
        .as_deref()
        .unwrap_or("");
    let style = if msg.contains("Delete") || msg.contains("Cannot") {
        Style::default().fg(theme::error())
    } else if msg.contains("added") || msg.contains("set as") || msg.contains("Updated") {
        Style::default().fg(theme::success())
    } else {
        Style::default().fg(theme::text_secondary())
    };

    let paragraph = Paragraph::new(Line::from(Span::styled(
        format!("  {}", msg),
        style,
    )));
    frame.render_widget(paragraph, area);
}
