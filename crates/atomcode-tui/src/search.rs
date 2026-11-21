//! `Ctrl+R`: find something you said before by a word that was in it.
//!
//! Arrowing back through the history answers "what did I just say?". Once the
//! history reaches across sessions ([`crate::host::Host`] folds it from every
//! session log in this project) it holds hundreds of lines, and arrowing is no
//! longer a way through them — a person looking for the one command with
//! `--features session` in it would press Up forty times or give up and retype.
//! Searching is the way through, and `Ctrl+R` is the chord every shell has
//! taught for it.
//!
//! **The composer shows the query; the matches rise above it as a list.** What
//! is typed while a search is up goes to the query, and every entry it matches
//! is listed over the field, newest at the bottom against it — so a person sees
//! how many there are and picks one, instead of stepping through them one at a
//! time blind. The rules:
//!
//! 1. **Opening shows nothing.** An empty query matches no entry and fills
//!    nothing in. It used to show the newest entry at once, shell-style — and
//!    then `Ctrl+R`, Enter, Enter sent the last prompt again, which a person
//!    reaching for search did not mean. "The last thing" is what Up is for;
//! 2. a printable character extends the query, Backspace shortens it, and the
//!    list is matched again with the newest lit;
//! 3. Up — or `Ctrl+R` again, the shell's key for it — lights the next
//!    **older** match; Down the next newer;
//! 4. **Enter only accepts**: the lit entry goes into the composer and the
//!    search closes — a second Enter sends it. One keystroke between "I found
//!    it" and "it is gone to the model" is a keystroke too few;
//! 5. Esc gives back the draft that was set aside;
//! 6. **any other key accepts the lit entry and then does its ordinary job** —
//!    so `Home`, `Ctrl+U`, or typing on after it all keep working without anyone
//!    having to learn a way out.
//!
//! Matching is a case-insensitive substring, newest first, because that is what
//! a person means by "the one with `nextest` in it". The same line said twice is
//! listed once, where it was said last: two rows reading the same are one
//! choice, not two.

use crate::frame::{Line, Span};
use crate::moment::Moment;
use crate::surface::{Key, KeyPress, Mods};
use crate::theme::{self, Role};
use std::sync::Arc;

/// A reverse search, while it is up.
///
/// Lives on [`Moment`] rather than here as a static, for the reason everything
/// else on `Moment` does: it is screen state, and the screen is drawn from one
/// value so that two modules cannot hold two answers about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Search {
    /// What has been typed to search *with*. Shown in the composer while the
    /// search is up; the history itself is not touched until one is accepted.
    pub query: String,
    /// Every match, as indices into [`Moment::history`], **newest first**.
    /// Empty for an empty query, and for one that matches nothing.
    pub matches: Vec<usize>,
    /// Which of [`Self::matches`] is lit: 0 is the newest.
    pub sel: usize,
    /// The composer as it was when the search opened, given back by Esc.
    before: String,
    before_caret: usize,
    before_at: Option<usize>,
    before_pastes: Arc<Vec<String>>,
}

impl Search {
    /// The lit match, as an index into [`Moment::history`].
    ///
    /// `None` is "nothing to accept" — an empty query, or one that matches
    /// nothing — which is a state and not a failure.
    pub fn at(&self) -> Option<usize> {
        self.matches.get(self.sel).copied()
    }

    /// The history grew older entries at the **front**, so every index into it
    /// moved by that many.
    ///
    /// The project's older history is fetched lazily — the first press of Up,
    /// or the first `Ctrl+R` — and it lands while the search that asked for it
    /// is still up. Without this every match would go on pointing at whatever
    /// slid into its place: a wrong answer, not an error. The same shift
    /// [`crate::moment::Moment::history_at`] takes, for the same reason. What
    /// the older entries themselves match is found at the next keystroke.
    pub fn shift_by(&mut self, older: usize) {
        for at in self.matches.iter_mut() {
            *at += older;
        }
        if let Some(at) = self.before_at.as_mut() {
            *at += older;
        }
    }
}

/// What became of a key offered to the search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The search answered it; nothing else runs.
    Took,
    /// The search has closed and this key was never its own — run it the
    /// ordinary way, against the text the search left behind.
    Left,
}

/// Every entry whose text contains `query`, ignoring case, newest first, each
/// distinct text once (where it was said last). Nothing for an empty query.
///
/// `history` is oldest-first (the order it is folded in), so "newest first" is
/// a walk backwards.
pub fn matches(history: &[String], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let needle = query.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    history
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, entry)| entry.to_lowercase().contains(&needle))
        .filter(|(_, entry)| seen.insert(entry.as_str()))
        .map(|(i, _)| i)
        .collect()
}

/// Open a search, setting the draft aside. Nothing is matched and nothing is
/// filled in until something is typed — see rule 1 above.
pub fn begin(m: &mut Moment) {
    let mut s = Search {
        query: String::new(),
        matches: Vec::new(),
        sel: 0,
        before: m.input.clone(),
        before_caret: m.caret,
        before_at: m.history_at,
        before_pastes: m.pastes.clone(),
    };
    rematch(m, &mut s);
    m.search = Some(s);
}

/// One key, while a search is up.
pub fn key(m: &mut Moment, press: KeyPress) -> Step {
    let Some(mut s) = m.search.take() else {
        return Step::Left;
    };
    // Ctrl+R again, before the printable fallthrough: `r` with ctrl held is not
    // a character being typed into the query. It stays put at the oldest rather
    // than wrapping: a wrap would make the key that means "further back" walk
    // forwards, and a person holding it would never find the end.
    if press == KeyPress::ctrl('r') {
        s.sel = (s.sel + 1).min(s.matches.len().saturating_sub(1));
        m.search = Some(s);
        return Step::Took;
    }
    match (press.key, press.mods) {
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            s.query.push(c);
            rematch(m, &mut s);
            m.search = Some(s);
            Step::Took
        }
        (Key::Backspace, Mods::NONE) => {
            s.query.pop();
            rematch(m, &mut s);
            m.search = Some(s);
            Step::Took
        }
        // Up and down move through the list while there is one. With nothing
        // listed they are not the search's: the search closes and they browse
        // the history the ordinary way.
        (Key::Up, _) if !s.matches.is_empty() => {
            s.sel = (s.sel + 1).min(s.matches.len() - 1);
            m.search = Some(s);
            Step::Took
        }
        (Key::Down, _) if !s.matches.is_empty() => {
            s.sel = s.sel.saturating_sub(1);
            m.search = Some(s);
            Step::Took
        }
        // Accept, and only accept. What is in the composer stays there, as an
        // ordinary draft with the caret at its end.
        (Key::Enter, Mods::NONE) => {
            accept(m, &s);
            Step::Took
        }
        // Esc puts everything back, including where the caret was and which
        // history entry was being browsed: a search opened by accident must
        // cost nothing.
        (Key::Esc, _) => {
            m.input = s.before;
            m.pastes = s.before_pastes;
            m.history_at = s.before_at;
            m.caret = s.before_caret.min(m.input.len());
            Step::Took
        }
        // Anything else: the search is over — with the lit entry taken, as if
        // accepted — and the key still has its own job to do against it.
        _ => {
            accept(m, &s);
            Step::Left
        }
    }
}

/// The lit entry into the composer, caret at its end. With nothing lit, the
/// query stays there as it was typed — a person who typed `nextset` sees the
/// typo rather than an empty line.
fn accept(m: &mut Moment, s: &Search) {
    if let Some(i) = s.at() {
        m.input = m.history[i].clone();
        m.hint_history_clear();
    }
    m.caret = m.input.len();
}

/// Match again for the query as it now stands, newest lit, and show the query.
fn rematch(m: &mut Moment, s: &mut Search) {
    s.matches = matches(&m.history, &s.query);
    s.sel = 0;
    m.input = s.query.clone();
    // A recalled entry carries no folded pastes of its own — the log stores the
    // expanded text — so whatever the draft was holding does not belong to it.
    Arc::make_mut(&mut m.pastes).clear();
    m.recent_folded_paste = None;
    // The history position is the search's to say now, and it says it as
    // "search 'x' 2/5". Two counters on one shoulder would be two answers.
    m.history_at = None;
    m.caret = m.input.len();
}

/// The most matches listed at once; more scroll with the lit one.
pub const LIST_ROWS: usize = 8;

/// How many rows the list over the composer takes: none with nothing matched.
pub fn list_rows(s: &Search) -> usize {
    s.matches.len().min(LIST_ROWS)
}

/// The list's rows, top to bottom, `w` wide and `rows` tall: the matches in
/// view, **oldest at the top and newest at the bottom** against the composer,
/// so Up — "further back" — moves the light upward. The lit row is the panel
/// one step brighter, the same as every list in this UI.
///
/// A multi-line entry is shown on one line: the list is for recognising an
/// entry, and the whole of it lands in the composer when it is taken.
pub fn list_lines(history: &[String], s: &Search, w: usize, rows: usize) -> Vec<Line> {
    if w == 0 || rows == 0 || s.matches.is_empty() {
        return Vec::new();
    }
    let rows = rows.min(s.matches.len());
    // The window holds the lit match and moves by the least it can.
    let start = s.sel.saturating_sub(rows - 1).min(s.matches.len() - rows);
    let panel = theme::bg(Role::PanelBg).under(theme::fg(Role::PanelFg));
    let lit = theme::bg(Role::PanelSelBg).under(theme::fg(Role::PanelFg));
    (start..start + rows)
        .rev()
        .map(|k| {
            let base = if k == s.sel { lit } else { panel };
            let text = history
                .get(s.matches[k])
                .map(|entry| crate::text::one_line(entry))
                .unwrap_or_default();
            let shown = crate::width::take_width(&format!("  {text}"), w);
            let pad = w.saturating_sub(crate::width::str_width(&shown));
            Line::from_spans(vec![
                Span::styled(shown, base),
                Span::styled(" ".repeat(pad), base),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moment(history: &[&str]) -> Moment {
        Moment {
            history: Arc::new(history.iter().map(|s| (*s).to_string()).collect()),
            ..Default::default()
        }
    }

    const H: [&str; 5] = [
        "cargo build",
        "cargo nextest run -p atomcode-tui",
        "git status",
        "cargo fmt --all",
        "git log --oneline",
    ];

    fn typed(m: &mut Moment, text: &str) {
        for c in text.chars() {
            assert_eq!(key(m, KeyPress::plain(Key::Char(c))), Step::Took);
        }
    }

    #[test]
    fn every_entry_with_the_word_is_matched_newest_first() {
        assert_eq!(matches(&H.map(String::from), "cargo"), vec![3, 1, 0]);
        assert_eq!(matches(&H.map(String::from), "CARGO"), vec![3, 1, 0]);
        assert!(matches(&H.map(String::from), "kubernetes").is_empty());
        assert!(matches(&[], "anything").is_empty());
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert!(matches(&H.map(String::from), "").is_empty());
    }

    /// The same line said twice is one choice, listed where it was said last.
    #[test]
    fn a_line_said_twice_is_listed_once() {
        let h = ["fix it", "git status", "fix it"].map(String::from);
        assert_eq!(matches(&h, "fix"), vec![2]);
    }

    /// The reported risk: opening a search filled in the last prompt, so
    /// `Ctrl+R`, Enter, Enter sent it again. Now nothing is filled in, and
    /// accepting an empty search leaves an empty line.
    #[test]
    fn opening_a_search_fills_nothing_in() {
        let mut m = moment(&H);
        m.input = "half a thought".into();
        m.caret = m.input.len();
        begin(&mut m);
        assert_eq!(
            m.input, "",
            "the draft is set aside, and nothing replaces it"
        );
        assert!(m.search.as_ref().unwrap().matches.is_empty());
        assert_eq!(key(&mut m, KeyPress::plain(Key::Enter)), Step::Took);
        assert_eq!(m.input, "", "an Enter here has nothing to send");

        // And Esc gives the thought back.
        let mut m = moment(&H);
        m.input = "half a thought".into();
        begin(&mut m);
        key(&mut m, KeyPress::plain(Key::Esc));
        assert_eq!(m.input, "half a thought");
        assert_eq!(m.search, None);
    }

    #[test]
    fn typing_goes_to_the_query_and_lists_every_match() {
        let mut m = moment(&H);
        begin(&mut m);
        typed(&mut m, "cargo");
        let s = m.search.as_ref().unwrap();
        assert_eq!(s.query, "cargo");
        assert_eq!(m.input, "cargo", "the composer shows what is typed");
        assert_eq!(s.matches, vec![3, 1, 0], "all three, not one");
        assert_eq!(s.at(), Some(3), "the newest is lit");
    }

    #[test]
    fn backspace_shortens_the_query_and_matches_again() {
        let mut m = moment(&H);
        begin(&mut m);
        typed(&mut m, "fmt");
        assert_eq!(m.search.as_ref().unwrap().matches, vec![3]);
        for _ in 0..3 {
            key(&mut m, KeyPress::plain(Key::Backspace));
        }
        assert_eq!(m.search.as_ref().unwrap().query, "");
        assert!(m.search.as_ref().unwrap().matches.is_empty());
        assert_eq!(m.input, "");
    }

    #[test]
    fn up_and_ctrl_r_go_older_down_goes_newer_and_neither_wraps() {
        let mut m = moment(&H);
        begin(&mut m);
        typed(&mut m, "cargo");
        let at = |m: &Moment| m.search.as_ref().unwrap().at();
        key(&mut m, KeyPress::plain(Key::Up));
        assert_eq!(at(&m), Some(1));
        key(&mut m, KeyPress::ctrl('r'));
        assert_eq!(at(&m), Some(0));
        key(&mut m, KeyPress::ctrl('r'));
        assert_eq!(at(&m), Some(0), "stays at the oldest");
        key(&mut m, KeyPress::plain(Key::Down));
        key(&mut m, KeyPress::plain(Key::Down));
        key(&mut m, KeyPress::plain(Key::Down));
        assert_eq!(at(&m), Some(3), "and at the newest");
        assert_eq!(
            m.input, "cargo",
            "moving the light does not touch the composer"
        );
    }

    #[test]
    fn a_query_that_matches_nothing_shows_itself() {
        let mut m = moment(&H);
        begin(&mut m);
        typed(&mut m, "zzz");
        assert_eq!(m.search.as_ref().unwrap().at(), None);
        assert_eq!(m.input, "zzz", "the person must see what they typed");
        // With nothing listed, Up is the history's again.
        assert_eq!(key(&mut m, KeyPress::plain(Key::Up)), Step::Left);
        assert_eq!(m.search, None);
        assert_eq!(m.input, "zzz");
    }

    #[test]
    fn enter_only_accepts_and_a_second_enter_is_the_one_that_sends() {
        let mut m = moment(&H);
        begin(&mut m);
        typed(&mut m, "cargo");
        key(&mut m, KeyPress::plain(Key::Up));
        assert_eq!(key(&mut m, KeyPress::plain(Key::Enter)), Step::Took);
        assert_eq!(m.search, None, "the search is closed");
        assert_eq!(m.input, "cargo nextest run -p atomcode-tui", "the lit one");
        assert_eq!(m.caret, m.input.len());
        // The next Enter is nobody's but the composer's.
        assert_eq!(key(&mut m, KeyPress::plain(Key::Enter)), Step::Left);
    }

    #[test]
    fn any_other_key_takes_the_lit_entry_and_then_does_its_own_job() {
        let mut m = moment(&H);
        begin(&mut m);
        typed(&mut m, "status");
        assert_eq!(key(&mut m, KeyPress::plain(Key::Home)), Step::Left);
        assert_eq!(m.search, None);
        assert_eq!(m.input, "git status", "taken, not rolled back to the query");
    }

    #[test]
    fn older_history_landing_mid_search_moves_the_matches_with_it() {
        let mut m = moment(&H);
        begin(&mut m);
        typed(&mut m, "cargo");
        key(&mut m, KeyPress::plain(Key::Up));
        // The project's older history lands. Through the one method `plugin.rs`
        // calls, so this judges the merge rather than a re-enactment of it.
        assert!(m.history_grew_older(vec!["old one".into(), "old two".into()]));
        key(&mut m, KeyPress::plain(Key::Enter));
        assert_eq!(
            m.input, "cargo nextest run -p atomcode-tui",
            "the lit match is the entry it was, not whatever slid into its index"
        );
    }

    #[test]
    fn a_search_hides_the_history_position_because_it_says_its_own() {
        let mut m = moment(&H);
        m.history_at = Some(2);
        m.input = "git status".into();
        begin(&mut m);
        assert_eq!(m.history_at, None);
        // And Esc puts the browsing back where it was.
        key(&mut m, KeyPress::plain(Key::Esc));
        assert_eq!(m.history_at, Some(2));
    }

    /// The list: newest at the bottom against the composer, the lit row in the
    /// window, never wider than it is given.
    #[test]
    fn the_list_is_newest_at_the_bottom_and_keeps_the_lit_row_in_view() {
        let history: Vec<String> = (0..20).map(|i| format!("step {i}")).collect();
        let mut m = Moment {
            history: Arc::new(history.clone()),
            ..Default::default()
        };
        begin(&mut m);
        typed(&mut m, "step");
        let s = m.search.clone().unwrap();
        assert_eq!(list_rows(&s), LIST_ROWS, "twenty matches, eight rows");
        let text = |lines: &[Line]| -> Vec<String> {
            lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.text.as_str()).collect::<String>())
                .map(|t| t.trim().to_string())
                .collect()
        };
        let rows = text(&list_lines(&history, &s, 30, LIST_ROWS));
        assert_eq!(
            rows.last().unwrap(),
            "step 19",
            "newest against the composer"
        );
        assert_eq!(rows.first().unwrap(), "step 12");

        // Ten further back: the window follows the light.
        for _ in 0..10 {
            key(&mut m, KeyPress::plain(Key::Up));
        }
        let s = m.search.clone().unwrap();
        let rows = text(&list_lines(&history, &s, 30, LIST_ROWS));
        assert!(rows.contains(&"step 9".to_string()), "{rows:?}");
        for line in list_lines(&history, &s, 12, LIST_ROWS) {
            assert!(line.width() <= 12);
        }
    }
}
