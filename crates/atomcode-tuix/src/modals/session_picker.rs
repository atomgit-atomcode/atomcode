// crates/atomcode-tuix/src/modals/session_picker.rs
//
// `/resume` modal — prior-sessions picker.
//
// Lists all sessions for the current project (pre-filtered to >0 msgs)
// with type-to-filter search. Up/Down navigates, Enter loads + replays
// into scrollback + syncs the agent via `AgentCommand::SetConversation`,
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
    /// Index into `sessions` awaiting delete confirmation, or None.
    pub confirm_delete: Option<usize>,
    /// Status message shown in the footer (overwrites previous status).
    pub delete_status: Option<String>,
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
            confirm_delete: None,
            delete_status: None,
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
        self.confirm_delete = None;
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
        self.confirm_delete = None;
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
                                    renderer.render(UiLine::CommandOutput(
                                        crate::i18n::t(crate::i18n::Msg::SessionRenamed {
                                            old: &old_name,
                                            new: &new_name,
                                        }).into_owned(),
                                    ));
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
                self.delete_status = None;
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Down => {
                self.down();
                self.delete_status = None;
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.update_filter();
                self.confirm_delete = None;
                self.delete_status = None;
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.update_filter();
                self.confirm_delete = None;
                self.delete_status = None;
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::F(2) => {
                // F2 to start rename editing for selected session
                if let Some(idx) = self.filtered.get(self.selected).copied() {
                    if let Some(session) = self.sessions.get(idx) {
                        self.rename_buffer = session.name.clone();
                        self.rename_editing = true;
                        self.confirm_delete = None;
                        self.delete_status = None;
                        self.draw(buf, state, ctx, renderer);
                    }
                } else {
                    renderer.render(UiLine::Error(
                        crate::i18n::t(crate::i18n::Msg::SessionNoneSelected).into_owned(),
                    ));
                    renderer.flush();
                }
                Ok(ModalAction::Continue)
            }
            KeyCode::Char(c) if mods.contains(KeyModifiers::CONTROL) && c == 'd' => {
                // Ctrl+D: delete selected session (with confirmation)
                let Some(idx) = self.filtered.get(self.selected).copied() else {
                    renderer.render(UiLine::Error(
                        crate::i18n::t(crate::i18n::Msg::SessionNoneSelected).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(ModalAction::Continue);
                };
                if self.confirm_delete == Some(idx) {
                    // Second Ctrl+D: confirm delete
                    if let Some(session) = self.sessions.get(idx) {
                        let id = session.id.clone();
                        match ctx.session_manager.delete(&id) {
                            Ok(()) => {
                                let name = session.name.clone();
                                self.sessions.remove(idx);
                                self.confirm_delete = None;
                                self.update_filter();
                                // Point to the session that shifted into the deleted slot.
                                // If that's out of bounds, go to the last one.
                                self.selected = self
                                    .filtered
                                    .iter()
                                    .position(|&fi| fi >= idx)
                                    .unwrap_or(self.filtered.len().saturating_sub(1));
                                self.delete_status = Some(
                                    crate::i18n::t(crate::i18n::Msg::SessionDeleted {
                                        name: &name,
                                    }).into_owned(),
                                );
                            }
                            Err(e) => {
                                self.confirm_delete = None;
                                self.delete_status = Some(
                                    crate::i18n::t(crate::i18n::Msg::SessionDeleteFailed {
                                        error: &e.to_string(),
                                    }).into_owned(),
                                );
                            }
                        }
                    }
                } else {
                    // First Ctrl+D: ask for confirmation
                    self.confirm_delete = Some(idx);
                    if let Some(session) = self.sessions.get(idx) {
                        self.delete_status = Some(
                            crate::i18n::t(crate::i18n::Msg::SessionDeleteConfirm {
                                name: &session.name,
                            }).into_owned(),
                        );
                    }
                }
                self.draw(buf, state, ctx, renderer);
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
                            .send(AgentCommand::SetConversation(
                                session.to_conversation_snapshot(),
                            ))
                            .ok();
                        // Continue accumulating into the same session file —
                        // future TurnComplete saves overwrite it. Bind
                        // telemetry + agent (header/datalog) to the resumed
                        // session's persistent id so /resume reuses its
                        // original id (gateway routes back to the warm upstream).
                        crate::event_loop::commands::bind_telemetry_to_session(ctx, &session);
                        ctx.current_session = session;
                        ctx.bg_manager
                            .set_foreground_session(ctx.current_session.clone());
                        state.on_turn_complete();
                        // 同步模式：把这次「本地切换到历史会话」双向同步到 webui，并把
                        // 共享 LiveSession 重绑到该会话（含其历史）。非同步模式下为 no-op。
                        crate::event_loop::commands::sync_local_session_switch(ctx, renderer);
                        Ok(ModalAction::Close)
                    }
                    Err(e) => {
                        ctx.current_session_id = None;
                        state.total_tokens = 0;
                        state.thinking_idx = 0;
                        state.on_turn_complete();
                        let msg = format!("{}", e);
                        renderer.render(UiLine::Error(
                            crate::i18n::t(crate::i18n::Msg::SessionLoadFailed { error: &msg }).into_owned(),
                        ));
                        renderer.flush();
                        Ok(ModalAction::Close)
                    }
                }
            }
            KeyCode::Esc => {
                if self.confirm_delete.is_some() {
                    self.confirm_delete = None;
                    self.draw(buf, state, ctx, renderer);
                    Ok(ModalAction::Continue)
                } else {
                    Ok(ModalAction::Close)
                }
            }
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let payload = build_menu_payload(self);
        let mut status = build_status(state, ctx);
        if let Some(msg) = &self.delete_status {
            status.hint = Some((msg.clone(), crate::render::HintSeverity::Info));
        }
        renderer.render(UiLine::InputPrompt {
            buf: buf.text.clone(),
            cursor_byte: buf.cursor,
            menu: Some(payload),
            status,
            attachments: Vec::new(),
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
            kind: crate::render::MenuKind::TwoColumn { row_prefix: "", selected_marker: "▸" },
        };
    }
    let items: Vec<(String, String)> = p
        .filtered
        .iter()
        .enumerate()
        .map(|(filter_idx, &session_idx)| {
            let s = &p.sessions[session_idx];
            let msgs = crate::i18n::t(crate::i18n::Msg::SessionMsgCount { count: s.message_count });
            let desc = format!("{} · {}", msgs, humanize_age(s.updated_at));
            // If in rename editing mode and this is the selected item, show the editing buffer
            if p.rename_editing && filter_idx == p.selected {
                (
                    crate::i18n::t(crate::i18n::Msg::SessionRenameEditing {
                        buffer: &p.rename_buffer,
                    }).into_owned(),
                    desc,
                )
            } else {
                (s.name.clone(), desc)
            }
        })
        .collect();
    MenuPayload {
        items,
        selected: p.selected,
        kind: crate::render::MenuKind::TwoColumn { row_prefix: "", selected_marker: "▸" },
    }
}

fn humanize_age(ts: u64) -> String {
    use crate::i18n::{t, Msg};
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(ts);
    let d = now.saturating_sub(ts);
    if d < 60 {
        t(Msg::SessionTimeJustNow).into_owned()
    } else if d < 3600 {
        t(Msg::SessionTimeMinAgo { n: d / 60 }).into_owned()
    } else if d < 86400 {
        t(Msg::SessionTimeHourAgo { n: d / 3600 }).into_owned()
    } else {
        t(Msg::SessionTimeDayAgo { n: d / 86400 }).into_owned()
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
/// Build the inter-turn divider label from a persisted turn stat. `None`
/// (old session, or a cancelled turn that recorded no stat) → empty label,
/// which renders as a plain horizontal rule so the visual interval is still
/// restored. `done` is fixed on replay (the live rotation is cosmetic).
fn turn_divider_label(stat: Option<&atomcode_core::session::TurnStat>) -> String {
    match stat {
        Some(s) if s.errored => crate::i18n::t(crate::i18n::Msg::TurnSummaryError {
            turn_count: s.turn_count,
            tool_call_count: s.tool_call_count,
            duration: &crate::render::fmt_dur(std::time::Duration::from_millis(s.duration_ms)),
            total_tokens: s.total_tokens,
        })
        .into_owned(),
        Some(s) => crate::i18n::t(crate::i18n::Msg::TurnSummary {
            done: "Done",
            turn_count: s.turn_count,
            tool_call_count: s.tool_call_count,
            duration: &crate::render::fmt_dur(std::time::Duration::from_millis(s.duration_ms)),
            total_tokens: s.total_tokens,
            cached_pct: None,
        })
        .into_owned(),
        None => String::new(),
    }
}

pub(crate) fn replay_session(renderer: &mut dyn Renderer, session: &Session, reset: bool) {
    use atomcode_core::conversation::message::{MessageContent, Role};
    // Bracket the whole replay — the `reset()` screen wipe plus the
    // line-by-line re-emit of the entire transcript — in ONE DECSET 2026
    // synchronized-output envelope. Capable hosts then paint it as a single
    // atomic update instead of visibly blanking the screen and re-scrolling
    // the history (the reported `/resume` flicker). `end_sync()` lands the
    // final frame inside the envelope and closes it. No-op on renderers
    // without synchronized output (plain/pipe mode).
    renderer.begin_sync();
    // Suppress auto-copy during replay so we don't overwrite the user's
    // clipboard or inject stale "Copied" hints (issue #699 P1).
    renderer.set_suppress_auto_copy(true);
    if reset {
        renderer.reset();
    }
    let resumed = crate::i18n::t(crate::i18n::Msg::SessionResumedLabel { name: &session.name }).into_owned();
    renderer.render(UiLine::TurnSeparator {
        label: resumed.clone(),
    });
    // Per-turn dividers: the live session draws a `✓ … 工具 · tokens` rule at
    // every turn end, but only `messages` is persisted. `turn_stats` is anchored
    // by "message count at turn end" — so as we replay, a divider goes before
    // each new-turn user message (turn boundary), carrying the stored stats when
    // present (None → plain rule, which still restores the interval for old
    // sessions). Without this the previous turn's last output butts straight
    // against the next user input.
    let mut seen_user = false;
    for (i, m) in session.messages.iter().enumerate() {
        if matches!(m.role, Role::User) {
            if seen_user {
                let stat = session.turn_stats.iter().find(|s| s.after_message == i);
                renderer.render(UiLine::TurnSeparator {
                    label: turn_divider_label(stat),
                });
            }
            seen_user = true;
        }
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
                        name: crate::event_loop::display_tool_name(&tc.name),
                        detail: format_tool_detail(&tc.name, &tc.arguments),
                    });
                }
            }
            (Role::Tool, MessageContent::ToolResult(r)) => {
                renderer.render(UiLine::ToolResult {
                    success: r.success,
                    summary: summarise(&r.output),
                });
            }
            (Role::Tool, MessageContent::ToolResultRef(r)) => {
                renderer.render(UiLine::ToolResult {
                    success: true,
                    summary: summarise(&r.summary),
                });
            }
            _ => {}
        }
    }
    // Final turn's divider (anchored at the end), mirroring the live view which
    // showed a stats rule after the last turn too.
    if let Some(stat) = session
        .turn_stats
        .iter()
        .find(|s| s.after_message == session.messages.len())
    {
        renderer.render(UiLine::TurnSeparator {
            label: turn_divider_label(Some(stat)),
        });
    }
    renderer.render(UiLine::TurnComplete);
    renderer.render(UiLine::TurnSeparator {
        label: resumed,
    });
    renderer.flush();
    renderer.set_suppress_auto_copy(false);
    renderer.end_sync();
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
    fn turn_divider_label_renders_stats_or_plain_rule() {
        use atomcode_core::session::TurnStat;
        let s = TurnStat {
            after_message: 4,
            turn_count: 3,
            tool_call_count: 5,
            duration_ms: 6800,
            total_tokens: 1651,
            errored: false,
        };
        // Persisted stat → the same `✓ … 工具 · tokens` line the live turn showed
        // (locale-independent: digits + glyph appear in both en/zh templates). Token
        // counts render abbreviated (K/M) via `fmt_tokens`, so assert on that form
        // (1651 → "1.65K") rather than the raw integer.
        let normal = super::turn_divider_label(Some(&s));
        assert!(normal.contains('✓'), "got {normal:?}");
        let tokens = crate::i18n::fmt_tokens(1651);
        assert!(
            normal.contains('3') && normal.contains('5') && normal.contains(&tokens),
            "got {normal:?}"
        );
        // Errored turn → ✗ variant.
        let err = TurnStat { errored: true, ..s.clone() };
        assert!(super::turn_divider_label(Some(&err)).contains('✗'));
        // No stat (old session / cancelled turn) → empty label → plain rule.
        assert_eq!(super::turn_divider_label(None), "");
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
