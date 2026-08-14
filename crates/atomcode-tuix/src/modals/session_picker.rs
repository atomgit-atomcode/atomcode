// crates/atomcode-tuix/src/modals/session_picker.rs
//
// `/resume` modal — prior-sessions picker.
//
// Lists all sessions for the current project (pre-filtered to >0 msgs)
// with type-to-filter search. Up/Down navigates, Enter loads + replays
// into scrollback + restores the runtime conversation through the native API,
// Esc cancels, printable chars + Backspace edit the filter query.
//
// The chrome mirrors the `/plugin` manager (MenuKind::SessionList): a title
// row with count + project name, a bordered search box (the InputPrompt buffer
// carries the filter query, NOT the main composer), two-line session rows
// (bright title + gray metadata), and a muted bottom hint. Rename is no longer
// supported.

use crate::session::{Session, SessionMeta, TurnStat};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction, ModalPointerAction};
use crate::event_loop::{
    build_status, format_tool_detail, provider_transition_pending, summarise, Buffer, LoopCtx,
};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

fn flatten_session_preparation<T>(
    result: Result<Result<T, String>, tokio::task::JoinError>,
) -> Result<T, String> {
    result
        .map_err(|error| format!("session preparation task failed: {error}"))
        .and_then(|result| result)
}

pub struct SessionPicker {
    /// All sessions for the project, pre-filtered to message_count > 0.
    pub sessions: Vec<SessionMeta>,
    /// User-typed filter text. Empty string = show all.
    pub query: String,
    /// Indices into `sessions` that match `query` (case-insensitive substring).
    pub filtered: Vec<usize>,
    /// Index into `filtered`.
    pub selected: usize,
    /// Whether keyboard focus is on the search box (vs. a session row). When
    /// true, the bordered search box is highlighted with a live cursor and no
    /// session row is marked. Reached by pressing Up from the first session.
    pub search_focused: bool,
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
            search_focused: false,
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
        self.confirm_delete = None;
        if self.filtered.is_empty() {
            self.selected = 0;
            self.search_focused = true;
            return;
        }
        if self.search_focused {
            // Already at the top — stay in the search box.
            return;
        }
        if self.selected == 0 {
            // Move focus off the first session and into the search box.
            self.search_focused = true;
            return;
        }
        self.selected -= 1;
    }

    pub fn down(&mut self) {
        self.confirm_delete = None;
        if self.filtered.is_empty() {
            self.selected = 0;
            self.search_focused = false;
            return;
        }
        if self.search_focused {
            // Leave the search box and land on the first session.
            self.search_focused = false;
            self.selected = 0;
            return;
        }
        let max = self.filtered.len().saturating_sub(1);
        if self.selected < max {
            self.select_index(self.selected + 1);
        }
    }

    fn select_index(&mut self, index: usize) -> bool {
        let previous = (
            self.selected,
            self.search_focused,
            self.confirm_delete,
            self.delete_status.is_some(),
        );
        if self.filtered.is_empty() {
            self.selected = 0;
            self.search_focused = false;
        } else {
            self.selected = index.min(self.filtered.len() - 1);
            self.search_focused = false;
        }
        self.confirm_delete = None;
        self.delete_status = None;
        previous
            != (
                self.selected,
                self.search_focused,
                self.confirm_delete,
                self.delete_status.is_some(),
            )
    }

    #[cfg(test)]
    fn pointer_select_with(&mut self, index: usize, redraw: impl FnOnce(&Self)) {
        if self.select_index(index) {
            redraw(self);
        }
    }

    pub(crate) fn begin_preview(&self, ctx: &mut LoopCtx) {
        ctx.session_preview_selection = None;
        ctx.session_preview_result = None;
        self.sync_preview_request(ctx);
    }

    fn sync_preview_request(&self, ctx: &mut LoopCtx) {
        let selected = self
            .chosen_session()
            .map(|session| (session.project_bucket.clone(), session.id.clone()));
        let unchanged = match (&ctx.session_preview_selection, &selected) {
            (Some(current), Some((project_bucket, session_id))) => {
                current.project_bucket == *project_bucket && current.session_id == *session_id
            }
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        ctx.session_preview_result = None;
        let request = selected.map(|(project_bucket, session_id)| {
            let generation = ctx.next_session_preview_generation;
            ctx.next_session_preview_generation = generation.wrapping_add(1).max(1);
            let selection = crate::session::SessionPreviewSelection {
                project_bucket,
                session_id,
                generation,
            };
            ctx.session_preview_selection = Some(selection.clone());
            crate::event_loop::bg_runtime::SessionPreviewRequest {
                runtime_id: ctx.foreground_runtime_id,
                selection,
            }
        });
        if request.is_none() {
            ctx.session_preview_selection = None;
        }
        ctx.session_preview_request_tx.send_replace(request);
    }

    pub fn chosen_id(&self) -> Option<String> {
        self.chosen_session().map(|session| session.id.clone())
    }

    fn chosen_session(&self) -> Option<&SessionMeta> {
        let index = *self.filtered.get(self.selected)?;
        self.sessions.get(index)
    }

    fn replay_selected_current_session(
        &self,
        current_session: &Session,
        current_project_bucket: &str,
        state: &mut UiState,
        renderer: &mut dyn Renderer,
    ) -> bool {
        let is_current = self.chosen_session().is_some_and(|session| {
            session.id == current_session.id && session.project_bucket == current_project_bucket
        });
        if !is_current {
            return false;
        }

        // The runtime already owns this exact session and its lease. Replaying
        // it is therefore a display-only operation: repaint the in-memory
        // transcript and restore copy/context/todo projections without asking
        // the runtime to reacquire the lease or publishing a session switch.
        replay_session(renderer, state, current_session, true);
        true
    }

    fn confirm_selected(
        &mut self,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        let Some(selected) = self.chosen_session().cloned() else {
            return Ok(ModalAction::Continue);
        };
        let expected_bucket = ctx
            .current_session_project_bucket
            .clone()
            .unwrap_or_else(|| {
                atomcode_capabilities::session::SessionManager::project_hash(&ctx.working_dir)
            });
        if self.replay_selected_current_session(
            &ctx.current_session,
            &expected_bucket,
            state,
            renderer,
        ) {
            return Ok(ModalAction::Close);
        }
        if provider_transition_pending(ctx) {
            renderer.render(UiLine::Error(
                crate::i18n::t(crate::i18n::Msg::CmdProviderReloading).into_owned(),
            ));
            renderer.flush();
            return Ok(ModalAction::Continue);
        }
        if ctx.pending_session_resume.is_some() || ctx.pending_session_resume_preparation.is_some()
        {
            renderer.render(UiLine::Error(
                crate::i18n::t(crate::i18n::Msg::SessionLoadFailed {
                    error: "another session resume is still in progress",
                })
                .into_owned(),
            ));
            renderer.flush();
            return Ok(ModalAction::Close);
        }

        let operation_id = ctx.next_session_resume_operation_id;
        ctx.next_session_resume_operation_id =
            ctx.next_session_resume_operation_id.wrapping_add(1).max(1);
        let pending = crate::event_loop::PendingSessionResumePreparation {
            operation_id,
            project_bucket: selected.project_bucket.clone(),
            session_id: selected.id.clone(),
            working_dir: ctx.working_dir.clone(),
        };
        ctx.pending_session_resume_preparation = Some(pending.clone());
        let event_tx = ctx.runtime_event_tx.clone();
        let runtime_id = ctx.foreground_runtime_id;
        let preparation = pending.clone();
        tokio::spawn(async move {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(15),
                tokio::task::spawn_blocking(move || {
                    atomcode_daemon::legacy_convert::prepare_catalog_session_resume_in_project(
                        &preparation.project_bucket,
                        &preparation.session_id,
                    )
                    .map_err(|error| error.to_string())
                    .and_then(|prepared| {
                        prepared
                            .ok_or_else(|| format!("session {} not found", preparation.session_id))
                    })
                }),
            )
            .await;
            let result = match result {
                Ok(joined) => flatten_session_preparation(joined),
                Err(_) => Err("session preparation timed out after 15 seconds; the session may be in use by another process".into()),
            };
            let _ = event_tx.send(crate::event_loop::bg_runtime::RuntimeEvent {
                runtime_id,
                event: crate::event_loop::bg_runtime::RuntimeEventPayload::Driver(
                    crate::event_loop::bg_runtime::DriverEvent::SessionResumePrepared {
                        operation_id,
                        project_bucket: pending.project_bucket,
                        session_id: pending.session_id,
                        working_dir: pending.working_dir,
                        result,
                    },
                ),
            });
        });
        renderer.render(UiLine::CommandOutput(
            crate::i18n::t(crate::i18n::Msg::CmdSessionTransitionPending).into_owned(),
        ));
        renderer.flush();
        Ok(ModalAction::Close)
    }

    /// The static key-legend hint advertising the picker's actions
    /// (`↑↓ move · Enter open · Ctrl+D×2 delete · Type to search · Esc cancel`),
    /// or `None` for an empty list (nothing to act on). Existence of this legend
    /// is the reported gap: users couldn't tell how to delete a session because
    /// the picker showed no keys.
    ///
    /// NOTE: with the `SessionList` chrome this hint is ALSO rendered as the
    /// bottom `— … —` row inside the menu, so it's visible even when a
    /// `build_status` warning occupies the status slot; the status-slot copy
    /// (below) is a bonus, gated LOWER priority than any warning.
    fn browse_hint(&self) -> Option<String> {
        if self.filtered.is_empty() {
            return None;
        }
        Some(crate::i18n::t(crate::i18n::Msg::SessionPickerHint).into_owned())
    }
}

impl Modal for SessionPicker {
    fn accepts_session_preview(&self) -> bool {
        true
    }

    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        match code {
            KeyCode::Up => {
                self.up();
                self.delete_status = None;
                self.sync_preview_request(ctx);
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Down => {
                self.down();
                self.delete_status = None;
                self.sync_preview_request(ctx);
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.update_filter();
                // Editing the query is editing the search box — pull focus there
                // so the caret appears and no session row looks active.
                self.search_focused = true;
                self.confirm_delete = None;
                self.delete_status = None;
                self.sync_preview_request(ctx);
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.update_filter();
                self.search_focused = true;
                self.confirm_delete = None;
                self.delete_status = None;
                self.sync_preview_request(ctx);
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Char(c) if mods.contains(KeyModifiers::CONTROL) && c == 'd' => {
                // Ctrl+D: delete selected session (with confirmation). Ignored
                // while the search box holds focus — no session row is marked, so
                // there is nothing unambiguous to delete.
                if self.search_focused {
                    return Ok(ModalAction::Continue);
                }
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
                        match atomcode_daemon::legacy_convert::delete_catalog_session_in_project(
                            &session.project_bucket,
                            id.as_str(),
                        ) {
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
                                    })
                                    .into_owned(),
                                );
                                self.sync_preview_request(ctx);
                            }
                            Err(e) => {
                                self.confirm_delete = None;
                                self.delete_status = Some(
                                    crate::i18n::t(crate::i18n::Msg::SessionDeleteFailed {
                                        error: &e.to_string(),
                                    })
                                    .into_owned(),
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
                            })
                            .into_owned(),
                        );
                    }
                }
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Enter => self.confirm_selected(state, ctx, renderer),
            KeyCode::Esc => {
                if self.confirm_delete.is_some() {
                    // Cancel the pending delete. Clear the confirm prompt too,
                    // otherwise the "Press Ctrl+D again…" footer lingers (and
                    // suppresses the key legend) until the next Up/Down/edit.
                    self.confirm_delete = None;
                    self.delete_status = None;
                    self.draw(buf, state, ctx, renderer);
                    Ok(ModalAction::Continue)
                } else {
                    Ok(ModalAction::Close)
                }
            }
            _ => Ok(ModalAction::Continue),
        }
    }

    fn handle_paste(
        &mut self,
        text: &str,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        // Paste goes into the query filter, not the main buffer
        for c in text.chars() {
            if c.is_control() {
                continue; // skip newlines/control characters
            }
            self.query.push(c);
        }
        self.update_filter();
        self.search_focused = true;
        self.confirm_delete = None;
        self.delete_status = None;
        self.selected = 0;
        self.sync_preview_request(ctx);
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }

    fn handle_pointer(
        &mut self,
        action: ModalPointerAction,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        match action {
            ModalPointerAction::Select(index) => {
                let changed = self.select_index(index);
                self.sync_preview_request(ctx);
                if changed {
                    self.draw(buf, state, ctx, renderer);
                }
                Ok(ModalAction::Continue)
            }
            ModalPointerAction::Confirm(index) => {
                self.select_index(index);
                self.sync_preview_request(ctx);
                self.confirm_selected(state, ctx, renderer)
            }
            ModalPointerAction::Cancel => Ok(ModalAction::Close),
        }
    }

    fn draw(&self, _buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        // Project name = basename of the working dir (falls back to the full
        // path string, then to a placeholder, so the title never renders blank).
        let project = ctx
            .working_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                let p = ctx.working_dir.to_string_lossy();
                if p.is_empty() {
                    "project".to_string()
                } else {
                    p.into_owned()
                }
            });
        let current_project_bucket =
            atomcode_capabilities::session::SessionManager::project_hash(&ctx.working_dir);
        let payload = build_menu_payload_with_preview(
            self,
            &project,
            Some((
                ctx.current_session.id.as_str(),
                current_project_bucket.as_str(),
            )),
            ctx.session_preview_selection.as_ref(),
            ctx.session_preview_result.as_ref(),
        );
        let mut status = build_status(state, ctx);
        // Delete confirmation/result is the user's own active interaction — it
        // must always show, overriding any build_status warning. The static key
        // legend is lowest priority: only fill an otherwise-empty hint slot so a
        // no-provider / official-build / usage warning stays visible in the
        // picker (the legend also renders as the menu's bottom `— … —` row, so
        // it's never fully hidden even when a warning owns the status slot).
        if let Some(msg) = &self.delete_status {
            status.hint = Some((msg.clone(), crate::render::HintSeverity::Info));
        } else if status.hint.is_none() {
            if let Some(msg) = self.browse_hint() {
                status.hint = Some((msg, crate::render::HintSeverity::Info));
            }
        }
        // The `SessionList` chrome puts the filter query in the (bordered) search
        // box, so the InputPrompt buffer carries the QUERY — not the main
        // composer — exactly like `/plugin`.
        renderer.render(UiLine::InputPrompt {
            buf: self.query.clone(),
            cursor_byte: self.query.len(),
            menu: Some(payload),
            status,
            attachments: Vec::new(),
        });
        renderer.flush();
    }
}

/// The four fixed header rows (title, blank, search query, blank) that the
/// `SessionList` chrome expects before the session rows begin. Session row
/// `selected` indices are offset by this in the payload so the `▸` marker
/// lands on the right row (mirrors `plugin_manager`'s `selected_offset`).
pub(crate) const HEADER_ROWS: usize = 4;

#[cfg(test)]
fn build_menu_payload(
    p: &SessionPicker,
    project: &str,
    current_session: Option<(&str, &str)>,
) -> MenuPayload {
    build_menu_payload_with_preview(p, project, current_session, None, None)
}

fn build_menu_payload_with_preview(
    p: &SessionPicker,
    project: &str,
    current_session: Option<(&str, &str)>,
    preview_selection: Option<&crate::session::SessionPreviewSelection>,
    preview_result: Option<
        &Result<
            Option<atomcode_daemon::legacy_convert::CatalogSessionPreview>,
            atomcode_daemon::legacy_convert::CatalogSessionPreviewError,
        >,
    >,
) -> MenuPayload {
    // Title row: when the search box is focused, show just the bare heading
    // ("恢复会话" / "Resume session") without position/total/project, so the
    // user isn't distracted by the count while typing.
    let title = if p.search_focused {
        crate::i18n::t(crate::i18n::Msg::SessionPickerTitleBare).into_owned()
    } else {
        let total = p.sessions.len();
        let pos = if p.filtered.is_empty() {
            0
        } else {
            p.selected + 1
        };
        crate::i18n::t(crate::i18n::Msg::SessionPickerTitle {
            n: pos,
            total,
            project,
        })
        .into_owned()
    };
    let hint = crate::i18n::t(crate::i18n::Msg::SessionPickerHint).into_owned();

    // Header chrome: title, blank, search query, blank.
    let mut items: Vec<(String, String)> =
        vec![(title, String::new()), (String::new(), String::new())];
    items.push((p.query.clone(), String::new()));
    items.push((String::new(), String::new()));
    // Empty state: surface a hint row so the user can tell the filter is
    // active and which query is excluding everything (otherwise the menu
    // renders as blank chrome and looks like the modal hung). This row is NOT
    // selectable, so `selected` points past the list (never highlights it).
    if p.filtered.is_empty() {
        let label = if p.sessions.is_empty() {
            crate::i18n::t(crate::i18n::Msg::SessionPickerEmptyProject).into_owned()
        } else if p.query.is_empty() {
            crate::i18n::t(crate::i18n::Msg::SessionPickerEmptyFilter).into_owned()
        } else {
            crate::i18n::t(crate::i18n::Msg::SessionPickerEmptyFilterQuery { query: &p.query })
                .into_owned()
        };
        items.push((label, String::new()));
        items.push((format!("— {} —", hint), String::new()));
        return MenuPayload {
            items,
            // No session is selectable. If the search box holds focus (the user
            // typed a query that excludes everything, or pressed Up), keep it
            // highlighted with a cursor at row 2; otherwise point past the end so
            // no row is marked.
            selected: if p.search_focused { 2 } else { usize::MAX },
            kind: crate::render::MenuKind::SessionList,
        };
    }
    for (filtered_index, &session_idx) in p.filtered.iter().enumerate() {
        let s = &p.sessions[session_idx];
        let msgs = crate::i18n::t(crate::i18n::Msg::SessionMsgCount {
            count: s.message_count,
        });
        let mut metadata = format!("{} · {}", msgs, humanize_age(s.updated_at));
        if current_session
            .is_some_and(|(id, project_bucket)| s.id == id && s.project_bucket == project_bucket)
        {
            metadata.push_str(" · ");
            metadata.push_str(&crate::i18n::t(crate::i18n::Msg::DirCurrent));
        }
        let is_previewed = !p.search_focused
            && filtered_index == p.selected
            && preview_selection.is_some_and(|selection| {
                selection.project_bucket == s.project_bucket && selection.session_id == s.id
            });
        if is_previewed {
            metadata.push('\n');
            metadata.push_str(&crate::sanitize::scrub_controls(
                &s.working_dir.to_string_lossy(),
            ));
            match preview_result {
                Some(Ok(Some(preview))) => {
                    metadata.push('\n');
                    metadata.push_str(&match (&preview.provider_id, &preview.model_id) {
                        (Some(provider), Some(model)) => format!("{provider} · {model}"),
                        _ => preview_unavailable_label().to_string(),
                    });
                    for line in &preview.excerpt {
                        metadata.push('\n');
                        metadata.push_str(line);
                    }
                }
                Some(Ok(None)) | Some(Err(_)) => {
                    metadata.push('\n');
                    metadata.push_str(preview_unavailable_label());
                }
                None => {
                    metadata.push('\n');
                    metadata.push_str(preview_loading_label());
                }
            }
        }
        items.push((s.name.clone(), metadata));
    }
    items.push((format!("— {} —", hint), String::new()));

    // When the search box holds focus, mark row 2 (the bordered query field) as
    // selected so it highlights and shows a cursor; no session row is marked.
    // Otherwise offset the selection past the header chrome so the ▸ marker
    // lands on the selected session row (rows begin at index HEADER_ROWS).
    let selected = if p.search_focused {
        2
    } else {
        HEADER_ROWS + p.selected
    };
    MenuPayload {
        selected,
        items,
        kind: crate::render::MenuKind::SessionList,
    }
}

fn preview_loading_label() -> &'static str {
    match crate::i18n::current_locale() {
        crate::i18n::Locale::ZhCn => "正在加载预览…",
        crate::i18n::Locale::En => "Loading preview…",
    }
}

fn preview_unavailable_label() -> &'static str {
    match crate::i18n::current_locale() {
        crate::i18n::Locale::ZhCn => "预览不可用",
        crate::i18n::Locale::En => "Preview unavailable",
    }
}

fn humanize_age(ts_ms: i64) -> String {
    use crate::i18n::{t, Msg};
    let d = atomcode_capabilities::session::now_ms()
        .saturating_sub(ts_ms)
        .max(0) as u64
        / 1000;
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
fn turn_divider_label(stat: Option<&TurnStat>) -> String {
    match stat {
        Some(s) if s.errored => crate::i18n::t(crate::i18n::Msg::TurnSummaryError {
            turn_count: s.turn_count,
            tool_call_count: s.tool_call_count,
            duration: &crate::render::fmt_dur(std::time::Duration::from_millis(s.duration_ms)),
            total_tokens: s.total_tokens,
            // Resume replay: the reason is live-only (not persisted in TurnStat).
            reason: None,
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

/// The raw markdown of the most recent assistant reply in `messages` — what
/// `/copy` / `/copy msg` must target after a `/resume` (or `atomcode -c`,
/// `/undo`, `/bg` resume) repaints the transcript but never streamed the reply
/// live. Mirrors the live [`UiState::last_assistant_response`]: the accumulated
/// assistant text of the newest turn that produced any (a `User` message is a
/// turn boundary). Returns `""` when the session has no assistant text.
fn last_assistant_reply_markdown(messages: &[atomcode_kernel::message::Message]) -> String {
    use atomcode_kernel::message::Role;
    // Append `text` to the in-progress turn's reply, paragraph-separated so a
    // turn that emitted prose, then a tool call, then more prose reads cleanly.
    fn push_reply(acc: &mut String, text: &str) {
        if text.is_empty() {
            return;
        }
        if !acc.is_empty() {
            acc.push_str("\n\n");
        }
        acc.push_str(text);
    }

    let mut acc = String::new(); // assistant text of the in-progress turn
    let mut last = String::new(); // newest turn that produced assistant text
    for m in messages {
        match m.role {
            // Turn boundary: seal the turn that just ended (keep the previous
            // `last` if this turn produced no assistant text — a trailing bare
            // user message must not blank out the reply above it).
            Role::User if is_real_user_message(m) => {
                if !acc.is_empty() {
                    last = std::mem::take(&mut acc);
                }
            }
            // Assistant text is the flat `text` field for both a plain reply and
            // a tool-call turn (kernel merges them; `text` is empty for a bare
            // tool call, so `push_reply` no-ops on it).
            Role::Assistant => push_reply(&mut acc, &m.text),
            _ => {}
        }
    }
    if !acc.is_empty() {
        last = acc;
    }
    last
}

fn is_real_user_message(message: &atomcode_kernel::message::Message) -> bool {
    use atomcode_kernel::message::Role;
    matches!(message.role, Role::User)
        && !message.synthetic
        // Compatibility only: old snapshots predate reliable synthetic provenance.
        && !atomcode_capabilities::reminder::is_system_reminder(&message.text)
}

pub(crate) fn replay_session(
    renderer: &mut dyn Renderer,
    state: &mut UiState,
    session: &Session,
    reset: bool,
) {
    use atomcode_kernel::message::Role;
    // Bracket the whole replay — the `reset()` screen wipe plus the
    // line-by-line re-emit of the entire transcript — in ONE DECSET 2026
    // synchronized-output envelope. Capable hosts then paint it as a single
    // atomic update instead of visibly blanking the screen and re-scrolling
    // the history (the reported `/resume` flicker). `end_sync()` lands the
    // final frame inside the envelope and closes it. No-op on renderers
    // without synchronized output (plain/pipe mode).
    renderer.begin_sync();
    renderer.begin_initial_history_replay();
    // Suppress auto-copy during replay so we don't overwrite the user's
    // clipboard or inject stale "Copied" hints (issue #699 P1).
    renderer.set_suppress_auto_copy(true);
    if reset {
        renderer.reset();
    }
    let resumed = crate::i18n::t(crate::i18n::Msg::SessionResumedLabel {
        name: &session.name,
    })
    .into_owned();
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
    // Suppress successful todowrite tool RESULTs (the persistent panel is the sole
    // view for todowrite output; errors still show). Track parseable call ids.
    let mut todowrite_call_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Call ids whose result is folded into a rebuilt parallel-batch child row
    // (`build_replay_tool_batch`); their standalone tool RESULT row is suppressed
    // so the replayed batch matches the compact live batch view.
    let mut batched_result_ids: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    // Persisted results keyed by call id, so a rebuilt batch child can show its
    // final `→ N lines` / `→ ✗` suffix (batch metadata itself is never persisted).
    let mut result_of: std::collections::HashMap<String, (bool, String)> =
        std::collections::HashMap::new();
    for m in &session.messages {
        if m.role == Role::Tool {
            if let Some(id) = m.tool_call_id.as_ref().filter(|id| !id.is_empty()) {
                result_of.insert(id.clone(), (!m.is_error, m.text.clone()));
            }
        }
    }
    // Seed the persistent todo panel + id→title cache BEFORE rendering so a
    // rebuilt batch's `todo update #N` children can splice in task titles the
    // same way the live path does. (Also resets a prior session's panel/titles.)
    state.active_todos = crate::event_loop::todo_progress_from_messages(&session.messages);
    crate::event_loop::sync_todo_titles(state);
    for (i, m) in session.messages.iter().enumerate() {
        if is_real_user_message(m) {
            if seen_user {
                let stat = session
                    .turn_stats
                    .iter()
                    .find(|s| s.position_valid && s.after_message == i);
                renderer.render(UiLine::TurnSeparator {
                    label: turn_divider_label(stat),
                });
            }
            seen_user = true;
        }
        match m.role {
            Role::User if is_real_user_message(m) => {
                renderer.render(UiLine::User(m.text.clone()));
            }
            // A kernel assistant message carries flat `text` + `tool_calls`
            // (empty for a pure text reply). Render the prose first, then each
            // requested tool call — identical to the old `Text` /
            // `AssistantWithToolCalls` split.
            Role::Assistant => {
                if !m.text.is_empty() {
                    renderer.render(UiLine::AssistantText(m.text.clone()));
                    renderer.render(UiLine::AssistantLineBreak);
                }
                // Group iff this step actually ran as a batch live. The kernel
                // emits a batch only when ≥2 calls are DISTINCT by identity
                // (name + canonicalized args); a step whose duplicate calls it
                // collapsed showed no batch header, so gating on raw
                // `tool_calls.len()` would over-group and diverge from live.
                if atomcode_kernel::agent::distinct_tool_call_count(&m.tool_calls) > 1 {
                    // A multi-call assistant step ran as one parallel batch live.
                    // Rebuild the grouped "Running N calls" header + child rows
                    // (with folded result suffixes) instead of N standalone tool
                    // rows, and remember the call ids so their individual tool
                    // RESULT rows are suppressed below — matching the compact live
                    // batch view the user saw before the resume.
                    let (header, children, edit_displays) =
                        crate::event_loop::build_replay_tool_batch(
                            &m.tool_calls,
                            &result_of,
                            &state.todo_titles,
                        );
                    for tc in &m.tool_calls {
                        if !tc.id.is_empty() {
                            batched_result_ids.insert(tc.id.clone());
                        }
                    }
                    renderer.render(UiLine::ToolGroupRender {
                        batch_id: format!("replay-batch-{i}"),
                        header,
                        children,
                    });
                    // Live batches append edit diffs only after every child has
                    // completed, so the retained group is no longer mutable.
                    // Replay groups are already final; restore the same durable
                    // diff rows immediately from persisted ToolResult text.
                    for display in edit_displays {
                        renderer.render(UiLine::ToolResult {
                            success: true,
                            summary: display.summary,
                            diff_stats: Some(display.diff_stats),
                        });
                        renderer.render(UiLine::EditDiffBlock(display.entries));
                    }
                } else {
                    for tc in &m.tool_calls {
                        // todowrite → no inline block; the persistent panel is the sole
                        // view. Suppress the (successful) tool RESULT below by remembering
                        // the call id. Mirror the live path: only a PARSEABLE call is
                        // suppressed — a bad one falls through to a normal tool row so its
                        // error still shows.
                        if tc.name == "todowrite"
                            && atomcode_capabilities::tools::todo::parse_todos(&tc.arguments)
                                .is_ok()
                        {
                            if !tc.id.is_empty() {
                                todowrite_call_ids.insert(tc.id.clone());
                            }
                            continue;
                        }
                        renderer.render(UiLine::ToolCall {
                            name: crate::event_loop::display_tool_name(&tc.name),
                            detail: format_tool_detail(&tc.name, &tc.arguments),
                        });
                    }
                }
            }
            // Tool RESULT: kernel carries `tool_call_id` + `is_error` + flat
            // `text` (both the old `ToolResult.output` and `ToolResultRef.summary`
            // land in `text` via `message_to_kernel`). Suppress ONLY a SUCCESSFUL
            // todowrite result (its block already rendered); an errored one still
            // shows — matches the live path (`suppress_body_echo = … && success`).
            Role::Tool => {
                let success = !m.is_error;
                let call_id = m.tool_call_id.as_deref().unwrap_or("");
                // Suppress a result that is either a (successful) todowrite — the
                // panel is its sole view — or folded into a rebuilt batch child row.
                let suppressed = (success && todowrite_call_ids.contains(call_id))
                    || batched_result_ids.contains(call_id);
                if !suppressed {
                    renderer.render(UiLine::ToolResult {
                        success,
                        summary: summarise(&m.text),
                        diff_stats: None,
                    });
                }
            }
            _ => {}
        }
    }
    // Final turn's divider (anchored at the end), mirroring the live view which
    // showed a stats rule after the last turn too.
    if let Some(stat) = session
        .turn_stats
        .iter()
        .find(|s| s.position_valid && s.after_message == session.messages.len())
    {
        renderer.render(UiLine::TurnSeparator {
            label: turn_divider_label(Some(stat)),
        });
    }
    renderer.render(UiLine::TurnComplete);
    renderer.render(UiLine::TurnSeparator { label: resumed });
    renderer.end_initial_history_replay();
    renderer.flush();
    renderer.set_suppress_auto_copy(false);
    renderer.end_sync();

    // The transcript is now on screen but was never streamed live, so
    // `last_assistant_response` (which only the live TextDelta path fills)
    // is empty or — worse, after switching sessions — still holds the PREVIOUS
    // session's reply. Restore it from the replayed messages so `/copy` and
    // `/copy msg` copy THIS session's last reply. `response_finalized = true`
    // so the next live turn's first delta clears it instead of appending.
    state.last_assistant_response = last_assistant_reply_markdown(&session.messages);
    state.response_finalized = true;

    // Rehydrate the context gauge (footer token line + `/context`) from the last
    // completed turn's persisted usage. Replay never sees live `ContextStats`
    // events, so without this the gauge reads `0/100%` after resume even for a
    // huge conversation, until the next live turn. Called unconditionally (with
    // `0/0` when the target has no completed turns) so a session SWITCH resets
    // the gauge to THIS session and never leaves the previous session's count on
    // screen; `restore_context` no-ops only when there's nothing to show or clear.
    let (used, window) = session
        .turn_stats
        .iter()
        .rev()
        .find(|stat| stat.position_valid)
        .map(|s| (s.used_tokens, s.ctx_window))
        .unwrap_or((0, 0));
    state.restore_context(used, window);

    // The persistent todo panel + id→title cache were seeded BEFORE the render
    // loop (so rebuilt batch children could enrich `todo update #N` titles); no
    // second seeding is needed here.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn meta(name: &str, msgs: usize) -> SessionMeta {
        SessionMeta {
            id: format!("id-{name}"),
            name: name.to_string(),
            project_bucket: "0123456789abcdef".to_string(),
            working_dir: PathBuf::from("/tmp/x"),
            created_at: 0,
            updated_at: 0,
            message_count: msgs,
            file_size: 0,
        }
    }

    #[test]
    fn pointer_selection_uses_the_same_session_index_semantic_as_keyboard() {
        let sessions = vec![meta("one", 1), meta("two", 2), meta("three", 3)];
        let mut keyboard = SessionPicker::open(sessions.clone());
        let mut pointer = SessionPicker::open(sessions);

        keyboard.down();
        pointer.pointer_select_with(1, |_| {});

        assert_eq!(keyboard.selected, pointer.selected);
        assert_eq!(keyboard.search_focused, pointer.search_focused);
        assert_eq!(keyboard.chosen_id(), pointer.chosen_id());
    }

    #[test]
    fn real_session_payload_keeps_surface_identity_across_selection_chrome() {
        let mut picker = SessionPicker::open(vec![meta("one", 1), meta("two", 2)]);
        let before = UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: Some(build_menu_payload(&picker, "atomcode", None)),
            status: Default::default(),
            attachments: Vec::new(),
        };
        picker.down();
        let after = UiLine::InputPrompt {
            buf: String::new(),
            cursor_byte: 0,
            menu: Some(build_menu_payload(&picker, "atomcode", None)),
            status: Default::default(),
            attachments: Vec::new(),
        };

        assert_eq!(
            crate::render::worker::interaction_surface_for_line(&before),
            crate::render::worker::interaction_surface_for_line(&after),
            "title position and selected chrome are not selectable identity"
        );
    }

    #[test]
    fn pointer_selection_redraws_for_real_session_state_changes_only() {
        let mut picker = SessionPicker::open(vec![meta("one", 1), meta("two", 2)]);
        let mut redraws = 0;

        picker.pointer_select_with(0, |_| redraws += 1);
        picker.pointer_select_with(1, |_| redraws += 1);
        picker.pointer_select_with(1, |_| redraws += 1);
        picker.confirm_delete = Some(1);
        picker.delete_status = Some("confirm".into());
        picker.pointer_select_with(1, |_| redraws += 1);

        assert_eq!(redraws, 2, "clearing delete state also requires a redraw");
        assert_eq!(picker.selected, 1);
        assert!(picker.confirm_delete.is_none());
        assert!(picker.delete_status.is_none());
    }

    #[test]
    fn turn_divider_label_renders_stats_or_plain_rule() {
        let s = TurnStat {
            after_message: 4,
            position_valid: true,
            turn_count: 3,
            tool_call_count: 5,
            duration_ms: 6800,
            total_tokens: 1651,
            errored: false,
            used_tokens: 0,
            ctx_window: 0,
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
        let err = TurnStat {
            errored: true,
            ..s.clone()
        };
        assert!(super::turn_divider_label(Some(&err)).contains('✗'));
        // No stat (old session / cancelled turn) → empty label → plain rule.
        assert_eq!(super::turn_divider_label(None), "");
    }

    // ── /copy after /resume: last_assistant_reply_markdown ───────────────
    // `replay_session` restores `last_assistant_response` from these so `/copy`
    // / `/copy msg` target the resumed session's last reply (previously empty
    // after resume → "/copy msg" said "reply is empty" / copied the wrong
    // session's reply).

    #[test]
    fn last_reply_picks_newest_assistant_turn() {
        use atomcode_kernel::message::Message;
        let msgs = vec![
            Message::user("q1"),
            Message::assistant("A1", vec![]),
            Message::user("q2"),
            Message::assistant("A2", vec![]),
        ];
        assert_eq!(last_assistant_reply_markdown(&msgs), "A2");
    }

    #[test]
    fn last_reply_accumulates_text_across_a_tool_call_turn() {
        use atomcode_kernel::message::Message;
        // One turn: prose → tool call (with text) → prose. Live accumulates all
        // visible text of the turn, so replay must too.
        let msgs = vec![
            Message::user("q"),
            // A tool-call turn that also emitted prose: kernel keeps the prose in
            // the flat `text` field alongside `tool_calls`.
            Message::assistant("Let me check.", vec![]),
            Message::assistant("The answer is X.", vec![]),
        ];
        assert_eq!(
            last_assistant_reply_markdown(&msgs),
            "Let me check.\n\nThe answer is X."
        );
    }

    #[test]
    fn last_reply_keeps_prior_reply_when_session_ends_on_bare_user() {
        use atomcode_kernel::message::Message;
        // Session ends on a user message the assistant hasn't answered yet —
        // the reply still on screen is the prior one, so keep it (mirrors live,
        // where the buffer isn't cleared until the next turn's first delta).
        let msgs = vec![
            Message::user("q1"),
            Message::assistant("A1", vec![]),
            Message::user("q2 pending"),
        ];
        assert_eq!(last_assistant_reply_markdown(&msgs), "A1");
    }

    #[test]
    fn last_reply_empty_when_no_assistant_text() {
        use atomcode_kernel::message::Message;
        assert_eq!(last_assistant_reply_markdown(&[]), "");
        let only_user = vec![Message::user("hi")];
        assert_eq!(last_assistant_reply_markdown(&only_user), "");
    }

    #[test]
    fn browse_hint_advertises_key_actions() {
        // Discoverability: the picker must surface the delete/open/search
        // shortcuts, else users can't tell how to act on a session (the reported
        // gap). Rename is intentionally no longer supported, so it is absent.
        let p = SessionPicker::open(vec![meta("a", 1)]);
        let h = p.browse_hint().expect("browse mode advertises key actions");
        // Locale-independent: the literal shortcuts appear in both en and zh.
        assert!(h.contains("Ctrl+D"), "delete shortcut must be shown: {h:?}");
        assert!(h.contains("Enter"), "open shortcut must be shown: {h:?}");
        assert!(h.contains("Esc"), "cancel shortcut must be shown: {h:?}");
        assert!(
            !h.contains("F2"),
            "rename is removed — F2 must not be advertised: {h:?}"
        );
    }

    #[test]
    fn browse_hint_suppressed_when_empty() {
        // Empty list: nothing to act on, so no footer legend.
        let empty = SessionPicker::open(vec![]);
        assert_eq!(empty.browse_hint(), None);
    }

    #[test]
    fn no_rename_fields_or_refs_remain() {
        // Rename removed: the struct must construct without any rename state and
        // the payload never renders a rename-editing label. (Compile-time proof
        // that `rename_editing` / `rename_buffer` are gone; if they were
        // re-added, `..Default`-style construction below would need them.)
        let p = SessionPicker::open(vec![meta("a", 1)]);
        let payload = build_menu_payload(&p, "proj", None);
        assert!(
            payload
                .items
                .iter()
                .all(|(name, _)| !name.contains("[Enter: confirm")),
            "no rename-editing label may appear: {:?}",
            payload.items
        );
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
    fn up_from_first_session_focuses_search_box() {
        let mut p = SessionPicker::open(vec![meta("a", 1), meta("b", 1)]);
        p.down();
        assert_eq!(p.selected, 1);
        assert!(!p.search_focused);
        p.up();
        assert_eq!(p.selected, 0, "up from second lands on first session");
        assert!(!p.search_focused, "still on a session row, not the box");
        p.up();
        assert!(
            p.search_focused,
            "up from first session focuses the search box"
        );
        assert_eq!(p.selected, 0);
        p.up();
        assert!(p.search_focused, "up again stays in the search box");
    }

    #[test]
    fn down_from_search_box_returns_to_first_session() {
        let mut p = SessionPicker::open(vec![meta("a", 1), meta("b", 1)]);
        p.up();
        assert!(p.search_focused);
        p.down();
        assert!(!p.search_focused, "down leaves the search box");
        assert_eq!(p.selected, 0, "and lands on the first session");
    }

    #[test]
    fn empty_filter_focuses_search_box_on_up() {
        let mut p = SessionPicker::open(vec![meta("x", 1)]);
        p.query = "zzz".to_string();
        p.update_filter();
        assert!(p.filtered.is_empty());
        p.up();
        assert!(
            p.search_focused,
            "up with no matches parks focus on the box"
        );
        p.down();
        assert!(!p.search_focused);
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
    fn selecting_current_session_replays_the_ui_without_a_runtime_transition() {
        use atomcode_kernel::message::Message;

        #[derive(Default)]
        struct Rec {
            lines: Vec<UiLine>,
            reset_count: usize,
        }
        impl Renderer for Rec {
            fn render(&mut self, line: UiLine) {
                self.lines.push(line);
            }
            fn flush(&mut self) {}
            fn shutdown(&mut self) {}
            fn reset(&mut self) {
                self.reset_count += 1;
            }
            fn clear_screen(&mut self) {}
            fn suspend_for_external(&mut self) {}
            fn resume_from_external(&mut self) {}
            fn flush_deferred(&mut self) {}
        }

        let mut current = Session::new(PathBuf::from("/tmp/x"));
        current.id = "current-id".into();
        current.name = "current session".into();
        current.messages = vec![
            Message::user("question"),
            Message::assistant("answer", vec![]),
        ];
        let mut selected = meta("current session", current.messages.len());
        selected.id = current.id.clone();
        let current_bucket =
            atomcode_capabilities::session::SessionManager::project_hash(&current.working_dir);
        selected.project_bucket = current_bucket.clone();
        let picker = SessionPicker::open(vec![selected]);
        let mut state = UiState::with_unicode(true);
        let mut renderer = Rec::default();

        assert!(picker.replay_selected_current_session(
            &current,
            &current_bucket,
            &mut state,
            &mut renderer,
        ));

        assert_eq!(
            renderer.reset_count, 1,
            "current replay must repaint scrollback"
        );
        assert!(
            renderer.lines.iter().any(|line| matches!(
                line,
                UiLine::TurnSeparator { label } if label.contains("current session")
            )),
            "current replay must show the same resumed separator as another session"
        );
        assert_eq!(state.last_assistant_response, "answer");

        let mut same_id_other_bucket = meta("duplicate", current.messages.len());
        same_id_other_bucket.id = current.id.clone();
        let other_picker = SessionPicker::open(vec![same_id_other_bucket]);
        let mut other_renderer = Rec::default();
        assert!(
            !other_picker.replay_selected_current_session(
                &current,
                &current_bucket,
                &mut state,
                &mut other_renderer,
            ),
            "session identity is project bucket plus id, not id alone"
        );
        assert_eq!(other_renderer.reset_count, 0);
    }

    #[test]
    fn build_menu_payload_shows_hint_when_filter_matches_nothing() {
        // Regression: typing a query that excludes every session used to
        // render a blank menu, so the user couldn't tell whether /resume hung,
        // the filter was active, or what. Now we surface a non-interactive hint
        // row (after the header chrome) so the empty state is visible.
        let mut p = SessionPicker::open(vec![meta("alpha", 1), meta("beta", 1)]);
        p.query = "zz".to_string();
        p.update_filter();
        assert_eq!(p.filtered.len(), 0);
        let payload = build_menu_payload(&p, "proj", None);
        // 4 header chrome rows + 1 empty-state label + 1 bottom hint = 6.
        assert_eq!(
            payload.items.len(),
            HEADER_ROWS + 2,
            "empty filter: header chrome + one label + hint, got: {:?}",
            payload.items
        );
        let (label, _) = &payload.items[HEADER_ROWS];
        assert!(
            label.contains("zz"),
            "hint should echo the user's query so they know which filter is active: {}",
            label
        );
        // Nothing selectable → selection points past every row (no highlight).
        assert!(
            payload.selected >= payload.items.len(),
            "empty state must not highlight a row: {}",
            payload.selected
        );
    }

    #[test]
    fn build_menu_payload_shows_hint_when_no_sessions_at_all() {
        let p = SessionPicker::open(vec![]);
        let payload = build_menu_payload(&p, "proj", None);
        assert_eq!(
            payload.items.len(),
            HEADER_ROWS + 2,
            "must show header chrome + empty-state label + hint"
        );
    }

    #[test]
    fn build_menu_payload_row_order_and_kind() {
        // The SessionList chrome must emit: title → blank → query → blank →
        // one row per session → bottom `— … —` hint, and use MenuKind::SessionList.
        let mut p = SessionPicker::open(vec![meta("First task", 12), meta("Second task", 8)]);
        p.query = "task".to_string();
        p.update_filter();
        assert_eq!(p.filtered.len(), 2);
        let payload = build_menu_payload(&p, "atomcode", None);

        assert_eq!(payload.kind, crate::render::MenuKind::SessionList);
        // Row 0: title.
        assert!(
            payload.items[0].0.contains("atomcode"),
            "row 0 must be the title with project name: {:?}",
            payload.items[0]
        );
        // Row 1: blank separator.
        assert_eq!(payload.items[1], (String::new(), String::new()));
        // Row 2: the search query (goes into the bordered search box).
        assert_eq!(payload.items[2].0, "task");
        // Row 3: blank separator.
        assert_eq!(payload.items[3], (String::new(), String::new()));
        // Rows 4..: session rows (name, metadata).
        assert_eq!(payload.items[HEADER_ROWS].0, "First task");
        assert!(payload.items[HEADER_ROWS].1.contains('·'));
        assert_eq!(payload.items[HEADER_ROWS + 1].0, "Second task");
        // Last row: bottom hint wrapped in em-dashes.
        let last = &payload.items[payload.items.len() - 1].0;
        assert!(
            last.starts_with('—') && last.ends_with('—'),
            "last row must be the em-dash-wrapped hint: {last:?}"
        );
        // Selection is offset past the header so ▸ lands on the selected session.
        assert_eq!(payload.selected, HEADER_ROWS);
        p.down();
        let payload2 = build_menu_payload(&p, "atomcode", None);
        assert_eq!(payload2.selected, HEADER_ROWS + 1);
    }

    #[test]
    fn title_string_format() {
        // Title = "Resume session (n/total · project)" with n = 1-based position
        // in the CURRENT filtered list, total = total sessions in the project.
        let mut p = SessionPicker::open(vec![meta("a", 1), meta("b", 1), meta("c", 1)]);
        p.down(); // selected = 1 → position 2
        let payload = build_menu_payload(&p, "atomcode", None);
        let title = &payload.items[0].0;
        assert!(
            title.contains("2/3") && title.contains("atomcode"),
            "title must show 1-based position / total · project: {title:?}"
        );

        // Filtering shrinks the position range but total stays the project count.
        p.query = "b".to_string();
        p.update_filter(); // filtered = [b], selected reset to 0 → position 1
        let payload = build_menu_payload(&p, "atomcode", None);
        let title = &payload.items[0].0;
        assert!(
            title.contains("1/3"),
            "position is over the filtered list; total is the whole project: {title:?}"
        );
    }

    #[test]
    fn build_menu_payload_marks_the_current_session() {
        let mut duplicate = meta("other", 3);
        duplicate.id = "id-current".into();
        duplicate.project_bucket = "fedcba9876543210".into();
        let sessions = vec![meta("current", 2), duplicate];
        let current_id = sessions[0].id.clone();
        let current_bucket = sessions[0].project_bucket.clone();
        let p = SessionPicker::open(sessions);

        let payload = build_menu_payload(&p, "atomcode", Some((&current_id, &current_bucket)));

        assert!(
            payload.items[HEADER_ROWS].1.contains("current")
                || payload.items[HEADER_ROWS].1.contains("当前"),
            "current session must be visible before Enter changes the screen: {:?}",
            payload.items
        );
        assert!(
            !payload.items[HEADER_ROWS + 1].1.contains("current")
                && !payload.items[HEADER_ROWS + 1].1.contains("当前"),
            "only the active session may carry the current marker: {:?}",
            payload.items
        );
    }

    #[test]
    fn selected_preview_enriches_only_the_selected_descriptor() {
        let p = SessionPicker::open(vec![meta("alpha", 3), meta("beta", 5)]);
        let selected = p.chosen_session().unwrap();
        let selection = crate::session::SessionPreviewSelection {
            project_bucket: selected.project_bucket.clone(),
            session_id: selected.id.clone(),
            generation: 9,
        };
        let result = Ok(Some(
            atomcode_daemon::legacy_convert::CatalogSessionPreview {
                session_id: selected.id.clone(),
                project_bucket: selected.project_bucket.clone(),
                provider_id: Some("provider".into()),
                model_id: Some("model".into()),
                excerpt: vec!["first".into(), "second".into()],
            },
        ));

        let payload = build_menu_payload_with_preview(
            &p,
            "project",
            None,
            Some(&selection),
            Some(&result),
        );
        let selected_desc = &payload.items[HEADER_ROWS].1;
        assert!(selected_desc.contains("provider · model"));
        assert!(selected_desc.ends_with("first\nsecond"));
        assert!(!payload.items[HEADER_ROWS + 1].1.contains('\n'));
    }

    // On /resume, todowrite calls no longer render an inline block. Instead the
    // persistent panel is seeded from the transcript (last valid todowrite wins),
    // and successful todowrite tool RESULTs are suppressed. Normal tool rows and
    // error results are unaffected.
    #[test]
    fn replay_seeds_todo_panel_and_suppresses_todowrite_result() {
        use atomcode_kernel::message::Message;
        use atomcode_kernel::tool::ToolCall;

        #[derive(Default)]
        struct Rec {
            lines: Vec<UiLine>,
        }
        impl Renderer for Rec {
            fn render(&mut self, line: UiLine) {
                self.lines.push(line);
            }
            fn flush(&mut self) {}
            fn shutdown(&mut self) {}
            fn reset(&mut self) {}
            fn clear_screen(&mut self) {}
            fn suspend_for_external(&mut self) {}
            fn resume_from_external(&mut self) {}
            fn flush_deferred(&mut self) {}
        }

        let mut session = Session::new(PathBuf::from("/tmp/x"));
        session.messages = vec![
            Message::user("go"),
            // Single-call steps (len == 1) so this test stays focused on the
            // standalone todowrite-suppression path; parallel-batch rebuild has
            // its own test (`replay_rebuilds_parallel_batch_group`).
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "t1".into(),
                    name: "todowrite".into(),
                    arguments: r#"{"todos":[{"content":"task A","status":"in_progress"},{"content":"task B","status":"pending"}]}"#.into(),
                }],
            ),
            // `is_error = !success`: a successful tool result is `false`.
            Message::tool_result("t1", "[~] task A\n[ ] task B", false),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "r1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"x"}"#.into(),
                }],
            ),
            Message::tool_result("r1", "file contents", false),
            // A FAILED todowrite (is_error=true): its error result must NOT be
            // suppressed (parity with live's `… && success` suppression), and
            // its requested state must not replace the last successful panel.
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "t2".into(),
                    name: "todowrite".into(),
                    arguments: r#"{"todos":[{"content":"task C","status":"pending"}]}"#.into(),
                }],
            ),
            Message::tool_result("t2", "error: bad todo item", true),
        ];

        let mut state = UiState::with_unicode(true);
        let mut rec = Rec::default();
        replay_session(&mut rec, &mut state, &session, false);

        let cmd_out: Vec<&str> = rec
            .lines
            .iter()
            .filter_map(|l| match l {
                UiLine::CommandOutput(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !cmd_out
                .iter()
                .any(|s| s.contains("task A") || s.contains("task B") || s.contains("task C")),
            "todowrite no longer renders an inline block on replay: {cmd_out:?}"
        );
        // Panel stays on the last SUCCESSFUL todowrite (task A/B); the rejected
        // task C call is not allowed to resurrect during transcript replay.
        let panel = state
            .active_todos
            .clone()
            .expect("panel seeded from transcript");
        assert_eq!(
            panel.total, 2,
            "failed task C call must preserve the prior task A/B panel: {panel:?}"
        );

        let tool_call_names: Vec<&str> = rec
            .lines
            .iter()
            .filter_map(|l| match l {
                UiLine::ToolCall { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !tool_call_names
                .iter()
                .any(|n| n.to_lowercase().contains("todo")),
            "todowrite must NOT render as a generic tool row: {tool_call_names:?}"
        );
        assert_eq!(
            tool_call_names.len(),
            1,
            "only the non-todowrite call is a tool row: {tool_call_names:?}"
        );

        let results: Vec<&str> = rec
            .lines
            .iter()
            .filter_map(|l| match l {
                UiLine::ToolResult { summary, .. } => Some(summary.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !results
                .iter()
                .any(|s| s.contains("task A") || s.contains("[~]")),
            "todowrite result must be suppressed: {results:?}"
        );
        assert!(
            results.iter().any(|s| s.contains("file contents")),
            "the read tool's result is still shown: {results:?}"
        );
        assert!(
            results.iter().any(|s| s.contains("error: bad todo item")),
            "a FAILED todowrite's error result must NOT be suppressed: {results:?}"
        );
    }

    #[test]
    fn replay_rebuilds_parallel_batch_group() {
        use atomcode_kernel::message::Message;
        use atomcode_kernel::tool::ToolCall;

        #[derive(Default)]
        struct Rec {
            lines: Vec<UiLine>,
        }
        impl Renderer for Rec {
            fn render(&mut self, line: UiLine) {
                self.lines.push(line);
            }
            fn flush(&mut self) {}
            fn shutdown(&mut self) {}
            fn reset(&mut self) {}
            fn clear_screen(&mut self) {}
            fn suspend_for_external(&mut self) {}
            fn resume_from_external(&mut self) {}
            fn flush_deferred(&mut self) {}
        }

        let mut session = Session::new(PathBuf::from("/tmp/x"));
        // One assistant step with TWO tool calls == a parallel batch live.
        session.messages = vec![
            Message::user("go"),
            Message::assistant(
                "",
                vec![
                    ToolCall {
                        id: "b1".into(),
                        name: "bash".into(),
                        arguments: r#"{"command":"echo hi"}"#.into(),
                    },
                    ToolCall {
                        id: "b2".into(),
                        name: "bash".into(),
                        arguments: r#"{"command":"ls -la"}"#.into(),
                    },
                ],
            ),
            Message::tool_result("b1", "hi", false),
            Message::tool_result("b2", "a\nb\nc", false),
        ];

        let mut state = UiState::with_unicode(true);
        let mut rec = Rec::default();
        replay_session(&mut rec, &mut state, &session, false);

        // The two calls collapse into ONE grouped batch header, not two
        // standalone ToolCall rows.
        let groups: Vec<(&str, usize)> = rec
            .lines
            .iter()
            .filter_map(|l| match l {
                UiLine::ToolGroupRender {
                    header, children, ..
                } => Some((header.as_str(), children.len())),
                _ => None,
            })
            .collect();
        assert_eq!(groups.len(), 1, "one rebuilt batch group: {groups:?}");
        assert_eq!(groups[0].1, 2, "both calls are children: {groups:?}");
        assert!(
            groups[0].0.contains("Running 2 bash calls"),
            "same-name batch uses the tool label: {:?}",
            groups[0].0
        );

        assert!(
            !rec.lines
                .iter()
                .any(|l| matches!(l, UiLine::ToolCall { .. })),
            "batched calls must NOT also render as standalone tool rows"
        );
        // Each result is folded into its child (→ N lines), so no standalone
        // ToolResult rows survive for the batched calls.
        assert!(
            !rec.lines
                .iter()
                .any(|l| matches!(l, UiLine::ToolResult { .. })),
            "batched results are folded into child rows, not shown expanded"
        );
    }

    #[test]
    fn replay_restores_batched_edit_diffs_in_call_order() {
        use atomcode_kernel::message::Message;
        use atomcode_kernel::tool::ToolCall;

        #[derive(Default)]
        struct Rec {
            lines: Vec<UiLine>,
        }
        impl Renderer for Rec {
            fn render(&mut self, line: UiLine) {
                self.lines.push(line);
            }
            fn flush(&mut self) {}
            fn shutdown(&mut self) {}
            fn reset(&mut self) {}
            fn clear_screen(&mut self) {}
            fn suspend_for_external(&mut self) {}
            fn resume_from_external(&mut self) {}
            fn flush_deferred(&mut self) {}
        }

        let mut session = Session::new(PathBuf::from("/tmp/x"));
        session.messages = vec![
            Message::user("edit both"),
            Message::assistant(
                "",
                vec![
                    ToolCall {
                        id: "e1".into(),
                        name: "edit_file".into(),
                        arguments: r#"{"file_path":"src/a.rs"}"#.into(),
                    },
                    ToolCall {
                        id: "e2".into(),
                        name: "edit_file".into(),
                        arguments: r#"{"file_path":"src/b.rs"}"#.into(),
                    },
                ],
            ),
            Message::tool_result(
                "e1",
                "Edited src/a.rs (1 replacement)\n@@ -1 +1 @@\n-old-a\n+new-a",
                false,
            ),
            Message::tool_result(
                "e2",
                "Edited src/b.rs (1 replacement)\n@@ -1 +1 @@\n-old-b\n+new-b",
                false,
            ),
        ];

        let mut state = UiState::with_unicode(true);
        let mut rec = Rec::default();
        replay_session(&mut rec, &mut state, &session, false);

        let summaries: Vec<&str> = rec
            .lines
            .iter()
            .filter_map(|line| match line {
                UiLine::ToolResult { summary, .. } => Some(summary.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(summaries, vec!["Edited src/a.rs", "Edited src/b.rs"]);
        assert_eq!(
            rec.lines
                .iter()
                .filter(|line| matches!(line, UiLine::EditDiffBlock(_)))
                .count(),
            2
        );
    }

    #[test]
    fn replay_does_not_group_duplicate_calls_the_kernel_collapsed() {
        use atomcode_kernel::message::Message;
        use atomcode_kernel::tool::ToolCall;

        #[derive(Default)]
        struct Rec {
            lines: Vec<UiLine>,
        }
        impl Renderer for Rec {
            fn render(&mut self, line: UiLine) {
                self.lines.push(line);
            }
            fn flush(&mut self) {}
            fn shutdown(&mut self) {}
            fn reset(&mut self) {}
            fn clear_screen(&mut self) {}
            fn suspend_for_external(&mut self) {}
            fn resume_from_external(&mut self) {}
            fn flush_deferred(&mut self) {}
        }

        let mut session = Session::new(PathBuf::from("/tmp/x"));
        // Two calls with identical name + (key-reordered) args collapse to ONE
        // distinct call, so the kernel emitted NO batch live. Replay must match:
        // no "Running 2 calls" group.
        session.messages = vec![
            Message::user("go"),
            Message::assistant(
                "",
                vec![
                    ToolCall {
                        id: "r1".into(),
                        name: "read".into(),
                        arguments: r#"{"path":"x","limit":5}"#.into(),
                    },
                    ToolCall {
                        id: "r2".into(),
                        name: "read".into(),
                        arguments: r#"{"limit":5,"path":"x"}"#.into(),
                    },
                ],
            ),
            Message::tool_result("r1", "contents", false),
            Message::tool_result("r2", "contents", false),
        ];

        let mut state = UiState::with_unicode(true);
        let mut rec = Rec::default();
        replay_session(&mut rec, &mut state, &session, false);

        assert!(
            !rec.lines
                .iter()
                .any(|l| matches!(l, UiLine::ToolGroupRender { .. })),
            "kernel-collapsed duplicates must NOT render as a batch group"
        );
    }

    #[test]
    fn replay_skips_synthetic_user_messages() {
        use atomcode_kernel::message::Message;

        #[derive(Default)]
        struct Rec {
            lines: Vec<UiLine>,
        }
        impl Renderer for Rec {
            fn render(&mut self, line: UiLine) {
                self.lines.push(line);
            }
            fn flush(&mut self) {}
            fn shutdown(&mut self) {}
            fn reset(&mut self) {}
            fn clear_screen(&mut self) {}
            fn suspend_for_external(&mut self) {}
            fn resume_from_external(&mut self) {}
            fn flush_deferred(&mut self) {}
        }

        let mut session = Session::new(PathBuf::from("/tmp/x"));
        session.messages = vec![
            Message::user("real prompt"),
            Message::assistant("first reply", vec![]),
            Message::synthetic_user("[SYNTAX CHECK: fix parser before continuing.]"),
            Message::assistant("second reply", vec![]),
        ];

        let mut state = UiState::with_unicode(true);
        let mut rec = Rec::default();
        replay_session(&mut rec, &mut state, &session, false);

        let users: Vec<&str> = rec
            .lines
            .iter()
            .filter_map(|line| match line {
                UiLine::User(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["real prompt"]);

        let visible_turn_separators = rec
            .lines
            .iter()
            .filter(|line| matches!(line, UiLine::TurnSeparator { .. }))
            .count();
        assert_eq!(
            visible_turn_separators, 2,
            "resume wrapper separators only; synthetic user must not add a turn divider"
        );
        assert_eq!(state.last_assistant_response, "first reply\n\nsecond reply");
    }

    #[test]
    fn replay_ignores_accounting_only_turn_positions() {
        use atomcode_kernel::message::Message;

        #[derive(Default)]
        struct Rec {
            lines: Vec<UiLine>,
        }
        impl Renderer for Rec {
            fn render(&mut self, line: UiLine) {
                self.lines.push(line);
            }
            fn flush(&mut self) {}
            fn shutdown(&mut self) {}
            fn reset(&mut self) {}
            fn clear_screen(&mut self) {}
            fn suspend_for_external(&mut self) {}
            fn resume_from_external(&mut self) {}
            fn flush_deferred(&mut self) {}
        }

        let mut session = Session::new(PathBuf::from("/tmp/x"));
        session.messages = vec![
            Message::user("q1"),
            Message::assistant("a1", vec![]),
            Message::user("q2"),
            Message::assistant("a2", vec![]),
        ];
        session.turn_stats.push(TurnStat {
            after_message: 4,
            position_valid: true,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 77,
            errored: false,
            used_tokens: 77,
            ctx_window: 100,
        });
        session.turn_stats.push(TurnStat {
            after_message: 2,
            position_valid: false,
            turn_count: 9,
            tool_call_count: 9,
            duration_ms: 9,
            total_tokens: 987_654,
            errored: false,
            used_tokens: 9,
            ctx_window: 9,
        });

        let mut state = UiState::with_unicode(true);
        let mut rec = Rec::default();
        replay_session(&mut rec, &mut state, &session, false);

        let labels: Vec<&str> = rec
            .lines
            .iter()
            .filter_map(|line| match line {
                UiLine::TurnSeparator { label } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            labels.iter().all(|label| !label.contains("987")),
            "accounting-only stat leaked into replay divider: {labels:?}"
        );
        let context = state.last_context.as_ref().expect("valid context restored");
        assert_eq!(context.sent_tokens, 77);
        assert_eq!(context.ctx_window, 100);
    }
}
#[tokio::test]
async fn preparation_join_failure_becomes_an_error_terminal() {
    let joined = tokio::task::spawn_blocking(|| -> Result<(), String> {
        panic!("preparation panic");
    })
    .await;

    let error = flatten_session_preparation(joined).unwrap_err();

    assert!(error.contains("session preparation task failed"));
    assert!(error.contains("panicked"));
}
