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

use crate::i18n::{t, Msg};
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

/// 一段任意来源的文字,压成一行、去掉控制字符。
///
/// 给的是模型写的东西:换行会把编辑区下面那一行撑成好几行,而 ESC / BEL 之类
/// 能把自己的转义序列夹带到屏幕上。两件都在这里挡掉,所以下游画它的地方不必
/// 各自再想一遍。
pub fn one_line(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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
    // The other front end's session picker says these four words already, so
    // this reads its entry instead of opening a second "how long ago" table.
    use crate::i18n::product::{t, Msg};
    match ago {
        0..=59 => t(Msg::SessionTimeJustNow),
        60..=3599 => t(Msg::SessionTimeMinAgo { n: ago / 60 }),
        3600..=86_399 => t(Msg::SessionTimeHourAgo { n: ago / 3600 }),
        86_400..=2_591_999 => t(Msg::SessionTimeDayAgo { n: ago / 86_400 }),
        _ => t(Msg::SessionTimeMonthAgo { n: ago / 2_592_000 }),
    }
    .into_owned()
}

/// Seconds, as a person would say them.
///
/// Two parts at most and the smaller one dropped when it is zero: "4 分 12 秒",
/// "2 小时 5 分", "8 秒". A running total is glanced at, and a glance does not
/// parse "7452".
pub fn spoken_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m, s) {
        (0, 0, s) => t(Msg::LastedSeconds { s }),
        (0, m, 0) => t(Msg::LastedMinutes { m }),
        (0, m, s) => t(Msg::LastedMinutesSeconds { m, s }),
        (h, 0, _) => t(Msg::LastedHours { h }),
        (h, m, _) => t(Msg::LastedHoursMinutes { h, m }),
    }
    .into_owned()
}

/// The terminal window/tab title: an optional status dot, then the session's
/// name — or a fallback for a window not yet named.
///
/// The name is the newest `Titled` fact — the first-prompt guess, then the
/// model's own summary, then a `/rename`. A placeholder (empty, `default`, an
/// auto `session-…`, or a legacy `[…]`) is no name a person can read across a
/// room, so those fall back to `fallback` (the app + version). Real names are
/// scrubbed of control characters (an OSC title-injection embedded in an
/// auto-name must not survive), whitespace-collapsed, and truncated to 40 chars
/// with a trailing `…`. The dot rides in front — `🟢`/`🟡`/`🔴` for idle /
/// working / needs-you — the way the reference and Claude Code prefix theirs.
pub fn terminal_title(name: Option<&str>, fallback: &str, dot: Option<&str>) -> String {
    let title = terminal_title_name(name, fallback);
    match dot {
        Some(d) => format!("{d} {title}"),
        None => title,
    }
}

/// Max characters kept in the title's name before truncation. Tab strips are
/// narrow; the ellipsis counts toward the budget.
const MAX_TITLE_CHARS: usize = 40;

fn terminal_title_name(name: Option<&str>, fallback: &str) -> String {
    // A missing or blank title is the only placeholder here: unlike the reference
    // — which names *sessions* and screens out synthetic ids like `session-…` and
    // `[image]` — this names from `Titled`, which is either absent (no title yet)
    // or a real title. Screening `[` / `session-` here would only hide a genuine
    // title like `[WIP] fix login`.
    let raw = name.unwrap_or("").trim();
    if raw.is_empty() {
        return fallback.to_string();
    }
    // Drop control characters (ESC, BEL, …) so a name derived from arbitrary
    // user text cannot smuggle its own OSC title sequence; a control char that
    // was a separator becomes a space, and the whitespace then collapses.
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        return fallback.to_string();
    }
    if cleaned.chars().count() > MAX_TITLE_CHARS {
        let kept: String = cleaned.chars().take(MAX_TITLE_CHARS - 1).collect();
        return format!("{kept}…");
    }
    cleaned
}

/// What the session is doing, as one character in front of the window's name.
///
/// The screen already says this — the status line, the live line, the question
/// on it — but all of that is behind the window you are not looking at. Four
/// tabs open on four sessions and "which one wants me" is the question the
/// tab strip cannot currently answer. A coloured dot answers it without a
/// pixel of the screen being spent: the terminal has somewhere to put a
/// title, and this is the smallest true thing to put in it.
///
/// Three states, not four: "stopping" reads as busy, because the turn it is
/// stopping is still the turn in flight, and a person who has just pressed
/// escape does not need the tab to argue with them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Light {
    /// Nothing running and nothing asked of the person.
    Idle,
    /// A turn is in flight — the model talking, a tool running, a stop landing.
    Busy,
    /// Something is waiting on the person: a question, an approval, a password.
    ///
    /// This is the one the light exists for: it is the only state where the
    /// session cannot get on without somebody, and the only one worth looking
    /// across a room for.
    Waiting,
}

impl Light {
    /// The dot itself.
    pub fn dot(self) -> &'static str {
        match self {
            Self::Idle => "🟢",
            Self::Busy => "🟡",
            Self::Waiting => "🔴",
        }
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

/// Every path inside a *command line* folded to `~`, for the row that shows a
/// shell call.
///
/// A command is not a path, so [`collapse_home`] cannot be pointed at it: the
/// home directory turns up in the middle, several times, next to quotes and
/// `=`. The rule is the shell's own — a `~` only expands at the start of a
/// word — so only a run that begins at a word boundary and ends at one is
/// folded.
///
/// `:` and `,` are deliberately **not** boundaries. A path inside a
/// `PATH`-style colon list, or the remote half of `host:/path`, is not
/// shell-expanded there, so writing `~` would be a line that no longer means
/// what it says.
///
/// Cosmetic only: what ran, and what a copy of the transcript carries, keep the
/// real path.
pub fn collapse_home_in_command(command: &str) -> String {
    collapse_home_in_command_with(command, home_dir().as_deref())
}

/// The implementation, with home explicit. See [`collapse_home_in_command`].
pub fn collapse_home_in_command_with(command: &str, home: Option<&std::path::Path>) -> String {
    let Some(home) = home else {
        return command.to_string();
    };
    let home = home.to_string_lossy();
    let home = home.trim_end_matches(std::path::MAIN_SEPARATOR);
    if home.is_empty() || !command.contains(home) {
        return command.to_string();
    }
    let boundary = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '=' | '(');
    let mut out = String::with_capacity(command.len());
    let mut i = 0;
    while i < command.len() {
        let rest = &command[i..];
        if let Some(after) = rest.strip_prefix(home) {
            let opens = i == 0 || command[..i].chars().next_back().is_none_or(boundary);
            let closes = after.is_empty()
                || after.starts_with(std::path::MAIN_SEPARATOR)
                || after.chars().next().is_none_or(boundary);
            if opens && closes {
                out.push('~');
                i += home.len();
                continue;
            }
        }
        let ch = rest.chars().next().expect("not at the end");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// `~` and `~/…` written back out as the home directory — the inverse of
/// [`collapse_home`], for a path a person **typed**.
///
/// Here because a person types what they read, and what they read is `~/…`:
/// this screen prints every path that way. Without this, `/view ~/notes.md`
/// is not absolute, so it gets joined onto the working directory and the
/// answer is "cannot read it" — a refusal that blames the file for a path
/// nobody ever meant.
///
/// **Only a leading `~` as its own segment.** `~foo` is another person's home
/// in shell syntax and this does not resolve those, so it is left alone rather
/// than guessed at; `a/~/b` is a real (if odd) relative path and is not ours to
/// rewrite. With no home directory to expand against, the path comes back as it
/// was — the caller's "cannot read it" is then the honest answer.
pub fn expand_home_with(path: &str, home: Option<&std::path::Path>) -> String {
    let rest = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/").or_else(|| {
            // Windows types `~\…`. Checked separately so a unix path containing
            // a backslash is not mistaken for one.
            (std::path::MAIN_SEPARATOR != '/')
                .then(|| path.strip_prefix(&format!("~{}", std::path::MAIN_SEPARATOR)))
                .flatten()
        })
    };
    let (Some(rest), Some(home)) = (rest, home) else {
        return path.to_string();
    };
    if rest.is_empty() {
        return home.to_string_lossy().into_owned();
    }
    home.join(rest).to_string_lossy().into_owned()
}

/// The person's home directory, from the two variables that say so.
///
/// `HOME` on unix, `USERPROFILE` on Windows. Empty is treated as absent: a set
/// but blank variable is not an answer, and `~/proj` built from it would be
/// wrong in a way nobody could see.
pub(crate) fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

// ── one line of text with a caret in it ──────────────────────────────────────
//
// Every single-line field on this screen edits a `String` and a byte offset
// into it, and they must all agree on what the arrow keys do. These five were
// written for the provider forms (`crate::providers`) and lived there until
// the settings panel needed the same five — at which point "the same five"
// had to stop meaning "typed out twice".
//
// Byte offsets, not character indices, because the callers slice with them;
// [`snap`] is what keeps that safe.

/// `at`, snapped back to a character boundary.
///
/// Every path that moves a caret keeps it on one; this is the belt to that
/// braces, on the functions that slice — a byte offset from the middle of a
/// multi-byte character would panic.
pub(crate) fn snap(text: &str, at: usize) -> usize {
    let at = at.min(text.len());
    (0..=at)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0)
}

/// Put `c` in at the caret, and step the caret over it.
pub(crate) fn insert_at(text: &mut String, caret: &mut usize, c: char) {
    let at = snap(text, *caret);
    text.insert(at, c);
    *caret = at + c.len_utf8();
}

/// Take out the character before the caret.
pub(crate) fn backspace_at(text: &mut String, caret: &mut usize) {
    let at = snap(text, *caret);
    let Some(previous) = text[..at].chars().next_back() else {
        return;
    };
    let from = at - previous.len_utf8();
    text.remove(from);
    *caret = from;
}

/// Take out the character the caret is on.
pub(crate) fn delete_at(text: &mut String, caret: &mut usize) {
    let at = snap(text, *caret);
    if at < text.len() {
        text.remove(at);
    }
    *caret = at;
}

/// The caret, one character further along — or where it was, at either end.
pub(crate) fn step_caret(text: &str, caret: usize, forward: bool) -> usize {
    let at = snap(text, caret);
    match forward {
        true => text[at..]
            .chars()
            .next()
            .map(|c| at + c.len_utf8())
            .unwrap_or(at),
        false => text[..at]
            .chars()
            .next_back()
            .map(|c| at - c.len_utf8())
            .unwrap_or(0),
    }
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

    /// The title is the session name behind a status dot, falling back to the
    /// app + version for a window not yet named, and scrubbed/truncated so an
    /// auto-name cannot smuggle an escape sequence or overflow the tab.
    #[test]
    fn the_terminal_title_is_a_dot_then_the_session_name() {
        use super::terminal_title;
        const FB: &str = "AtomCode v9.9.9";
        // A real name rides behind the dot.
        assert_eq!(
            terminal_title(Some("修解析器"), FB, Some("🟢")),
            "🟢 修解析器"
        );
        // Only a missing / blank title falls back to the app.
        assert_eq!(terminal_title(None, FB, Some("🟡")), "🟡 AtomCode v9.9.9");
        assert_eq!(
            terminal_title(Some("   "), FB, Some("🔴")),
            "🔴 AtomCode v9.9.9"
        );
        // A real title that happens to start with `[` is shown, not hidden.
        assert_eq!(
            terminal_title(Some("[WIP] fix login"), FB, None),
            "[WIP] fix login"
        );
        // A control-char / OSC injection in the name does not survive.
        assert_eq!(
            terminal_title(Some("hi\x1b]2;pwned\x07there"), FB, None),
            "hi ]2;pwned there"
        );
        // Over-long names are cut with an ellipsis, dot excluded from the budget.
        let long = "a".repeat(50);
        let title = terminal_title(Some(&long), FB, Some("🟢"));
        assert!(title.starts_with("🟢 "));
        assert!(title.ends_with('…'));
    }

    /// Each state has its own dot, and the three are distinct — a light that
    /// could not be told apart from another is a light that says nothing.
    #[test]
    fn each_state_has_a_dot_of_its_own() {
        use super::Light;
        assert_eq!(Light::Idle.dot(), "🟢");
        assert_eq!(Light::Busy.dot(), "🟡");
        assert_eq!(Light::Waiting.dot(), "🔴");
        assert_ne!(Light::Idle.dot(), Light::Busy.dot());
        assert_ne!(Light::Busy.dot(), Light::Waiting.dot());
    }

    /// The dot goes in front of the name, with a space, and the name underneath
    /// is untouched — so the truncation and scrubbing budget of the name do not
    /// change when a light is added.
    #[test]
    fn the_dot_is_prefixed_and_the_name_is_left_alone() {
        use super::{terminal_title, Light};
        assert_eq!(
            terminal_title(Some("修解析器"), "atomcode", Some(Light::Waiting.dot())),
            format!("🔴 {}", terminal_title(Some("修解析器"), "atomcode", None)),
        );
        // The fallback carries a light too: a fresh session is exactly the one
        // somebody may be waiting on.
        assert_eq!(
            terminal_title(None, "atomcode", Some(Light::Idle.dot())),
            "🟢 atomcode"
        );
    }

    /// No light is no prefix at all — the title is byte-for-byte what it was
    /// before this feature, which is what turning the setting off has to mean.
    #[test]
    fn no_light_is_the_plain_title_unchanged() {
        use super::terminal_title;
        assert_eq!(
            terminal_title(Some("修解析器"), "atomcode", None),
            "修解析器"
        );
        assert_eq!(terminal_title(None, "atomcode", None), "atomcode");
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

    /// A command line folds every path in it, and only where a `~` would have
    /// meant the same thing.
    ///
    /// A shell row is one line wide, and on a real machine the absolute home
    /// path eats most of it before the command has said anything. But a `~` is
    /// only the home directory **at the start of a word** — written anywhere
    /// else it is a literal tilde, so folding there would print a line that no
    /// longer means what it says. `:` and `,` are the two that look like
    /// boundaries and are not: neither a `PATH` list nor `host:/path` expands
    /// a `~` after them.
    #[test]
    fn a_command_folds_the_home_paths_a_shell_would_have_expanded() {
        let home = std::path::Path::new("/home/me");
        let fold = |cmd: &str| collapse_home_in_command_with(cmd, Some(home));
        assert_eq!(fold("ls /home/me/proj"), "ls ~/proj");
        assert_eq!(fold("cd /home/me"), "cd ~");
        // Several, and inside quotes, which is a word boundary.
        assert_eq!(fold("diff \"/home/me/a\" /home/me/b"), "diff \"~/a\" ~/b");
        assert_eq!(fold("X=/home/me/bin cmd"), "X=~/bin cmd");
        // Not a boundary: the shell would not have expanded these either.
        assert_eq!(
            fold("PATH=/usr/bin:/home/me/bin"),
            "PATH=/usr/bin:/home/me/bin"
        );
        assert_eq!(fold("scp host:/home/me/a ."), "scp host:/home/me/a .");
        // A longer directory that merely starts the same is a different one.
        assert_eq!(fold("ls /home/melon"), "ls /home/melon");
        // Nothing to fold against leaves the line exactly as it is.
        assert_eq!(
            collapse_home_in_command_with("ls /home/me", None),
            "ls /home/me"
        );
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

    /// A person types the path back the way this screen printed it, so `~/…`
    /// has to mean what it looks like. Unexpanded it is not absolute, gets
    /// joined onto the working directory, and the answer is "cannot read it".
    #[test]
    fn a_typed_tilde_becomes_the_home_directory() {
        let home = std::path::Path::new("/home/me");
        assert_eq!(
            expand_home_with("~/notes.md", Some(home)),
            "/home/me/notes.md"
        );
        assert_eq!(expand_home_with("~", Some(home)), "/home/me");

        // `~foo` is another person's home in shell syntax and this does not
        // resolve those — guessing would open the wrong file silently.
        assert_eq!(expand_home_with("~other/a", Some(home)), "~other/a");
        // Not a leading segment: a real, if odd, relative path.
        assert_eq!(expand_home_with("a/~/b", Some(home)), "a/~/b");
        // Nothing to expand against: unchanged, so the caller's "cannot read
        // it" stays the honest answer rather than becoming a wrong path.
        assert_eq!(expand_home_with("~/notes.md", None), "~/notes.md");
        // Untouched paths pass through whatever the home is.
        assert_eq!(expand_home_with("/tmp/a", Some(home)), "/tmp/a");
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
