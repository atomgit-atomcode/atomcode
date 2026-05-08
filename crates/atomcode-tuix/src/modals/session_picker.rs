// crates/atomcode-tuix/src/modals/session_picker.rs
//
// `/resume` modal — prior-sessions picker.
//
// Lists all sessions for the current project (pre-filtered to >0 msgs)
// with type-to-filter search. Up/Down navigates, Enter loads + replays
// into scrollback + syncs the agent via `AgentCommand::SetMessages`,
// Esc cancels, printable chars + Backspace edit the filter query.
// F2 renames the selected session.

use anyhow::Result;
use atomcode_core::agent::AgentCommand;
use atomcode_core::session::{Session, SessionMeta};
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::{
    build_status, format_tool_detail, perform_session_rename, summarise, Buffer, LoopCtx,
};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

pub struct SessionPicker {
    /// All sessions for the project, pre-filtered to message_count > 0.
    pub sessions: Vec<SessionMeta>,
    /// User-typed filter text. Empty string = show all.
    pub query: String,
    /// Indices into `sessions` that match `query` (case-insensitive substring).
    pub filtered: Vec<usize>,
    /// Index into `filtered`.
    pub selected: usize,
    /// Whether we are in rename editing mode.
    pub rename_editing: bool,
    /// The new name being edited for rename.
    pub rename_buffer: String,
}

impl SessionPicker {
    pub fn open(sessions: Vec<SessionMeta>) -> Self {
        let filtered: Vec<usize> = (0..sessions.len()).collect();
        Self {
            sessions,
            query: String::new(),
            filtered,
            selected: 0,
            rename_editing: false,
            rename_buffer: String::new(),
        }
    }

    pub fn update_filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| q.is_empty() || s.name.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        self.selected = 0;
    }

    pub fn up(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.filtered.len().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
        }
    }

    pub fn chosen_id(&self) -> Option<atomcode_core::session::SessionId> {
        let i = *self.filtered.get(self.selected)?;
        self.sessions.get(i).map(|s| s.id.clone())
    }
}

impl Modal for SessionPicker {
    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // Handle rename editing mode
        if self.rename_editing {
            match code {
                KeyCode::Esc => {
                    // Cancel rename editing
                    self.rename_editing = false;
                    self.rename_buffer.clear();
                    self.draw(buf, state, ctx, renderer);
                    return Ok(ModalAction::Continue);
                }
                KeyCode::Enter => {
                    if let Some(idx) = self.filtered.get(self.selected).copied() {
                        if let Some(session_meta) = self.sessions.get(idx) {
                            let id = session_meta.id.clone();
                            match perform_session_rename(
                                &ctx.session_manager,
                                &id,
                                &self.rename_buffer,
                            ) {
                                Ok((old_name, new_name)) => {
                                    // Update the session name in our local list
                                    if let Some(s) = self.sessions.get_mut(idx) {
                                        s.name = new_name.clone();
                                    }
                                    // Recompute filtered list since new name may no longer match query
                                    let prev_id = id.clone();
                                    self.update_filter();
                                    // Try to keep the same session selected
                                    self.selected = self
                                        .filtered
                                        .iter()
                                        .position(|&fi| self.sessions[fi].id == prev_id)
                                        .unwrap_or(0);
                                    // Show success feedback
                                    renderer.render(UiLine::CommandOutput(format!(
                                        "  Renamed: '{}' -> '{}'",
                                        old_name, new_name
                                    )));
                                    renderer.flush();
                                }
                                Err(err) => {
                                    renderer.render(UiLine::Error(err));
                                    renderer.flush();
                                }
                            }
                        }
                    }
                    self.rename_editing = false;
                    self.rename_buffer.clear();
                    self.draw(buf, state, ctx, renderer);
                    return Ok(ModalAction::Continue);
                }
                KeyCode::Backspace => {
                    self.rename_buffer.pop();
                    self.draw(buf, state, ctx, renderer);
                    return Ok(ModalAction::Continue);
                }
                KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                    self.rename_buffer.push(c);
                    self.draw(buf, state, ctx, renderer);
                    return Ok(ModalAction::Continue);
                }
                _ => {
                    self.draw(buf, state, ctx, renderer);
                    return Ok(ModalAction::Continue);
                }
            }
        }

        // Normal mode handling
        match code {
            KeyCode::Up => {
                self.up();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Down => {
                self.down();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.update_filter();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.update_filter();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::F(2) => {
                // F2 to start rename editing for selected session
                if let Some(idx) = self.filtered.get(self.selected).copied() {
                    if let Some(session) = self.sessions.get(idx) {
                        self.rename_buffer = session.name.clone();
                        self.rename_editing = true;
                        self.draw(buf, state, ctx, renderer);
                    }
                } else {
                    renderer.render(UiLine::Error("No session selected".into()));
                    renderer.flush();
                }
                Ok(ModalAction::Continue)
            }
            KeyCode::Enter => {
                let Some(id) = self.chosen_id() else {
                    // Filter matched nothing — ignore Enter, stay open.
                    return Ok(ModalAction::Continue);
                };
                match ctx.session_manager.load(&id) {
                    Ok(session) => {
                        ctx.current_session_id = Some(id);
                        replay_session(renderer, &session, true);
                        ctx.agent
                            .cmd_tx
                            .send(AgentCommand::SetMessages(session.messages.clone()))
                            .ok();
                        // Continue accumulating into the same session
                        // file — future TurnComplete saves overwrite it
                        // instead of leaving the old snapshot + creating
                        // a new one beside it.
                        // Bind telemetry session_id to the resumed session's UUID.
                        if let Ok(uuid) = uuid::Uuid::parse_str(session.id.as_str()) {
                            ctx.telemetry.set_session_id(uuid);
                        }
                        ctx.current_session = session;
                        state.on_turn_complete();
                        Ok(ModalAction::Close)
                    }
                    Err(e) => {
                        ctx.current_session_id = None;
                        state.total_tokens = 0;
                        state.thinking_idx = 0;
                        state.on_turn_complete();
                        renderer.render(UiLine::Error(format!("load session failed: {}", e)));
                        renderer.flush();
                        Ok(ModalAction::Close)
                    }
                }
            }
            KeyCode::Esc => Ok(ModalAction::Close),
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let payload = build_menu_payload(self);
        renderer.render(UiLine::InputPrompt {
            buf: buf.text.clone(),
            cursor_byte: buf.cursor,
            menu: Some(payload),
            status: build_status(state, ctx),
        });
        renderer.flush();
    }
}

fn build_menu_payload(p: &SessionPicker) -> MenuPayload {
    // Empty state: surface a hint row so the user can tell the filter is
    // active and which query is excluding everything (otherwise the menu
    // renders as blank space and looks like the modal hung).
    if p.filtered.is_empty() {
        let label = if p.sessions.is_empty() {
            "(no sessions in this project yet)".to_string()
        } else if p.query.is_empty() {
            "(no sessions match)".to_string()
        } else {
            format!("(no sessions match \"{}\" — Backspace to clear)", p.query)
        };
        return MenuPayload {
            items: vec![(label, String::new())],
            selected: 0,
            kind: crate::render::MenuKind::SlashCommand,
        };
    }
    let items: Vec<(String, String)> = p
        .filtered
        .iter()
        .enumerate()
        .map(|(filter_idx, &session_idx)| {
            let s = &p.sessions[session_idx];
            let desc = format!("{} msgs · {}", s.message_count, humanize_age(s.updated_at));
            // If in rename editing mode and this is the selected item, show the editing buffer
            if p.rename_editing && filter_idx == p.selected {
                (format!("> {}_  [Enter: confirm, Esc: cancel]", p.rename_buffer), desc)
            } else {
                (s.name.clone(), desc)
            }
        })
        .collect();
    MenuPayload {
        items,
        selected: p.selected,
        kind: crate::render::MenuKind::SlashCommand,
    }
}

fn humanize_age(ts: u64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(ts);
    let d = now.saturating_sub(ts);
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86400)
    }
}

/// Emit historical session messages into scrollback as semantic UiLines,
/// so the user sees the prior conversation before continuing.
///
/// `reset = true` clears the screen first (used by `/resume` mid-session
/// — without this, repeated switches stack body_lines and the worker's
/// render-cmd backlog, manifesting as dropped keystrokes "吞字" + 50-150ms
/// per-keystroke latency). `reset = false` appends to existing scrollback
/// — used by the CLI auto-continue path at startup, which has the welcome
/// banner above the replay and shouldn't wipe it.
pub(crate) fn replay_session(renderer: &mut dyn Renderer, session: &Session, reset: bool) {
    use atomcode_core::conversation::message::{MessageContent, Role};
    if reset {
        renderer.reset();
    }
    renderer.render(UiLine::TurnSeparator {
        label: format!("resumed: {}", session.name),
    });
    for m in &session.messages {
        match (&m.role, &m.content) {
            (Role::User, MessageContent::Text(s)) => {
                renderer.render(UiLine::User(s.clone()));
            }
            (Role::Assistant, MessageContent::Text(s)) => {
                if !s.is_empty() {
                    renderer.render(UiLine::AssistantText(s.clone()));
                    renderer.render(UiLine::AssistantLineBreak);
                }
            }
            (
                Role::Assistant,
                MessageContent::AssistantWithToolCalls {
                    text, tool_calls, ..
                },
            ) => {
                if let Some(t) = text {
                    if !t.is_empty() {
                        renderer.render(UiLine::AssistantText(t.clone()));
                        renderer.render(UiLine::AssistantLineBreak);
                    }
                }
                for tc in tool_calls {
                    renderer.render(UiLine::ToolCall {
                        name: tc.name.clone(),
                        detail: format_tool_detail(&tc.name, &tc.arguments),
                    });
                }
            }
            (Role::Tool, MessageContent::ToolResult(r)) => {
                renderer.render(UiLine::ToolResult {
                    success: r.success,
                    summary: summarise(&r.output, r.success),
                });
            }
            (Role::Tool, MessageContent::ToolResultRef(r)) => {
                renderer.render(UiLine::ToolResult {
                    success: true,
                    summary: summarise(&r.summary, true),
                });
            }
            _ => {}
        }
    }
    renderer.render(UiLine::TurnComplete);
    renderer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::session::{SessionId, SessionMeta};
    use std::path::PathBuf;

    fn meta(name: &str, msgs: usize) -> SessionMeta {
        SessionMeta {
            id: SessionId::from_string(format!("id-{name}")),
            name: name.to_string(),
            working_dir: PathBuf::from("/tmp/x"),
            created_at: 0,
            updated_at: 0,
            message_count: msgs,
            file_size: 0,
        }
    }

    #[test]
    fn open_shows_all_sessions_initially() {
        let p = SessionPicker::open(vec![meta("alpha", 3), meta("beta", 5)]);
        assert_eq!(p.filtered.len(), 2);
        assert_eq!(p.selected, 0);
        assert!(p.query.is_empty());
    }

    #[test]
    fn update_filter_matches_by_substring_case_insensitive() {
        let mut p = SessionPicker::open(vec![
            meta("Fix auth bug", 4),
            meta("Refactor renderer", 7),
            meta("authentication flow", 2),
        ]);
        p.query = "auth".to_string();
        p.update_filter();
        assert_eq!(p.filtered.len(), 2);
        let names: Vec<&str> = p
            .filtered
            .iter()
            .map(|i| p.sessions[*i].name.as_str())
            .collect();
        assert!(names.contains(&"Fix auth bug"));
        assert!(names.contains(&"authentication flow"));
    }

    #[test]
    fn update_filter_empty_query_shows_all() {
        let mut p = SessionPicker::open(vec![meta("x", 1), meta("y", 1)]);
        p.query = "zz".to_string();
        p.update_filter();
        assert_eq!(p.filtered.len(), 0);
        p.query.clear();
        p.update_filter();
        assert_eq!(p.filtered.len(), 2);
    }

    #[test]
    fn update_filter_resets_selection_to_zero() {
        let mut p = SessionPicker::open(vec![meta("one", 1), meta("two", 1), meta("three", 1)]);
        p.selected = 2;
        p.query = "on".to_string();
        p.update_filter();
        assert_eq!(p.selected, 0, "selection must reset when filter changes");
    }

    #[test]
    fn down_and_up_stay_within_filtered_bounds() {
        let mut p = SessionPicker::open(vec![meta("a", 1), meta("b", 1)]);
        p.down();
        assert_eq!(p.selected, 1);
        p.down();
        assert_eq!(p.selected, 1, "down at end stays put");
        p.up();
        assert_eq!(p.selected, 0);
        p.up();
        assert_eq!(p.selected, 0, "up at top stays put");
    }

    #[test]
    fn chosen_returns_session_at_selected() {
        let sessions = vec![meta("first", 1), meta("second", 1)];
        let mut p = SessionPicker::open(sessions);
        p.down();
        let id = p.chosen_id().expect("selection should exist");
        assert_eq!(id.as_str(), "id-second");
    }

    #[test]
    fn chosen_returns_none_when_filter_empty() {
        let mut p = SessionPicker::open(vec![meta("alpha", 1)]);
        p.query = "xyz".to_string();
        p.update_filter();
        assert!(p.chosen_id().is_none());
    }

    #[test]
    fn build_menu_payload_shows_hint_when_filter_matches_nothing() {
        // Regression: typing a query that excludes every session used to
        // render a blank menu (items.len() == 0), so the user couldn't
        // tell whether /resume hung, the filter was active, or what.
        // Now we surface a single non-interactive hint row so the empty
        // state is visible.
        let mut p = SessionPicker::open(vec![meta("alpha", 1), meta("beta", 1)]);
        p.query = "zz".to_string();
        p.update_filter();
        assert_eq!(p.filtered.len(), 0);
        let payload = build_menu_payload(&p);
        assert_eq!(
            payload.items.len(),
            1,
            "empty filter should produce a single hint row, got: {:?}",
            payload.items
        );
        let (label, _) = &payload.items[0];
        assert!(
            label.contains("zz"),
            "hint should echo the user's query so they know which filter is active: {}",
            label
        );
    }

    #[test]
    fn build_menu_payload_shows_hint_when_no_sessions_at_all() {
        let p = SessionPicker::open(vec![]);
        let payload = build_menu_payload(&p);
        assert_eq!(payload.items.len(), 1, "must show some empty-state hint");
    }
}
