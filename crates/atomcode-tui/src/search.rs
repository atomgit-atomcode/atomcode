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
//! **What is typed while a search is up does not go in the composer.** That is
//! the whole difference between this and typing: the composer shows the *hit*,
//! and the query lives here, on the rule above the field. Which is also why a
//! search is a mode rather than a filter — and why the rules below are the
//! readline ones rather than invented:
//!
//! 1. a printable character extends the query and re-searches from the newest;
//! 2. Backspace shortens it, same;
//! 3. `Ctrl+R` again steps to the next **older** hit;
//! 4. **Enter only accepts** the hit into the composer and closes the search —
//!    a second Enter sends it. One keystroke between "I found it" and "it is
//!    gone to the model" is a keystroke too few;
//! 5. Esc gives back the draft that was set aside;
//! 6. **any other key closes the search and then does its ordinary job** — so
//!    arrowing, `Ctrl+U`, or typing on after an accept all keep working without
//!    anyone having to learn a way out.
//!
//! Matching is a case-insensitive substring, newest first, because that is what
//! a person means by "the one with `nextest` in it". An empty query matches
//! everything and therefore shows the newest entry.

use crate::moment::Moment;
use crate::surface::{Key, KeyPress, Mods};

/// A reverse search, while it is up.
///
/// Lives on [`Moment`] rather than here as a static, for the reason everything
/// else on `Moment` does: it is screen state, and the screen is drawn from one
/// value so that two modules cannot hold two answers about it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Search {
    /// What has been typed to search *with*. Never the composer's text.
    pub query: String,
    /// The hit, as an index into [`Moment::history`].
    ///
    /// `None` is "nothing matches", which is a state and not a failure: the
    /// composer then shows the query itself, so a person who typed `nextset`
    /// can see the typo instead of an empty line.
    pub at: Option<usize>,
    /// The composer as it was when the search opened, given back by Esc.
    before: String,
    before_caret: usize,
    before_at: Option<usize>,
    before_pastes: Vec<String>,
}

impl Search {
    /// The history grew older entries at the **front**, so every index into it
    /// moved by that many.
    ///
    /// The project's older history is fetched lazily — the first press of Up,
    /// or the first `Ctrl+R` — and it lands while the search that asked for it
    /// is still up. Without this the hit index would go on pointing at whatever
    /// slid into its place, and the next `Ctrl+R` would step from the wrong
    /// entry: a wrong answer, not an error. The same shift
    /// [`crate::moment::Moment::history_at`] takes, for the same reason.
    pub fn shift_by(&mut self, older: usize) {
        if let Some(at) = self.at.as_mut() {
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

/// The newest entry strictly older than `older_than` whose text contains
/// `query`, ignoring case. `None` for `older_than` starts from the newest.
///
/// `history` is oldest-first (the order it is folded in), so "newest first" is
/// a walk backwards — and "older than the hit I am on" is a walk backwards from
/// that index, which is what `Ctrl+R` pressed twice means.
pub fn hit(history: &[String], query: &str, older_than: Option<usize>) -> Option<usize> {
    let upper = match older_than {
        None => history.len(),
        // Already on the oldest: there is nothing further back to offer.
        Some(0) => return None,
        Some(i) => i.min(history.len()),
    };
    let needle = query.to_lowercase();
    history[..upper]
        .iter()
        .rposition(|entry| entry.to_lowercase().contains(&needle))
}

/// Open a search, setting the draft aside.
///
/// The newest entry is shown at once (an empty query matches it), the same as a
/// shell: a search that showed nothing until a character was typed would make
/// the most common case — "the last thing, but I want to edit it" — two
/// keystrokes instead of one.
pub fn begin(m: &mut Moment) {
    let mut s = Search {
        query: String::new(),
        at: None,
        before: m.input.clone(),
        before_caret: m.caret,
        before_at: m.history_at,
        before_pastes: m.pastes.clone(),
    };
    show(m, &mut s, None);
    m.search = Some(s);
}

/// One key, while a search is up.
pub fn key(m: &mut Moment, press: KeyPress) -> Step {
    let Some(mut s) = m.search.take() else {
        return Step::Left;
    };
    // Ctrl+R again, before the printable fallthrough: `r` with ctrl held is not
    // a character being typed into the query.
    if press == KeyPress::ctrl('r') {
        // Staying put when there is no older hit, rather than falling back to
        // the newest: a wrap would make the key that means "further back" walk
        // forwards, and a person holding it would never find the end.
        let older = hit(&m.history, &s.query, s.at);
        if older.is_some() {
            show_at(m, &mut s, older);
        }
        m.search = Some(s);
        return Step::Took;
    }
    match (press.key, press.mods) {
        (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
            s.query.push(c);
            show(m, &mut s, None);
            m.search = Some(s);
            Step::Took
        }
        (Key::Backspace, Mods::NONE) => {
            s.query.pop();
            show(m, &mut s, None);
            m.search = Some(s);
            Step::Took
        }
        // Accept, and only accept. What is in the composer stays there, as an
        // ordinary draft with the caret at its end.
        (Key::Enter, Mods::NONE) => {
            m.caret = m.input.len();
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
        // Anything else: the search is over, and the key still has its own job
        // to do against what the search left in the composer.
        _ => Step::Left,
    }
}

/// Re-search from the newest and show what was found.
fn show(m: &mut Moment, s: &mut Search, older_than: Option<usize>) {
    let found = hit(&m.history, &s.query, older_than);
    show_at(m, s, found);
}

fn show_at(m: &mut Moment, s: &mut Search, found: Option<usize>) {
    s.at = found;
    m.input = match found {
        Some(i) => m.history[i].clone(),
        // No hit: show what was typed, so the person can see their own typo.
        None => s.query.clone(),
    };
    // A recalled entry carries no folded pastes of its own — the log stores the
    // expanded text — so whatever the draft was holding does not belong to it.
    m.pastes.clear();
    m.recent_folded_paste = None;
    // The history position is the search's to say now, and it says it as
    // "search 'x' 2/5". Two counters on one shoulder would be two answers.
    m.history_at = None;
    m.caret = m.input.len();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moment(history: &[&str]) -> Moment {
        Moment {
            history: history.iter().map(|s| (*s).to_string()).collect(),
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

    #[test]
    fn a_search_finds_the_newest_entry_containing_the_word() {
        assert_eq!(hit(&H.map(String::from), "cargo", None), Some(3));
    }

    #[test]
    fn a_search_ignores_case() {
        assert_eq!(hit(&H.map(String::from), "CARGO", None), Some(3));
        assert_eq!(hit(&["Cargo Build".to_string()], "cargo b", None), Some(0));
    }

    #[test]
    fn an_empty_query_is_the_newest_entry() {
        assert_eq!(hit(&H.map(String::from), "", None), Some(4));
    }

    #[test]
    fn pressing_again_steps_to_the_next_older_hit() {
        let h = H.map(String::from);
        assert_eq!(hit(&h, "cargo", None), Some(3));
        assert_eq!(hit(&h, "cargo", Some(3)), Some(1));
        assert_eq!(hit(&h, "cargo", Some(1)), Some(0));
        assert_eq!(hit(&h, "cargo", Some(0)), None);
    }

    #[test]
    fn nothing_matches_is_an_answer_not_a_panic() {
        assert_eq!(hit(&H.map(String::from), "kubernetes", None), None);
        assert_eq!(hit(&[], "anything", None), None);
    }

    #[test]
    fn opening_a_search_shows_the_newest_and_sets_the_draft_aside() {
        let mut m = moment(&H);
        m.input = "half a thought".into();
        m.caret = m.input.len();
        begin(&mut m);
        assert_eq!(m.input, "git log --oneline");
        assert_eq!(m.search.as_ref().unwrap().at, Some(4));
        // And Esc gives the thought back.
        key(&mut m, KeyPress::plain(Key::Esc));
        assert_eq!(m.input, "half a thought");
        assert_eq!(m.search, None);
    }

    #[test]
    fn typing_goes_to_the_query_and_not_to_the_composer() {
        let mut m = moment(&H);
        begin(&mut m);
        for c in "fmt".chars() {
            assert_eq!(key(&mut m, KeyPress::plain(Key::Char(c))), Step::Took);
        }
        assert_eq!(m.search.as_ref().unwrap().query, "fmt");
        // The composer shows the hit, never the query.
        assert_eq!(m.input, "cargo fmt --all");
    }

    #[test]
    fn backspace_shortens_the_query_and_researches() {
        let mut m = moment(&H);
        begin(&mut m);
        for c in "fmt".chars() {
            key(&mut m, KeyPress::plain(Key::Char(c)));
        }
        assert_eq!(m.input, "cargo fmt --all");
        key(&mut m, KeyPress::plain(Key::Backspace));
        key(&mut m, KeyPress::plain(Key::Backspace));
        key(&mut m, KeyPress::plain(Key::Backspace));
        assert_eq!(m.search.as_ref().unwrap().query, "");
        assert_eq!(m.input, "git log --oneline", "an empty query is the newest");
    }

    #[test]
    fn a_query_that_matches_nothing_shows_itself() {
        let mut m = moment(&H);
        begin(&mut m);
        for c in "zzz".chars() {
            key(&mut m, KeyPress::plain(Key::Char(c)));
        }
        assert_eq!(m.search.as_ref().unwrap().at, None);
        assert_eq!(m.input, "zzz", "the person must see what they typed");
    }

    #[test]
    fn ctrl_r_again_walks_back_and_stops_at_the_oldest() {
        let mut m = moment(&H);
        begin(&mut m);
        for c in "cargo".chars() {
            key(&mut m, KeyPress::plain(Key::Char(c)));
        }
        assert_eq!(m.input, "cargo fmt --all");
        key(&mut m, KeyPress::ctrl('r'));
        assert_eq!(m.input, "cargo nextest run -p atomcode-tui");
        key(&mut m, KeyPress::ctrl('r'));
        assert_eq!(m.input, "cargo build");
        // And there it stays: no wrap to the newest.
        key(&mut m, KeyPress::ctrl('r'));
        assert_eq!(m.input, "cargo build");
        assert!(m.search.is_some(), "still searching");
    }

    #[test]
    fn enter_only_accepts_and_a_second_enter_is_the_one_that_sends() {
        let mut m = moment(&H);
        begin(&mut m);
        for c in "status".chars() {
            key(&mut m, KeyPress::plain(Key::Char(c)));
        }
        assert_eq!(key(&mut m, KeyPress::plain(Key::Enter)), Step::Took);
        assert_eq!(m.search, None, "the search is closed");
        assert_eq!(m.input, "git status", "and the hit is the draft now");
        assert_eq!(m.caret, m.input.len());
        // The next Enter is nobody's but the composer's.
        assert_eq!(key(&mut m, KeyPress::plain(Key::Enter)), Step::Left);
    }

    #[test]
    fn any_other_key_closes_the_search_and_then_does_its_own_job() {
        let mut m = moment(&H);
        begin(&mut m);
        for c in "status".chars() {
            key(&mut m, KeyPress::plain(Key::Char(c)));
        }
        // Home is not the search's; it closes the search and is then applied by
        // the caller to the text the search left behind.
        assert_eq!(key(&mut m, KeyPress::plain(Key::Home)), Step::Left);
        assert_eq!(m.search, None);
        assert_eq!(m.input, "git status", "the hit is kept, not rolled back");
    }

    #[test]
    fn older_history_landing_mid_search_moves_the_hit_with_it() {
        let mut m = moment(&H);
        begin(&mut m);
        for c in "cargo".chars() {
            key(&mut m, KeyPress::plain(Key::Char(c)));
        }
        assert_eq!(m.search.as_ref().unwrap().at, Some(3), "`cargo fmt --all`");
        // The project's older history lands. Through the one method `plugin.rs`
        // calls, so this judges the merge rather than a re-enactment of it.
        assert!(m.history_grew_older(vec!["old one".into(), "old two".into()]));
        // Ctrl+R steps to the hit before the one on screen. Without the shift
        // the index still says 3 — which is now `cargo nextest`'s neighbour —
        // and the step skips a match instead of landing on it.
        key(&mut m, KeyPress::ctrl('r'));
        assert_eq!(
            m.input, "cargo nextest run -p atomcode-tui",
            "stepping back must start from the hit, not from whatever slid \
             into its old index"
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
}
