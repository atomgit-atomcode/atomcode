//! The settings the screen shows, and the port that changes them.
//!
//! Two halves, and the split is the point:
//!
//! - **What a setting *is*** — its label, its current value, what it accepts,
//!   when a change takes effect — arrives as plain data from whoever reads the
//!   configuration. This crate does not read it and cannot: the file, its
//!   schema and the reload that applies a change all belong to the product, not
//!   to a screen (`docs/adr/0022` §3 — the screen is an App apart).
//! - **How it is *drawn and worked*** — the list, the search box, the keys, and
//!   the composer the panel stands in for — is this crate's, because that is
//!   exactly what it is.
//!
//! So the seam is one port. A launcher fills it with an implementation that
//! reads the configuration and writes it back; the screen asks for rows and
//! hands back an edit. Neither learns the other's types.
//!
//! [`SettingRow`] is deliberately the *presentation* of a setting and not the
//! setting: `label` is already the word to draw in the language to draw it in,
//! `value` is already the text a person reads, and [`SettingKind`] says only
//! which gesture edits it — cycle, type, or confirm. A host that handed over a
//! config enum would have handed this crate the product's schema, and the screen
//! would have to be edited every time a setting was added.

use std::sync::Arc;

/// What a value is, as far as editing it goes.
///
/// Not a type system for settings — a description of the *gesture*. Three
/// gestures cover every setting in the catalog: confirm it (a boolean flips),
/// pick from a list (a choice cycles), and type it (a number or a path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingKind {
    /// `true` / `false`. Confirming flips it.
    Boolean,
    /// `true` / `false` / unset, cycled in that order. Unset is a third state
    /// and not a synonym for `false`: it means "decide for me", which is what
    /// the product does with it.
    OptionalBoolean,
    /// A number, inclusive. Rendered as typed text so an empty field can mean
    /// "unset".
    Integer { min: i64, max: i64 },
    /// One of these strings, cycled. Never empty — a choice with nothing to
    /// choose is a setting that cannot be set, and the host has no business
    /// offering it.
    Choice(Vec<String>),
    /// Free text.
    Text,
}

impl SettingKind {
    /// The value one step on from `current` under a confirm, when the kind has
    /// a cycle at all.
    ///
    /// `None` for the kinds that are typed rather than cycled: an integer has no
    /// successor, and a text field has nothing to cycle through. The caller
    /// opens an editor for those instead — see [`crate::modules::settings`].
    pub fn cycled(&self, current: &str) -> Option<String> {
        match self {
            Self::Boolean => Some(if current == "true" { "false" } else { "true" }.to_string()),
            // Unset first, so the cycle ends where it started: a person who
            // lands on `auto` and confirms again gets `true`, not a dead key.
            Self::OptionalBoolean => Some(
                match current {
                    "auto" => "enabled",
                    "enabled" => "disabled",
                    _ => "auto",
                }
                .to_string(),
            ),
            Self::Choice(values) => {
                if values.is_empty() {
                    return None;
                }
                let at = values.iter().position(|v| v == current);
                let next = match at {
                    Some(i) => (i + 1) % values.len(),
                    // A value not in the list is one this build does not offer —
                    // a file edited by hand, or a setting that grew an option.
                    // Starting at the first is the only honest move: the current
                    // value is not something this list can offer, and leaving the
                    // key dead would hide that.
                    None => 0,
                };
                Some(values[next].clone())
            }
            Self::Integer { .. } | Self::Text => None,
        }
    }

    /// Whether confirming this kind opens an editor rather than cycling it.
    pub fn needs_typing(&self) -> bool {
        matches!(self, Self::Integer { .. } | Self::Text)
    }
}

/// When a change takes effect, as the person changing it is owed the answer.
///
/// The wording is this crate's ([`Applies::say`]); the fact is the host's. A
/// host that sent a sentence would be writing the screen's prose for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applies {
    /// The screen changes now; nothing is rebuilt.
    Immediately,
    /// From the next turn on.
    NextTurn,
    /// The agent is reassembled around the new configuration.
    Reload,
    /// The capability graph is rebuilt.
    Reprepare,
    /// Only after a restart.
    Restart,
}

impl Applies {
    pub fn say(self) -> &'static str {
        match self {
            Self::Immediately => "立即",
            Self::NextTurn => "下一轮",
            Self::Reload => "重新加载",
            Self::Reprepare => "重建能力",
            Self::Restart => "重启后",
        }
    }
}

/// One setting, as it is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingRow {
    /// What a change is sent back under. Stable and unique — it is the key the
    /// panel writes with, so two rows sharing one would make the second
    /// uneditable and the first editable by the wrong row.
    pub id: String,
    /// The word to draw.
    pub label: String,
    /// What it is now, as text. Empty is a value like any other: it is what an
    /// unset optional setting looks like.
    pub value: String,
    pub kind: SettingKind,
    pub applies: Applies,
}

/// The settings, as of one frame.
///
/// Immutable and cheap to clone ([`Arc`]), so a `Moment` can carry it and two
/// renders against that moment see the same list — the promise
/// [`crate::raster::RastersView`] keeps for bitmaps and `caps` keeps for the
/// terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SettingsView(Arc<Vec<SettingRow>>);

impl SettingsView {
    pub fn new(rows: Vec<SettingRow>) -> Self {
        Self(Arc::new(rows))
    }

    pub fn rows(&self) -> &[SettingRow] {
        &self.0
    }

    /// The rows matching what has been typed, in the order the host gave them.
    ///
    /// Matched on id, label and value, all case-insensitively, because those are
    /// the three things a person can see on the row: searching for something on
    /// screen and not finding it is the failure this rules out.
    pub fn matching(&self, query: &str) -> Vec<&SettingRow> {
        let q = query.trim().to_lowercase();
        self.0
            .iter()
            .filter(|row| {
                q.is_empty()
                    || row.id.to_lowercase().contains(&q)
                    || row.label.to_lowercase().contains(&q)
                    || row.value.to_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// A value being typed into a row.
///
/// Carries the row's id, not its index: the list re-filters while an edit is
/// open whenever the search box has focus, and an index would then point at
/// whatever slid into that slot. The id says which setting the text is for, and
/// survives any reordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub id: String,
    pub value: String,
}

/// What the panel is doing while it is up.
///
/// Not folded from facts — a panel is not a fact, and the log records what was
/// changed, never that a search box had four characters in it. It travels in
/// [`crate::moment::Moment`] so a `View` can render it without reaching a
/// service, which is the same road `asking` travels.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Panel {
    /// What is typed in the search box.
    pub query: String,
    /// The row the arrows are on, as an index into the *filtered* rows.
    pub cursor: usize,
    /// The measure typed into the search box.
    ///
    /// There is one field and one caret. An earlier version had a `searching`
    /// flag and a caret that moved between two boxes, and it was wrong in the
    /// way a panel cannot afford: the key that opened the search box ate the
    /// character that opened it, so searching for a path lost its leading slash
    /// and the list went blank under the person's hands. A panel whose whole
    /// point is a filter does not need a mode for the filter — it is *in* it.
    pub query_caret: usize,
    /// The row being typed into, when one is.
    pub editing: Option<Edit>,
}

impl Panel {
    /// A panel just opened: no search, first row pointed at, nothing being
    /// edited.
    pub fn new() -> Self {
        Self::default()
    }

    /// Move the highlight by `delta` rows, clamped to the ones there are.
    ///
    /// Clamped rather than wrapped, for the reason `Ask::point_at` clamps: a
    /// highlight that jumps from the last row to the first reads as a slip.
    pub fn move_by(&mut self, delta: i32, rows: usize) -> bool {
        let last = rows.saturating_sub(1) as i32;
        let next = (self.cursor as i32 + delta).clamp(0, last.max(0)) as usize;
        if next == self.cursor {
            return false;
        }
        self.cursor = next;
        true
    }

    /// Point at a row by index, clamped. True when it moved.
    pub fn point_at(&mut self, row: usize, rows: usize) -> bool {
        let next = row.min(rows.saturating_sub(1));
        if next == self.cursor {
            return false;
        }
        self.cursor = next;
        true
    }

    /// Type into the search box. The highlight goes back to the first row, for
    /// the reason `Picker` does it: the list under the cursor just changed, so
    /// staying at the same index would be pointing at a different setting.
    pub fn type_into_search(&mut self, c: char) -> bool {
        self.query.insert(self.query_caret, c);
        self.query_caret += c.len_utf8();
        self.cursor = 0;
        true
    }

    /// Take one character back out of the search box, by character rather than
    /// by byte — the box holds whatever was typed, including words that are not
    /// one byte per letter.
    pub fn backspace_search(&mut self) -> bool {
        if self.query_caret == 0 {
            return false;
        }
        let before = &self.query[..self.query_caret];
        let Some((at, _)) = before.char_indices().next_back() else {
            return false;
        };
        self.query.remove(at);
        self.query_caret = at;
        self.cursor = 0;
        true
    }

    /// Empty the search box, and put the highlight back on the first row.
    ///
    /// What Escape does before it closes the panel: the unfiltered list is what
    /// a person who has searched themselves into a corner wants back, and
    /// closing would take the panel away with the search.
    pub fn clear_search(&mut self) -> bool {
        if self.query.is_empty() && self.query_caret == 0 {
            return false;
        }
        self.query.clear();
        self.query_caret = 0;
        self.cursor = 0;
        true
    }

    /// Whether the row at `row` is the one the arrows are on.
    pub fn is_pointed_at(&self, row: usize) -> bool {
        row == self.cursor
    }
}

/// What one key did to the panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// The panel changed; nothing outside it did.
    Stay,
    /// Put the panel away.
    Close,
    /// Send this change over the seam. The caller owns the write, because only
    /// it can reach the [`Settings`] port.
    Set { id: String, value: String },
}

/// Run one key against the panel.
///
/// Free-standing and pure so it can be tested against a `Panel` and a
/// [`SettingsView`] with no screen: this is the part with the branching — which
/// field has the keyboard, whether a confirm cycles or opens an editor — and the
/// part that is wrong in ways a screenshot cannot show.
///
/// **While a field is being edited, nothing else takes a key.** An arrow that
/// walked the list under the text being typed would change which setting the
/// text is for, and the person would not find out until they saved.
pub fn key(view: &SettingsView, panel: &mut Panel, press: crate::surface::KeyPress) -> Step {
    use crate::surface::{Key, Mods};

    if panel.editing.is_some() {
        return match (press.key, press.mods) {
            // Esc and ctrl-c abandon the edit. The value that was there stays —
            // a field that wrote half a number back on the way out would change
            // a setting nobody confirmed.
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
                panel.editing = None;
                Step::Stay
            }
            (Key::Enter, _) => match panel.editing.take() {
                Some(edit) => Step::Set {
                    id: edit.id,
                    value: edit.value,
                },
                None => Step::Stay,
            },
            (Key::Backspace, _) => {
                if let Some(edit) = panel.editing.as_mut() {
                    edit.value.pop();
                }
                Step::Stay
            }
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
                if let Some(edit) = panel.editing.as_mut() {
                    edit.value.push(c);
                }
                Step::Stay
            }
            _ => Step::Stay,
        };
    }

    let shown = view.matching(&panel.query);
    match (press.key, press.mods) {
        // Escape does the innermost thing, the same rule the composer's Escape
        // follows: out of what is typed, then out of the panel. Clearing first
        // is what makes a filter recoverable — a person who has narrowed the
        // list to nothing wants the unfiltered list back, and closing the panel
        // would throw the panel away with the search.
        (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
            if !panel.query.is_empty() {
                panel.clear_search();
                Step::Stay
            } else {
                Step::Close
            }
        }
        (Key::Up, _) | (Key::Char('k'), Mods::CTRL) => {
            panel.move_by(-1, shown.len());
            Step::Stay
        }
        (Key::Down, _) | (Key::Char('j'), Mods::CTRL) => {
            panel.move_by(1, shown.len());
            Step::Stay
        }
        (Key::PageUp, _) => {
            panel.move_by(-10, shown.len());
            Step::Stay
        }
        (Key::PageDown, _) => {
            panel.move_by(10, shown.len());
            Step::Stay
        }
        (Key::Backspace, _) => {
            panel.backspace_search();
            Step::Stay
        }
        // An ordinary character goes into the search box, and the list narrows
        // as it is typed. **Every** ordinary character, and that is the point:
        // there is no "key that opens the search box", so there is no key whose
        // own character has to be swallowed to open it. `/` is a slash here
        // like anywhere else — which is what makes searching for a path
        // possible at all.
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            panel.type_into_search(c);
            Step::Stay
        }
        // Confirm the highlighted row: cycle it, or open an editor when the kind
        // has no cycle to offer.
        (Key::Enter, _) => {
            let Some(row) = shown.get(panel.cursor) else {
                return Step::Stay;
            };
            if row.kind.needs_typing() {
                panel.editing = Some(Edit {
                    id: row.id.clone(),
                    value: row.value.clone(),
                });
                Step::Stay
            } else {
                match row.kind.cycled(&row.value) {
                    Some(value) => Step::Set {
                        id: row.id.clone(),
                        value,
                    },
                    // A kind with neither a cycle nor typing is a setting this
                    // build cannot change; the row stays, the key does nothing,
                    // and nothing pretends otherwise.
                    None => Step::Stay,
                }
            }
        }
        _ => Step::Stay,
    }
}

/// Reading the settings, and changing one.
///
/// Filled by whoever launches the screen, taken by the screen when it runs — the
/// same shape as the agent connection (`crate::plugin::ConnectionSvc`), and for
/// the same reason: what the screen knows about the product came over a seam,
/// not out of a service of the product's own (`docs/adr/0022` §3).
pub trait Settings: Send + Sync {
    /// The settings as they are now.
    ///
    /// Asked again when the panel opens and after every change, so a file edited
    /// behind the screen's back is whatever the file says rather than what was
    /// true when the session started.
    fn rows(&self) -> SettingsView;

    /// Set `id` to `value`, and answer with the settings as they are after it.
    ///
    /// `Err` is a refusal — a value the host will not take, a file it cannot
    /// write — and the message is drawn as it stands. Answering with the rows
    /// rather than `()` is not politeness: a change may take effect by rebuilding
    /// the agent, and re-reading is what tells the panel whether it did.
    fn set(&self, id: &str, value: &str) -> Result<SettingsView, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, label: &str, value: &str, kind: SettingKind) -> SettingRow {
        SettingRow {
            id: id.into(),
            label: label.into(),
            value: value.into(),
            kind,
            applies: Applies::Reload,
        }
    }

    #[test]
    fn a_boolean_flips_and_a_choice_cycles_and_wraps() {
        assert_eq!(
            SettingKind::Boolean.cycled("true"),
            Some("false".to_string())
        );
        let three = SettingKind::Choice(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(three.cycled("a"), Some("b".to_string()));
        assert_eq!(three.cycled("c"), Some("a".to_string()), "it wraps");
    }

    #[test]
    fn a_choice_that_does_not_offer_the_current_value_starts_at_the_first() {
        // A value set by hand, or an option this build no longer has. The key has
        // to do something: a dead confirm on a live row is indistinguishable
        // from a hang.
        let two = SettingKind::Choice(vec!["a".into(), "b".into()]);
        assert_eq!(two.cycled("nowhere"), Some("a".to_string()));
    }

    #[test]
    fn the_typed_kinds_have_no_cycle_to_offer() {
        assert_eq!(SettingKind::Text.cycled("anything"), None);
        assert_eq!(SettingKind::Integer { min: 0, max: 9 }.cycled("3"), None);
        assert!(SettingKind::Integer { min: 0, max: 9 }.needs_typing());
        assert!(SettingKind::Text.needs_typing());
        assert!(!SettingKind::Boolean.needs_typing());
    }

    #[test]
    fn an_unset_optional_is_a_third_state_and_not_a_synonym_for_false() {
        let kind = SettingKind::OptionalBoolean;
        assert_eq!(kind.cycled("auto"), Some("enabled".to_string()));
        assert_eq!(kind.cycled("enabled"), Some("disabled".to_string()));
        assert_eq!(
            kind.cycled("disabled"),
            Some("auto".to_string()),
            "and back to where it started, rather than off the end"
        );
    }

    #[test]
    fn a_choice_with_nothing_to_choose_is_a_refusal_not_a_panic() {
        // `%` by zero, caught at the one place it could happen.
        assert_eq!(SettingKind::Choice(Vec::new()).cycled("a"), None);
    }

    #[test]
    fn search_reads_the_three_things_the_row_shows() {
        let view = SettingsView::new(vec![
            row("ui.theme", "主题", "dark", SettingKind::Choice(vec![])),
            row(
                "coding.max_rounds",
                "单回合最大轮数",
                "50",
                SettingKind::Text,
            ),
        ]);
        assert_eq!(view.matching("theme").len(), 1, "by id");
        assert_eq!(view.matching("主题").len(), 1, "by label");
        assert_eq!(view.matching("50").len(), 1, "by value");
        assert_eq!(view.matching("THEME").len(), 1, "case does not matter");
        assert_eq!(view.matching("").len(), 2, "everything, with nothing typed");
        assert!(view.matching("nope").is_empty());
    }

    #[test]
    fn an_unset_value_is_a_value_and_stays_searchable() {
        // The empty string is what "unset" looks like on the row, and a row that
        // vanished from a search would be a row a person cannot reach.
        let view = SettingsView::new(vec![row(
            "init_prompt_file",
            "自定义",
            "",
            SettingKind::Text,
        )]);
        assert_eq!(view.matching("").len(), 1);
        assert_eq!(view.matching("init").len(), 1);
    }

    // ---- the keys ---------------------------------------------------------

    use crate::surface::{Key, KeyPress};

    fn view() -> SettingsView {
        SettingsView::new(vec![
            row("a.first", "第一", "true", SettingKind::Boolean),
            row(
                "b.second",
                "第二",
                "50",
                SettingKind::Integer { min: 0, max: 99 },
            ),
            row(
                "c.third",
                "第三",
                "dark",
                SettingKind::Choice(vec!["auto".into(), "dark".into()]),
            ),
        ])
    }

    #[test]
    fn arrows_walk_the_list_and_clamp_at_the_ends() {
        let view = view();
        let mut panel = Panel::new();
        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Down)),
            Step::Stay
        );
        assert_eq!(panel.cursor, 1);
        // Past the end it stays put rather than wrapping: a highlight that jumps
        // from the last row to the first reads as a slip.
        for _ in 0..5 {
            key(&view, &mut panel, KeyPress::plain(Key::Down));
        }
        assert_eq!(panel.cursor, 2, "clamped to the last row");
        for _ in 0..9 {
            key(&view, &mut panel, KeyPress::plain(Key::Up));
        }
        assert_eq!(panel.cursor, 0, "and to the first");
    }

    #[test]
    fn a_confirm_on_a_boolean_cycles_it_and_changes_nothing_else() {
        let view = view();
        let mut panel = Panel::new();
        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Enter)),
            Step::Set {
                id: "a.first".into(),
                value: "false".into()
            },
            "a boolean flips"
        );
        assert_eq!(panel.editing, None, "and no editor opens");
    }

    #[test]
    fn a_confirm_on_a_choice_cycles_and_on_a_number_opens_the_field() {
        let view = view();
        let mut panel = Panel::new();
        panel.cursor = 2;
        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Enter)),
            Step::Set {
                id: "c.third".into(),
                value: "auto".into()
            }
        );

        panel.cursor = 1;
        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Enter)),
            Step::Stay,
            "a number has no cycle"
        );
        assert_eq!(
            panel.editing,
            Some(Edit {
                id: "b.second".into(),
                value: "50".into()
            }),
            "so the field opens with the value that is there"
        );
    }

    #[test]
    fn while_a_field_is_open_nothing_else_takes_a_key() {
        // The failure this rules out: an arrow that walked the list under the
        // text being typed would change which setting the text is for, and the
        // person would not find out until they saved.
        let view = view();
        let mut panel = Panel::new();
        panel.cursor = 1;
        key(&view, &mut panel, KeyPress::plain(Key::Enter));
        assert!(panel.editing.is_some());

        for press in [
            KeyPress::plain(Key::Down),
            KeyPress::plain(Key::Up),
            KeyPress::ch('7'),
            KeyPress::plain(Key::Backspace),
        ] {
            key(&view, &mut panel, press);
        }
        assert_eq!(panel.cursor, 1, "the list did not move under the field");
        assert_eq!(
            panel.editing.as_ref().map(|e| e.value.as_str()),
            Some("50"),
            "and what is typed went into the field, not the search box"
        );
    }

    #[test]
    fn the_field_types_and_enter_sends_what_it_holds() {
        let view = view();
        let mut panel = Panel::new();
        panel.cursor = 1;
        key(&view, &mut panel, KeyPress::plain(Key::Enter));
        key(&view, &mut panel, KeyPress::plain(Key::Backspace));
        key(&view, &mut panel, KeyPress::plain(Key::Backspace));
        key(&view, &mut panel, KeyPress::ch('6'));
        key(&view, &mut panel, KeyPress::ch('0'));
        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Enter)),
            Step::Set {
                id: "b.second".into(),
                value: "60".into()
            }
        );
        assert_eq!(panel.editing, None, "and the field closes on the way out");
    }

    #[test]
    fn escape_abandons_the_field_without_changing_the_setting() {
        // A field that wrote half a number back on the way out would change a
        // setting nobody confirmed.
        let view = view();
        let mut panel = Panel::new();
        panel.cursor = 1;
        key(&view, &mut panel, KeyPress::plain(Key::Enter));
        key(&view, &mut panel, KeyPress::ch('9'));
        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Esc)),
            Step::Stay
        );
        assert_eq!(panel.editing, None, "the field is gone");
    }

    #[test]
    fn the_panel_opens_in_the_search_box_and_typing_filters_it() {
        // There is no key that opens the search box: the panel *is* in it, and
        // an ordinary character is a search character. The version this replaced
        // bound `/` to "start searching" and ate the slash doing it, so a person
        // searching for a path lost the character that says "path" and watched
        // the list go blank.
        let view = view();
        let mut panel = Panel::new();
        assert_eq!(panel.query, "", "nothing typed yet");
        assert_eq!(view.matching(&panel.query).len(), 3, "and everything shows");

        for c in "第一".chars() {
            assert_eq!(key(&view, &mut panel, KeyPress::ch(c)), Step::Stay);
        }
        assert_eq!(panel.query, "第一");
        assert_eq!(
            view.matching(&panel.query).len(),
            1,
            "and the list narrowed as it was typed, with no key pressed first"
        );
    }

    #[test]
    fn a_slash_is_a_slash_and_searching_for_a_path_is_possible() {
        // The regression this whole change is about, pinned: `/` is a character
        // like any other. It used to be the key that opened the box and was
        // swallowed doing it, so `/usr` searched for `usr`.
        let view = view();
        let mut panel = Panel::new();
        for c in "/usr/local".chars() {
            key(&view, &mut panel, KeyPress::ch(c));
        }
        assert_eq!(panel.query, "/usr/local", "every character arrived");
    }

    #[test]
    fn escape_clears_what_is_typed_before_it_closes_the_panel() {
        // A filter is recoverable: someone who has narrowed the list to nothing
        // wants the unfiltered list back, and closing would take the panel away
        // with the search.
        let view = view();
        let mut panel = Panel::new();
        for c in "zzz".chars() {
            key(&view, &mut panel, KeyPress::ch(c));
        }
        assert!(view.matching(&panel.query).is_empty());

        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Esc)),
            Step::Stay,
            "the first escape clears"
        );
        assert_eq!(panel.query, "", "and the list is whole again");
        assert_eq!(view.matching(&panel.query).len(), 3);

        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Esc)),
            Step::Close,
            "and now, with nothing left to clear, it closes"
        );
    }

    #[test]
    fn typing_in_the_search_box_goes_back_to_the_first_row() {
        // The list under the cursor just changed, so staying at the same index
        // would be pointing at a different setting.
        let view = view();
        let mut panel = Panel::new();
        panel.cursor = 2;
        key(&view, &mut panel, KeyPress::ch('第'));
        assert_eq!(panel.cursor, 0);
    }

    #[test]
    fn enter_still_takes_the_highlighted_row_while_the_search_box_has_the_keyboard() {
        // The thing that would break if the box "had focus" in the modal sense:
        // enter must go on confirming the row. A panel where typing filters and
        // enter does nothing is a panel nobody can change anything in.
        let view = view();
        let mut panel = Panel::new();
        for c in "第一".chars() {
            key(&view, &mut panel, KeyPress::ch(c));
        }
        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Enter)),
            Step::Set {
                id: "a.first".into(),
                value: "false".into()
            },
            "the filtered, highlighted row is the one enter takes"
        );
    }

    #[test]
    fn backspace_in_the_search_box_takes_a_character_not_a_byte() {
        let view = view();
        let mut panel = Panel::new();
        for c in "第一".chars() {
            key(&view, &mut panel, KeyPress::ch(c));
        }
        assert_eq!(panel.query, "第一");
        key(&view, &mut panel, KeyPress::plain(Key::Backspace));
        assert_eq!(panel.query, "第", "one character, not one byte");
        key(&view, &mut panel, KeyPress::plain(Key::Backspace));
        assert_eq!(panel.query, "");
        // And at the start it is a no-op rather than a panic — and not a close.
        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Backspace)),
            Step::Stay
        );
        assert_eq!(panel.query, "");
    }

    #[test]
    fn confirming_with_nothing_matching_does_nothing() {
        // A filtered-to-nothing list has no row to change, and a key that did
        // something anyway would be changing a setting nobody can see.
        let view = view();
        let mut panel = Panel::new();
        for c in "zzzz".chars() {
            key(&view, &mut panel, KeyPress::ch(c));
        }
        assert!(view.matching(&panel.query).is_empty());
        assert_eq!(
            key(&view, &mut panel, KeyPress::plain(Key::Enter)),
            Step::Stay
        );
    }

    #[test]
    fn the_search_box_and_a_rows_field_do_not_both_type() {
        // One keyboard, one destination: with a row's field open, characters go
        // there — and the search box keeps what it had.
        let view = view();
        let mut panel = Panel::new();
        key(&view, &mut panel, KeyPress::ch('第'));
        panel.cursor = 1;
        key(&view, &mut panel, KeyPress::plain(Key::Enter));
        assert!(panel.editing.is_some(), "the number's field is open");

        key(&view, &mut panel, KeyPress::ch('9'));
        assert_eq!(
            panel.editing.as_ref().map(|e| e.value.as_str()),
            Some("509"),
            "the 9 went into the field"
        );
        assert_eq!(panel.query, "第", "and the search box is untouched");
    }
}
