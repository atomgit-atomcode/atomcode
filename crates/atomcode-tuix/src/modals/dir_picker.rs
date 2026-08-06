// crates/atomcode-tuix/src/modals/dir_picker.rs
//
// `/cd` (no argument) modal — searchable project-directory picker.
//
// Shows current/MRU/catalog directories in `/resume`-style half-screen chrome:
// title, bordered search/path box, scrollable list, and bottom key legend.

use std::path::PathBuf;

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};

use super::{Modal, ModalAction};
use crate::event_loop::commands::apply_cd;
use crate::event_loop::{build_status, Buffer, LoopCtx};
use crate::render::{MenuPayload, Renderer, UiLine};
use crate::state::UiState;

pub struct DirPicker {
    /// Snapshot of all known project dirs at open time. Catalog projects keep
    /// activity order; current/MRU-only directories remain available too.
    pub dirs: Vec<PathBuf>,
    /// The working dir at open time — used to label the matching entry
    /// as `(current)` so users can tell which one they're already on.
    pub current: PathBuf,
    /// Index into the filtered directory list.
    pub selected: usize,
    /// Search text or a new path. Enter opens the selected match when one exists;
    /// only a zero-match query is resolved as a literal `/cd <path>` target.
    pub query: String,
    tab_matches: Vec<String>,
    tab_index: usize,
}

impl DirPicker {
    pub fn open(dirs: Vec<PathBuf>, current: PathBuf) -> Self {
        Self {
            dirs,
            current,
            selected: 0,
            query: String::new(),
            tab_matches: Vec::new(),
            tab_index: 0,
        }
    }

    /// Known dirs matching the current query (case-insensitive substring on the
    /// displayed `~`-collapsed path). Empty query → all projects. When this is
    /// EMPTY but the query is non-empty (no project matches), Enter takes the query
    /// as a literal typed path, so a directory not in the recent list still works.
    fn filtered(&self) -> Vec<PathBuf> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return self.dirs.clone();
        }
        self.dirs
            .iter()
            .filter(|d| {
                crate::platform::collapse_home(&d.to_string_lossy())
                    .to_lowercase()
                    .contains(&q)
            })
            .cloned()
            .collect()
    }

    /// Append a typed character to the path query and reset the highlight to the
    /// top of the (re-filtered) list.
    fn on_char(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.reset_tab_completion();
    }

    /// Delete the last character of the path query and reset the highlight.
    fn on_backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.reset_tab_completion();
    }

    fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn down(&mut self) {
        let n = self.filtered().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        if self.selected + 1 < n {
            self.selected += 1;
        }
    }

    fn complete(&mut self, cwd: &std::path::Path) {
        if looks_like_path(&self.query) {
            if !self.tab_matches.is_empty()
                && self
                    .tab_matches
                    .get(self.tab_index)
                    .is_some_and(|value| value == &self.query)
            {
                self.tab_index = (self.tab_index + 1) % self.tab_matches.len();
                self.query = self.tab_matches[self.tab_index].clone();
                self.selected = 0;
                return;
            }
            let completions = directory_completions(&self.query, cwd);
            if let Some(completion) = best_completion(&self.query, &completions) {
                self.query = completion;
                self.selected = 0;
                self.reset_tab_completion();
            } else if !completions.is_empty() {
                self.tab_matches = completions;
                self.tab_index = 0;
                self.query = self.tab_matches[0].clone();
                self.selected = 0;
            }
            return;
        }

        if let Some(path) = self.filtered().get(self.selected) {
            self.query = crate::platform::collapse_home(&path.to_string_lossy());
            self.selected = 0;
            self.reset_tab_completion();
        }
    }

    fn reset_tab_completion(&mut self) {
        self.tab_matches.clear();
        self.tab_index = 0;
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
            KeyCode::Tab => {
                self.complete(&ctx.working_dir);
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
                let filt = self.filtered();
                let path = match resolve_enter_target(
                    &filt,
                    self.selected,
                    &self.query,
                    &ctx.working_dir,
                    ctx.previous_dir.as_deref(),
                ) {
                    Ok(Some(path)) => path,
                    Ok(None) => return Ok(ModalAction::Continue),
                    Err(error) => {
                        // Invalid ad-hoc paths stay editable in the modal.
                        renderer.render(UiLine::Error(error));
                        self.draw(buf, state, ctx, renderer);
                        return Ok(ModalAction::Continue);
                    }
                };
                if crate::event_loop::commands::paths_same(&path, &ctx.working_dir) {
                    return Ok(ModalAction::Close);
                }
                if !path.is_dir() {
                    let p = path.display().to_string();
                    renderer.render(UiLine::Error(
                        crate::i18n::t(crate::i18n::Msg::DirNotExists { path: &p }).into_owned(),
                    ));
                    renderer.flush();
                    return Ok(ModalAction::Continue);
                }
                match apply_cd(ctx, path) {
                    Ok(_) => renderer.render(UiLine::CommandOutput(
                        crate::i18n::t(crate::i18n::Msg::CmdSessionTransitionPending).into_owned(),
                    )),
                    Err(error) => {
                        renderer.render(UiLine::Error(error));
                        self.draw(buf, state, ctx, renderer);
                        return Ok(ModalAction::Continue);
                    }
                }
                renderer.flush();
                Ok(ModalAction::Close)
            }
            KeyCode::Esc => Ok(ModalAction::Close),
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
        // Paste goes into the query, not the main buffer
        for c in text.chars() {
            if c.is_control() {
                continue; // skip newlines/control characters
            }
            self.query.push(c);
        }
        self.selected = 0;
        self.reset_tab_completion();
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
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
    let filtered = p.filtered();
    let pos = if filtered.is_empty() {
        0
    } else {
        p.selected + 1
    };
    let title = crate::i18n::t(crate::i18n::Msg::DirPickerTitle {
        n: pos,
        total: p.dirs.len(),
    })
    .into_owned();
    let hint = crate::i18n::t(crate::i18n::Msg::DirPickerHint).into_owned();
    // Same fixed chrome contract as `/resume`: title, blank, bordered query,
    // blank, scrollable entries, and a sticky bottom hint.
    let mut items = vec![(title, String::new()), (String::new(), String::new())];
    items.push((p.query.clone(), String::new()));
    items.push((String::new(), String::new()));
    if filtered.is_empty() && !p.query.trim().is_empty() {
        items.push((
            crate::i18n::t(crate::i18n::Msg::DirPickerEmptyPath {
                query: p.query.trim(),
            })
            .into_owned(),
            String::new(),
        ));
    } else {
        // The working dir can appear more than once (e.g. as an absolute path and as
        // `.`), all `paths_same` to `current`. Label only the FIRST occurrence so the
        // list never shows two "current" markers.
        let mut current_labeled = false;
        items.extend(filtered.iter().map(|d| {
            let name = crate::platform::collapse_home(&d.to_string_lossy());
            let desc = if !current_labeled
                && crate::event_loop::commands::paths_same(d, &p.current)
            {
                current_labeled = true;
                crate::i18n::t(crate::i18n::Msg::DirCurrent).into_owned()
            } else {
                String::new()
            };
            (name, desc)
        }));
    }
    items.push((format!("— {hint} —"), String::new()));
    MenuPayload {
        items,
        selected: if filtered.is_empty() {
            usize::MAX
        } else {
            DIR_HEADER_ROWS + p.selected
        },
        kind: crate::render::MenuKind::DirectoryList,
    }
}

pub(crate) const DIR_HEADER_ROWS: usize = 4;

fn looks_like_path(query: &str) -> bool {
    let q = query.trim();
    q.starts_with('~')
        || q.starts_with('.')
        || q.starts_with('/')
        || q.starts_with('\\')
        || q.as_bytes().get(1) == Some(&b':')
        || q.contains('/')
        || q.contains('\\')
}

fn resolve_enter_target(
    filtered: &[PathBuf],
    selected: usize,
    query: &str,
    cwd: &std::path::Path,
    previous_dir: Option<&std::path::Path>,
) -> Result<Option<PathBuf>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(filtered.get(selected).cloned());
    }
    // An explicit path is an instruction, not a fuzzy-search hint. Resolve it
    // before consulting saved projects so `/tmp/app` cannot be shadowed by a
    // historical `/tmp/app-old` match.
    if looks_like_path(query) {
        match crate::event_loop::commands::resolve_cd(query, cwd, previous_dir) {
            // An explicit path that resolves wins outright.
            Ok(path) => return Ok(Some(path)),
            // The literal path doesn't resolve. If it was actually a search PREFIX that
            // narrowed the list to a highlighted match (e.g. "~/Des" → "~/Desktop"), take
            // that match; only surface the error when nothing is highlighted.
            Err(e) => {
                if let Some(path) = filtered.get(selected) {
                    return Ok(Some(path.clone()));
                }
                return Err(e);
            }
        }
    }
    if let Some(path) = filtered.get(selected) {
        return Ok(Some(path.clone()));
    }
    crate::event_loop::commands::resolve_cd(query, cwd, previous_dir).map(Some)
}

fn directory_completions(query: &str, cwd: &std::path::Path) -> Vec<String> {
    directory_completions_with_home(query, cwd, crate::platform::home_dir().as_deref())
}

fn directory_completions_with_home(
    query: &str,
    cwd: &std::path::Path,
    home: Option<&std::path::Path>,
) -> Vec<String> {
    let raw = query.trim();
    if raw.is_empty() {
        return Vec::new();
    }

    let separator = if raw.contains('\\') && !raw.contains('/') {
        '\\'
    } else {
        '/'
    };
    let (display_parent, leaf) = if raw == "~" {
        (format!("~{separator}"), "")
    } else if raw.ends_with(|c| c == '/' || c == '\\') {
        (raw.to_string(), "")
    } else if let Some(index) = raw.rfind(|c| c == '/' || c == '\\') {
        (raw[..=index].to_string(), &raw[index + 1..])
    } else {
        (String::new(), raw)
    };

    let parent = if display_parent == format!("~{separator}") {
        let Some(home) = home else {
            return Vec::new();
        };
        home.to_path_buf()
    } else if display_parent.starts_with(&format!("~{separator}")) {
        let Some(home) = home else {
            return Vec::new();
        };
        home.join(display_parent[2..].trim_end_matches(|c| c == '/' || c == '\\'))
    } else {
        let path = std::path::PathBuf::from(&display_parent);
        if path.as_os_str().is_empty() {
            cwd.to_path_buf()
        } else if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    };

    let leaf_folded = leaf.to_lowercase();
    let mut matches = std::fs::read_dir(parent)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.to_lowercase().starts_with(&leaf_folded))
        .map(|name| format!("{display_parent}{name}{separator}"))
        .collect::<Vec<_>>();
    matches.sort_by_key(|value| value.to_lowercase());
    matches
}

fn best_completion(query: &str, matches: &[String]) -> Option<String> {
    let first = matches.first()?.clone();
    if matches.len() == 1 {
        return Some(first);
    }
    let mut prefix = first;
    for value in &matches[1..] {
        while !value.to_lowercase().starts_with(&prefix.to_lowercase()) {
            prefix.pop();
            if prefix.is_empty() {
                break;
            }
        }
    }
    (prefix.len() > query.len()).then_some(prefix)
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
    fn query_filters_recent_list_case_insensitive() {
        // Typing narrows the recent list (case-insensitive substring on the path).
        let mut p = DirPicker::open(
            vec![pb("/tmp/alpha"), pb("/tmp/beta"), pb("/tmp/AlphaBeta")],
            pb("/tmp/alpha"),
        );
        for c in "alp".chars() {
            p.on_char(c);
        }
        assert_eq!(p.filtered(), vec![pb("/tmp/alpha"), pb("/tmp/AlphaBeta")]);
        assert_eq!(p.selected, 0, "highlight resets to the first match");
    }

    #[test]
    fn empty_query_lists_all_recents() {
        let p = DirPicker::open(vec![pb("/tmp/a"), pb("/tmp/b")], pb("/tmp/a"));
        assert_eq!(p.filtered().len(), 2);
    }

    #[test]
    fn down_clamps_to_filtered_results_not_all_dirs() {
        let mut p = DirPicker::open(
            vec![pb("/tmp/alpha"), pb("/tmp/beta"), pb("/tmp/alphabeta")],
            pb("/x"),
        );
        for c in "alp".chars() {
            p.on_char(c); // matches alpha + alphabeta (2 of 3)
        }
        p.down();
        assert_eq!(p.selected, 1);
        p.down();
        assert_eq!(
            p.selected, 1,
            "clamps to the 2 filtered results, not all 3 dirs"
        );
    }

    #[test]
    fn unmatched_query_filters_to_empty_for_typed_path_fallback() {
        let mut p = DirPicker::open(vec![pb("/tmp/alpha")], pb("/tmp/alpha"));
        for c in "zzz".chars() {
            p.on_char(c);
        }
        assert!(
            p.filtered().is_empty(),
            "no recent matches → empty list, so Enter falls back to the typed path"
        );
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
        assert_eq!(
            p.selected, 0,
            "typing re-focuses the typed path, not a recent dir"
        );
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
    fn selection_indexes_into_filtered_list() {
        // `selected` is an index into `filtered()` — the Enter handler resolves the
        // highlighted recent dir as `filtered()[selected]`.
        let mut p = DirPicker::open(vec![pb("/a"), pb("/b"), pb("/c")], pb("/a"));
        p.down();
        assert_eq!(p.filtered().get(p.selected).cloned(), Some(pb("/b")));
    }

    #[test]
    fn enter_prefers_explicit_typed_path_over_fuzzy_saved_match() {
        let root = tempfile::tempdir().unwrap();
        let desktop = root.path().join("Desktop");
        std::fs::create_dir(&desktop).unwrap();
        let app = desktop.join("app");
        std::fs::create_dir(&app).unwrap();

        let query = desktop.to_string_lossy();
        let resolved = resolve_enter_target(&[app], 0, &query, root.path(), None)
            .expect("typed path resolves")
            .expect("typed path is selected");
        assert_eq!(resolved, desktop.canonicalize().unwrap());
    }

    #[test]
    fn enter_partial_path_prefix_falls_through_to_highlighted_match() {
        // Typing a path-looking PREFIX that doesn't resolve (e.g. "~/Des" narrowing the
        // list to "~/Desktop") must select the highlighted match, not error on the
        // incomplete literal path.
        let root = tempfile::tempdir().unwrap();
        let desktop = root.path().join("Desktop");
        std::fs::create_dir(&desktop).unwrap();
        let partial = format!("{}/Des", root.path().display()); // path-looking, does NOT exist
        let resolved = resolve_enter_target(&[desktop.clone()], 0, &partial, root.path(), None)
            .expect("a partial path prefix must not error when a match is highlighted")
            .expect("the highlighted match is selected");
        assert_eq!(resolved, desktop);
    }

    #[test]
    fn enter_plain_search_uses_selected_saved_directory() {
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("project-name");
        std::fs::create_dir(&app).unwrap();
        let resolved = resolve_enter_target(&[app.clone()], 0, "project", root.path(), None)
            .expect("plain search resolves")
            .expect("saved directory is selected");
        assert_eq!(resolved, app);
    }

    #[test]
    fn enter_zero_match_resolves_query_as_new_path() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("new-project");
        std::fs::create_dir(&target).unwrap();
        let resolved = resolve_enter_target(&[], 0, target.to_str().unwrap(), root.path(), None)
            .expect("zero-match path resolves");
        assert_eq!(resolved, Some(target.canonicalize().unwrap()));
    }

    #[test]
    fn enter_zero_match_invalid_path_returns_error() {
        let root = tempfile::tempdir().unwrap();
        let result = resolve_enter_target(&[], 0, "./missing/project", root.path(), None);
        assert!(result.is_err());
    }

    #[test]
    fn menu_payload_marks_current_dir() {
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        let p = DirPicker::open(vec![pb("/a"), pb("/b")], pb("/b"));
        let payload = build_menu_payload(&p);
        assert_eq!(payload.items[DIR_HEADER_ROWS].1, "");
        assert_eq!(payload.items[DIR_HEADER_ROWS + 1].1, "current");
    }

    #[test]
    fn menu_payload_marks_only_the_first_current_dir() {
        let _locale = crate::i18n::test_lock();
        crate::i18n::set_locale(crate::i18n::Locale::En);
        // The working dir can appear twice in the list (e.g. as an absolute path and
        // as `.`), and both would be `paths_same` to `current`. Only ONE entry may
        // carry the "current" label — two "current" markers is a bug.
        let p = DirPicker::open(vec![pb("/proj"), pb("/proj")], pb("/proj"));
        let payload = build_menu_payload(&p);
        assert_eq!(payload.items[DIR_HEADER_ROWS].1, "current", "first match labeled");
        assert_eq!(
            payload.items[DIR_HEADER_ROWS + 1].1,
            "",
            "the aliased second occurrence must NOT be labeled current"
        );
    }

    #[test]
    fn menu_payload_keeps_all_projects_for_renderer_pagination() {
        let dirs = (0..12).map(|n| pb(&format!("/p{n}"))).collect();
        let mut p = DirPicker::open(dirs, pb("/p0"));
        p.selected = 9;
        let payload = build_menu_payload(&p);
        assert_eq!(payload.items.len(), DIR_HEADER_ROWS + 12 + 1);
        assert_eq!(payload.selected, DIR_HEADER_ROWS + 9);
        assert_eq!(payload.kind, crate::render::MenuKind::DirectoryList);
    }

    #[test]
    fn tab_on_search_fills_selected_project_path() {
        let mut p = DirPicker::open(vec![pb("/alpha"), pb("/beta")], pb("/alpha"));
        p.query = "bet".into();
        p.complete(std::path::Path::new("/"));
        assert_eq!(p.query, "/beta");
    }

    #[test]
    fn path_completion_lists_only_matching_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("Desktop")).unwrap();
        std::fs::create_dir(root.path().join("Documents")).unwrap();
        std::fs::write(root.path().join("Desk.txt"), "not a directory").unwrap();

        let matches = directory_completions_with_home("~/De", root.path(), Some(root.path()));
        assert_eq!(matches, vec!["~/Desktop/"]);
    }

    #[test]
    fn path_completion_extends_to_common_directory_prefix() {
        let matches = vec!["~/Documents/".to_string(), "~/Downloads/".to_string()];
        assert_eq!(best_completion("~/Do", &matches), None);
        assert_eq!(best_completion("~/D", &matches), Some("~/Do".to_string()));
    }

    #[test]
    fn repeated_tab_cycles_ambiguous_directory_matches() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("Documents")).unwrap();
        std::fs::create_dir(root.path().join("Downloads")).unwrap();
        let mut p = DirPicker::open(Vec::new(), root.path().to_path_buf());
        p.query = "./Do".into();

        p.complete(root.path());
        assert_eq!(p.query, "./Documents/");
        p.complete(root.path());
        assert_eq!(p.query, "./Downloads/");
        p.complete(root.path());
        assert_eq!(p.query, "./Documents/");
    }
}
