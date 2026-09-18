//! Foreign text: bytes from a file, a tool, the model, the clipboard.
//!
//! Text this crate composes is text it measured. Text from anywhere else is
//! bytes, and bytes are not what a terminal prints: `\r` returns the cursor to
//! column 1, `\n` moves it down a row — and scrolls the whole screen if it is
//! already on the last one — `\t` jumps to the next tab stop, and one `\x1b`
//! begins a sequence that can recolour the screen, move the cursor anywhere or
//! clear the display. Our width arithmetic counts every one of those as **zero
//! cells** ([`crate::width::char_width`]), because they are not printed. So
//! foreign text carrying one is a row that is not the row we composed.
//!
//! That is not cosmetic, and it is not only about one ugly line. The repaint
//! diff ([`crate::ansi::Rows::patch_from`]) skips a row whose bytes did not
//! change — the point of it — so a row the terminal drew differently from our
//! model is a row we will never repaint. The damage is permanent until
//! something forces a whole frame (ctrl-l, or a resize), which is exactly the
//! shape of the report that led here: the screen drifted, and dragging the
//! window fixed it.
//!
//! And foreign text is ordinary text: a CRLF file, a `\r` progress bar, a
//! colourised `git diff`, a Python file indented with tabs, a stack trace off
//! the clipboard. Which is why this is handled where the text enters rather
//! than hoped about at each place that draws it.
//!
//! Two ways in, so two policies over one scanner:
//!
//! * [`for_buffer`] — text going into the composer, where a newline is
//!   *content*: the composer breaks rows on it and Enter means send, so CRLF
//!   and a lone CR become LF and everything else goes.
//! * [`for_screen`] — text about to be written as a drawn row. The row has
//!   already been laid out, so a newline in it is not content but a corrupted
//!   frame: every control character goes, and a tab becomes the spaces it is
//!   drawn as.

use std::borrow::Cow;
use std::iter::Peekable;
use std::str::Chars;

/// A tab, as the cells a terminal would have advanced over.
///
/// The terminal's own stop is not ours to know — eight by default, four in
/// plenty of configurations, and nothing reports which — so a tab resolved by
/// the terminal is a row whose width we guessed. Resolving it here is what
/// makes the counted cells and the drawn cells the same number.
const TAB_SPACES: &str = "    ";

/// Foreign text on its way into the composer.
pub fn for_buffer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => eat_escape(&mut chars),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str(TAB_SPACES),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Foreign text about to be drawn as a row.
///
/// The last fence before the terminal, and the only one that can be: every
/// drawn span passes through here on its way to bytes, so this is where the
/// invariant *no byte we emit moves the cursor* can actually be held.
///
/// Borrowed when there is nothing to strip, which is the overwhelmingly common
/// case — every span of every row goes through here, and allocating a `String`
/// to tell it that `hello` is `hello` was the encoder's per-span cost. A row
/// with a control character in it is the exception, and pays for the copy.
pub fn for_screen(text: &str) -> Cow<'_, str> {
    if !needs_sanitising(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => eat_escape(&mut chars),
            '\t' => out.push_str(TAB_SPACES),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// Whether [`for_screen`] would change anything.
///
/// A byte test, not a `char` walk: UTF-8 continuation bytes are all `>= 0x80`,
/// so no multibyte character can hide a C0 control or DEL from it. C1 controls
/// (`U+0080..=U+009F`) *are* `char::is_control` and arrive as the pair `C2 80..9F`,
/// so they are matched as a pair rather than missed.
fn needs_sanitising(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().any(|b| *b < 0x20 || *b == 0x7f)
        || bytes
            .windows(2)
            .any(|pair| pair[0] == 0xc2 && (0x80..=0x9f).contains(&pair[1]))
}

/// A path under the home directory, written as `~/…` for a person to read.
///
/// **Bounded by path segments, not by string prefix.** With home `/home/me`,
/// `/home/melon` must come back untouched — a prefix match would rewrite it to
/// `~on`, and that is a bug that only shows up on somebody else's machine.
///
/// Here rather than in a module because the caller is `Tui::run`, which already
/// reads the environment; a module may not (the `os_probes` ratchet and
/// `docs/adr/0008` both say why). Doing the folding upstream is what lets the
/// module that draws this stay a pure function.
///
/// `collapse_home_with` is the same thing with the home directory passed in, so a
/// judgement about it does not depend on the machine running the tests.
pub fn collapse_home(path: &str) -> String {
    collapse_home_with(path, home_dir().as_deref())
}

/// The last segment of a path, for a window title.
///
/// Separators of both kinds, because a Windows path arrives with backslashes
/// and a title saying `C:\\work\\thing` when it could say `thing` is the same
/// waste on either OS. Falls back to the whole string when there is no
/// separator in it, which is what a bare directory name already is.
pub fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(at) => &trimmed[at + 1..],
        None => trimmed,
    }
}

/// The path being typed after an `@`, when one is.
///
/// The **last** `@` that opens a word, and only when the caret is still in that
/// word — a person writes `看一下 @src/ma` and means the thing at the end. An
/// `@` in the middle of a word is an email address or a decorator, not a path
/// somebody is reaching for, so it takes a boundary in front of it.
///
/// `Some("")` — a bare `@` at the end — is a real answer: it lists the working
/// directory, which is how a person finds out what is there.
pub fn being_pathed(typed: &str) -> Option<&str> {
    let at = typed.rfind('@')?;
    let opens = at == 0
        || typed[..at]
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace);
    if !opens {
        return None;
    }
    let rest = &typed[at + 1..];
    // Still one word: a space after it means the path was finished and
    // something else is being written now.
    (!rest.contains(char::is_whitespace)).then_some(rest)
}

/// The rest of the newest earlier line that starts with what is typed.
///
/// What a shell does (fish, zsh's autosuggest) and for the same reason: the
/// thing a person is most likely to be typing is a thing they typed before.
/// **Its source is this session's own history** — no model is asked, nothing is
/// invented. A suggestion with no source behind it is a sentence made up on the
/// screen, which is why this is the whole of it.
///
/// `None` while browsing the history (the field is already showing an entry),
/// for an empty field (everything would match), and for an exact repeat (there
/// is nothing left to accept).
pub fn ghost<'a>(typed: &str, history: &'a [String], browsing: bool) -> Option<&'a str> {
    if browsing || typed.is_empty() {
        return None;
    }
    history
        .iter()
        .rev()
        .find(|entry| entry.starts_with(typed) && entry.len() > typed.len())
        .map(|entry| &entry[typed.len()..])
}

/// A timestamp as "how long ago", for a list a person scans.
///
/// Milliseconds since the epoch, which is what the session store keeps. Coarse
/// on purpose: sorting a list of sessions needs "yesterday" and "just now", not
/// a clock reading — and a clock reading would also mean this screen and the
/// store agreeing about a timezone (`docs/adr/0008`).
pub fn when(at_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(at_ms);
    let ago = now.saturating_sub(at_ms) / 1000;
    match ago {
        0..=59 => "刚刚".into(),
        60..=3599 => format!("{} 分钟前", ago / 60),
        3600..=86_399 => format!("{} 小时前", ago / 3600),
        86_400..=2_591_999 => format!("{} 天前", ago / 86_400),
        _ => format!("{} 个月前", ago / 2_592_000),
    }
}

/// Seconds, as a person would say them.
///
/// Two parts at most and the smaller one dropped when it is zero: "4 分 12 秒",
/// "2 小时 5 分", "8 秒". A running total is glanced at, and a glance does not
/// parse "7452".
pub fn spoken_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m, s) {
        (0, 0, s) => format!("{s} 秒"),
        (0, m, 0) => format!("{m} 分"),
        (0, m, s) => format!("{m} 分 {s} 秒"),
        (h, 0, _) => format!("{h} 小时"),
        (h, m, _) => format!("{h} 小时 {m} 分"),
    }
}

/// What the window should be called: the session's name, or where it is
/// working when it has none.
///
/// An untitled session is the common case for the first minute, and four
/// windows all called `atomcode` tell nobody which of the four they are looking
/// at. A blank name counts as none — a title made of spaces is a title nobody
/// can read across a room.
pub fn window_name(title: Option<&str>, cwd: &str) -> String {
    match title {
        Some(title) if !title.trim().is_empty() => title.to_string(),
        _ => basename(cwd).to_string(),
    }
}

/// The implementation, with home explicit. See [`collapse_home`].
pub fn collapse_home_with(path: &str, home: Option<&std::path::Path>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    // A trailing separator would otherwise make every path fail the segment test
    // below, and collapsing would silently stop working for a person whose `HOME`
    // happens to end in one.
    let home = home.trim_end_matches(std::path::MAIN_SEPARATOR);
    if home.is_empty() {
        // `home` was just separators — the filesystem root, or malformed. Nothing
        // to collapse against: rewriting every absolute path to `~/…` would be a
        // shorter string that says less.
        return path.to_string();
    }
    let rest = if path == home {
        ""
    } else if let Some(rest) = path.strip_prefix(home) {
        // The segment boundary. `/home/melon` starts with `/home/me` but the next
        // character is not a separator, so it is a different directory.
        match rest.strip_prefix(std::path::MAIN_SEPARATOR) {
            Some(rest) => rest,
            None => return path.to_string(),
        }
    } else {
        return path.to_string();
    };
    if rest.is_empty() {
        "~".to_string()
    } else {
        format!("~{}{rest}", std::path::MAIN_SEPARATOR)
    }
}

/// The person's home directory, from the two variables that say so.
///
/// `HOME` on unix, `USERPROFILE` on Windows. Empty is treated as absent: a set
/// but blank variable is not an answer, and `~/proj` built from it would be
/// wrong in a way nobody could see.
fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// Consume one escape sequence, if the cursor is sitting on the `ESC` that
/// begins one.
///
/// The whole sequence, not just the `ESC`: dropping the `ESC` alone would leave
/// `[32m` sitting on the screen as text, which is both ugly and — for the
/// colourised tool output this exists for — the common case.
fn eat_escape(chars: &mut Peekable<Chars<'_>>) {
    match chars.next() {
        // CSI: parameters, then one final byte in @..~.
        Some('[') => {
            for c in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&c) {
                    break;
                }
            }
        }
        // OSC: runs to BEL or to ST (`ESC \`).
        Some(']') => {
            while let Some(c) = chars.next() {
                if c == '\x07' || (c == '\x1b' && chars.peek() == Some(&'\\')) {
                    if c == '\x1b' {
                        chars.next();
                    }
                    break;
                }
            }
        }
        // A two-character sequence (`ESC c`, `ESC 7`), or a stray `ESC` with
        // nothing after it. Either way the sequence is over.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    /// The ghost is the rest of something this session already said — never
    /// something invented.
    #[test]
    fn the_ghost_completes_from_this_sessions_own_history() {
        use super::ghost;
        let history: Vec<String> = ["git status", "cargo test", "cargo nextest run"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // The newest match wins: `cargo nextest run` was said after `cargo test`.
        assert_eq!(ghost("cargo ", &history, false), Some("nextest run"));
        assert_eq!(ghost("git ", &history, false), Some("status"));
        // Nothing left to accept.
        assert_eq!(ghost("git status", &history, false), None);
        // Nothing matches.
        assert_eq!(ghost("make", &history, false), None);
        // An empty field would match everything, which is not a suggestion.
        assert_eq!(ghost("", &history, false), None);
        // While arrowing through the history the field already shows an entry.
        assert_eq!(ghost("cargo ", &history, true), None);
    }

    /// A running total is glanced at, so it is said the way a person says it.
    #[test]
    fn a_duration_is_said_in_words_not_in_seconds() {
        use super::spoken_duration;
        assert_eq!(spoken_duration(8), "8 秒");
        assert_eq!(spoken_duration(252), "4 分 12 秒");
        // The smaller part goes when it is zero rather than reading "5 分 0 秒".
        assert_eq!(spoken_duration(300), "5 分");
        assert_eq!(spoken_duration(7500), "2 小时 5 分");
        assert_eq!(spoken_duration(7200), "2 小时");
        assert_eq!(spoken_duration(0), "0 秒");
    }

    /// `@` opens a path only where a path could start.
    ///
    /// The last one that opens a word, because a person writes the thing they
    /// mean at the end; an `@` inside a word is an email address or a
    /// decorator, not somebody reaching for a file.
    #[test]
    fn an_at_sign_opens_a_path_only_where_one_could_start() {
        use super::being_pathed;
        assert_eq!(being_pathed("@src/ma"), Some("src/ma"));
        assert_eq!(being_pathed("看一下 @src/ma"), Some("src/ma"));
        // A bare `@` lists where you are, which is how you find out.
        assert_eq!(being_pathed("@"), Some(""));
        // The last one wins.
        assert_eq!(being_pathed("@a/b 和 @c/d"), Some("c/d"));
        // Not an email, not a decorator.
        assert_eq!(being_pathed("写信给 li@example.com"), None);
        assert_eq!(being_pathed("#[serde(default)] x@y"), None);
        // Finished: a space after it means something else is being written.
        assert_eq!(being_pathed("@src/main.rs 改一下"), None);
        assert_eq!(being_pathed("没有 at 符号"), None);
    }

    /// The window says the session's name when it has one, and where it is
    /// working when it does not.
    #[test]
    fn a_window_is_named_after_the_session_or_after_where_it_is() {
        use super::window_name;
        assert_eq!(window_name(Some("修解析器"), "/w/atomcode"), "修解析器");
        assert_eq!(window_name(None, "/w/atomcode"), "atomcode");
        // A name of spaces is no name.
        assert_eq!(window_name(Some("   "), "/w/atomcode"), "atomcode");
    }

    /// A window title says which project, not the whole path to it.
    #[test]
    fn a_path_shows_up_as_its_last_segment() {
        use super::basename;
        assert_eq!(basename("/Users/me/work/atomcode"), "atomcode");
        assert_eq!(basename("/Users/me/work/atomcode/"), "atomcode");
        // Windows arrives with the other separator, and the waste is the same.
        assert_eq!(basename("C:\\work\\atomcode"), "atomcode");
        // A bare name is already what this returns.
        assert_eq!(basename("atomcode"), "atomcode");
        assert_eq!(basename(""), "");
    }

    use super::*;

    #[test]
    fn collapse_home_rewrites_the_prefix_and_nothing_else() {
        let home = std::path::Path::new("/home/me");
        assert_eq!(
            collapse_home_with("/home/me/proj/a", Some(home)),
            "~/proj/a"
        );
        // The segment boundary, which is the whole reason this is not a string
        // prefix check: `/home/melon` must not become `~on`.
        assert_eq!(
            collapse_home_with("/home/melon/a", Some(home)),
            "/home/melon/a"
        );
        // Not underneath home: untouched.
        assert_eq!(collapse_home_with("/tmp/a", Some(home)), "/tmp/a");
        // Home itself.
        assert_eq!(collapse_home_with("/home/me", Some(home)), "~");
        // A home we could not determine is not a home we guess at.
        assert_eq!(collapse_home_with("/home/me/a", None), "/home/me/a");
    }

    #[test]
    fn a_trailing_separator_on_home_does_not_turn_collapsing_off() {
        // `HOME=/home/me/` is something a shell can hand over, and with the naive
        // version every path failed the segment test — so the fold silently did
        // nothing and the welcome block printed the whole path.
        let home = std::path::Path::new("/home/me/");
        assert_eq!(collapse_home_with("/home/me/proj", Some(home)), "~/proj");
        assert_eq!(collapse_home_with("/home/me", Some(home)), "~");
    }

    #[test]
    fn a_home_of_only_separators_collapses_nothing() {
        // The degenerate case: home `/` (or a malformed value). Every absolute
        // path is "under" it, and returning `~/tmp/a` for the whole filesystem
        // would be a shorter string that says less. It is left alone instead.
        assert_eq!(
            collapse_home_with("/tmp/a", Some(std::path::Path::new("/"))),
            "/tmp/a"
        );
    }

    #[test]
    fn an_escape_sequence_in_a_paste_never_reaches_the_terminal() {
        // The hazard: spans are written verbatim and a control character is
        // counted as zero cells, so a pasted log could move the cursor, repaint
        // the screen, or leave the alternate screen.
        assert_eq!(for_buffer("\x1b[32mgreen\x1b[0m"), "green");
        assert_eq!(for_buffer("before\x1b[2Jafter"), "beforeafter");
        assert_eq!(for_buffer("\x1b]0;a title\x07x"), "x");
        assert_eq!(for_buffer("\x1b]11;rgb:00/00/00\x1b\\x"), "x");
        assert!(!for_buffer("\x1b[?1049lgone").contains('\x1b'));
    }

    #[test]
    fn newlines_survive_because_they_are_content() {
        // The composer breaks on them, and a paste that lost them would be a
        // paste that silently changed what the user is sending.
        assert_eq!(for_buffer("one\ntwo"), "one\ntwo");
        assert_eq!(
            for_buffer("crlf\r\nfile"),
            "crlf\nfile",
            "CRLF is one break"
        );
        assert_eq!(for_buffer("old\rmac"), "old\nmac");
    }

    #[test]
    fn a_tab_becomes_spaces_because_its_drawn_width_is_not_the_counted_one() {
        assert_eq!(for_buffer("a\tb"), "a    b");
    }

    #[test]
    fn ordinary_text_is_untouched_including_chinese_and_emoji() {
        for s in ["hello", "写一个网页", "🙂 ok", "path/to/file.rs:12"] {
            assert_eq!(for_buffer(s), s);
            assert_eq!(for_screen(s), s);
        }
    }

    #[test]
    fn clean_text_is_borrowed_so_a_span_that_needs_no_work_allocates_nothing() {
        // Every span of every drawn row passes through `for_screen`; handing
        // back a fresh `String` for text that has nothing to strip was a
        // per-span allocation in the encoder.
        assert!(matches!(for_screen("hello 世界"), Cow::Borrowed(_)));
        // And the copy really is only for text that needs one.
        assert!(matches!(for_screen("a\tb"), Cow::Owned(_)));
    }

    #[test]
    fn a_c1_control_character_is_stripped_like_any_other() {
        // `char::is_control` covers U+0080..=U+009F, which the byte test that
        // decides whether to borrow must not miss — they arrive as two bytes,
        // so a scan for `b < 0x20` alone would pass them through.
        assert_eq!(for_screen("a\u{9b}b"), "ab");
        assert_eq!(for_screen("a\u{80}b"), "ab");
        // The neighbouring non-controls are left alone.
        assert_eq!(for_screen("a\u{a0}b"), "a\u{a0}b", "NBSP is content");
    }

    #[test]
    fn a_drawn_row_loses_every_character_the_terminal_would_act_on() {
        // The screen-side policy: a line has already been laid out, so a
        // newline inside it is not a line break, it is a frame that scrolls.
        assert_eq!(for_screen("a\rb\nc"), "abc");
        assert_eq!(for_screen("a\tb"), "a    b");
        assert_eq!(for_screen("a\x07b"), "ab", "BEL is not content either");
        assert_eq!(
            for_screen("git diff\x1b[32m+ added\x1b[0m"),
            "git diff+ added"
        );
        // The same escapes as a paste, stripped the same way: one scanner, so
        // the two policies cannot drift apart on the hard part.
        assert_eq!(for_screen("\x1b]11;rgb:00/00/00\x1b\\done"), "done");
    }
}
