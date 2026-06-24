// crates/atomcode-tuix/src/modals/dir_picker.rs
//
// `/cd` (no argument) modal — recent-project-dirs picker.
//
// Lists the up-to-5 most recently visited project directories from
// `ctx.recent_dirs` (backed by `~/.atomcode/recent_dirs.txt`). Up/Down
// navigates, Enter commits the cd via `apply_cd`, Esc cancels.

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::commands::apply_cd;
use crate::event_loop::{build_status, Buffer, LoopCtx};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

pub struct DirPicker {
    /// Snapshot of recent dirs at open time. Already in "most recent
    /// first" order; includes the current working directory at index 0
    /// (seeded at startup / refreshed on every `apply_cd`).
    pub dirs: Vec<PathBuf>,
    /// The working dir at open time — used to label the matching entry
    /// as `(current)` so users can tell which one they're already on.
    pub current: PathBuf,
    /// Index into `dirs`.
    pub selected: usize,
    /// Free-text path the user has typed. When non-empty, Enter jumps to THIS
    /// path (resolved like `/cd <path>`) instead of the highlighted recent dir —
    /// so a directory that isn't in the recent list can still be entered directly.
    pub query: String,
}

impl DirPicker {
    pub fn open(dirs: Vec<PathBuf>, current: PathBuf) -> Self {
        Self {
            dirs,
            current,
            selected: 0,
            query: String::new(),
        }
    }

    /// Append a typed character to the path query and reset the recent-list
    /// highlight (the typed path is now the primary Enter target).
    fn on_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
    }

    /// Delete the last character of the path query.
    fn on_backspace(&mut self) {
        self.query.pop();
    }

    fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn down(&mut self) {
        if self.dirs.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.dirs.len() - 1;
        if self.selected < max {
            self.selected += 1;
        }
    }

    fn chosen(&self) -> Option<PathBuf> {
        self.dirs.get(self.selected).cloned()
    }
}

impl Modal for DirPicker {
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
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Down => {
                self.down();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            // Free-text path entry: plain printable chars build the `query` (Ctrl/Alt
            // combos are left for shortcuts, not captured as text).
            KeyCode::Char(c)
                if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) =>
            {
                self.on_char(c);
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Backspace => {
                self.on_backspace();
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
            KeyCode::Enter => {
                // A typed query takes precedence: resolve+validate+canonicalize it
                // exactly like `/cd <path>` (handles `~`, `~\`/`~/`, `-`, absolute,
                // and cwd-relative), so a directory NOT in the recent list works.
                let query = self.query.trim();
                if !query.is_empty() {
                    // Resolve the typed path EXACTLY like `/cd <path>` — and the
                    // command path trims its arg, so trim here too for parity.
                    match crate::event_loop::commands::resolve_cd(
                        query,
                        &ctx.working_dir,
                        ctx.previous_dir.as_deref(),
                    ) {
                        Ok(path) => {
                            if path != ctx.working_dir {
                                apply_cd(ctx, path.clone());
                                let p = path.display().to_string();
                                renderer.render(UiLine::CommandOutput(
                                    crate::i18n::t(crate::i18n::Msg::DirChanged { path: &p })
                                        .into_owned(),
                                ));
                            }
                            renderer.flush();
                            return Ok(ModalAction::Close);
                        }
                        Err(e) => {
                            // A typo shouldn't dismiss the picker — show the error
                            // but KEEP it open (redraw) so the query can be fixed.
                            renderer.render(UiLine::Error(e));
                            self.draw(buf, state, ctx, renderer);
                            return Ok(ModalAction::Continue);
                        }
                    }
                }
                let Some(path) = self.chosen() else {
                    return Ok(ModalAction::Continue);
                };
                if path == ctx.working_dir {
                    // No-op cd: skip the agent round-trip but still close
                    // the picker so the user isn't stuck inside it.
                    return Ok(ModalAction::Close);
                }
                if !path.is_dir() {
                    let p = path.display().to_string();
                    renderer.render(UiLine::Error(
                        crate::i18n::t(crate::i18n::Msg::DirNotExists { path: &p }).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(ModalAction::Close);
                }
                apply_cd(ctx, path.clone());
                let p = path.display().to_string();
                renderer.render(UiLine::CommandOutput(
                    crate::i18n::t(crate::i18n::Msg::DirChanged { path: &p }).into_owned(),
                ));
                renderer.flush();
                Ok(ModalAction::Close)
            }
            KeyCode::Esc => Ok(ModalAction::Close),
            _ => Ok(ModalAction::Continue),
        }
    }

    fn draw(&self, _buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let payload = build_menu_payload(self);
        // Show the typed path query as the editable input line (not the main
        // buffer, which stays untouched while the modal is open).
        renderer.render(UiLine::InputPrompt {
            buf: self.query.clone(),
            cursor_byte: self.query.len(),
            menu: Some(payload),
            status: build_status(state, ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
    }
}

fn build_menu_payload(p: &DirPicker) -> MenuPayload {
    let items: Vec<(String, String)> = p
        .dirs
        .iter()
        .map(|d| {
            let name = crate::platform::collapse_home(&d.to_string_lossy());
            let desc = if d == &p.current {
                crate::i18n::t(crate::i18n::Msg::DirCurrent).into_owned()
            } else {
                String::new()
            };
            (name, desc)
        })
        .collect();
    MenuPayload {
        items,
        selected: p.selected,
            kind: crate::render::MenuKind::TwoColumn { row_prefix: "", selected_marker: "▸" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn open_seeds_selection_at_zero() {
        let p = DirPicker::open(vec![pb("/a"), pb("/b")], pb("/a"));
        assert_eq!(p.selected, 0);
        assert_eq!(p.dirs.len(), 2);
    }

    #[test]
    fn typing_builds_query_and_resets_highlight() {
        // The over-arching fix: you can type a path that isn't in the recent list.
        let mut p = DirPicker::open(vec![pb("/a"), pb("/b")], pb("/a"));
        p.selected = 1;
        p.on_char('~');
        p.on_char('/');
        p.on_char('x');
        assert_eq!(p.query, "~/x");
        assert_eq!(p.selected, 0, "typing re-focuses the typed path, not a recent dir");
        p.on_backspace();
        assert_eq!(p.query, "~/");
    }

    #[test]
    fn down_and_up_stay_within_bounds() {
        let mut p = DirPicker::open(vec![pb("/a"), pb("/b")], pb("/a"));
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
    fn chosen_returns_selected_path() {
        let mut p = DirPicker::open(vec![pb("/a"), pb("/b"), pb("/c")], pb("/a"));
        p.down();
        assert_eq!(p.chosen(), Some(pb("/b")));
    }

    #[test]
    fn menu_payload_marks_current_dir() {
        let p = DirPicker::open(vec![pb("/a"), pb("/b")], pb("/b"));
        let payload = build_menu_payload(&p);
        assert_eq!(payload.items[0].1, "");
        assert_eq!(payload.items[1].1, "current");
    }
}
