//! What changed in each release, as the person who upgraded reads it.
//!
//! **Where the words come from.** `CHANGELOG.md` (Chinese) and `CHANGELOG.en.md`
//! (English) at the repository root, written before a release and compiled into
//! the binary: a build carries the notes for itself and every release before it,
//! so they read offline, and a build can never show the notes of a release it is
//! not. Each release is one section that starts `## vX.Y.Z` (a date may follow);
//! anything above the first such heading is for whoever edits the file and is
//! never shown. The language in force picks the file ([`shipped`]); a release the
//! English file has no section for is shown from the Chinese one rather than not
//! at all.
//!
//! **Telling once.** An upgrade is news on the first launch after it, and on no
//! launch after that — the people this is for update often, and a notice that
//! comes back every launch is the thing they asked not to get. What has been
//! told is one small file in the config directory ([`seen_path`]): the newest
//! release a person was told about, and the one before it, so `/changelog` can
//! still mark what is new to them after the notice is spent.
//!
//! This module only decides. Recording a notice as told is the launcher's,
//! when the screen has actually drawn it ([`record_seen`]).

use std::fmt;
use std::path::{Path, PathBuf};

/// The notes as this build ships them, in Chinese.
pub const DOCUMENT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../CHANGELOG.md"));

/// The same notes in English. A release missing here is shown from [`DOCUMENT`].
pub const DOCUMENT_EN: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../CHANGELOG.en.md"
));

/// The shipped notes in the language in force, newest first.
pub fn shipped() -> Vec<Release> {
    localized(DOCUMENT, DOCUMENT_EN, crate::i18n::current_locale())
}

/// `zh` and `en` read as one set of notes for `locale`: in English, each
/// release's English section where there is one and its Chinese one where there
/// is not — a release is never missing because nobody translated it yet.
///
/// Issues are kept once, in the Chinese file, under their own titles: an
/// English section without an Issues part shows the Chinese section's.
pub fn localized(zh: &str, en: &str, locale: crate::locale::Locale) -> Vec<Release> {
    let chinese = releases(zh);
    if locale != crate::locale::Locale::En {
        return chinese;
    }
    let mut out = releases(en);
    for release in &mut out {
        let Some(twin) = chinese.iter().find(|r| r.version == release.version) else {
            continue;
        };
        let issues = twin.parts().issues;
        if release.parts().issues.is_empty() && !issues.is_empty() {
            release.body = format!("{}\n\n### Issues\n\n{issues}", release.body.trim_end());
        }
    }
    for release in chinese {
        if !out.iter().any(|r| r.version == release.version) {
            out.push(release);
        }
    }
    out.sort_by(|a, b| b.version.cmp(&a.version));
    out
}

/// How many highlights the one-line notice names.
pub const NOTICE_HIGHLIGHTS: usize = 3;

/// A release number. A pre-release suffix (`-beta.1`) and build metadata are
/// not part of the order: the notes are written per release, not per build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// `v5.2.1`, `5.2.1`, `5.2.1-beta.1`. `None` for anything else.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text
            .strip_prefix('v')
            .or_else(|| text.strip_prefix('V'))
            .unwrap_or(text);
        let core = text.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let version = Version(
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
            parts.next()?.parse().ok()?,
        );
        parts.next().is_none().then_some(version)
    }

    /// This build's own number.
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is a release number")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.0, self.1, self.2)
    }
}

/// One release's section of the notes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// What follows the number on the heading line, brackets taken off:
    /// `## v5.2.1 (2026-10-08)` → `2026-10-08`.
    pub date: Option<String>,
    /// The section below its heading, as written.
    pub body: String,
}

/// A release's section split into what `/changelog` shows on its tabs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parts {
    /// Under `### 概览` / `### Overview`: a paragraph or two on what the release
    /// is about. Empty when the section has none.
    pub overview: String,
    /// Everything else: under `### 更新内容` / `### Changes`, under any other
    /// subheading (`### 新功能`, kept as written), and above the first one.
    pub changes: String,
    /// Under `### Issues` / `### 问题`: one issue per item,
    /// `- [#1182 标题](链接)`.
    pub issues: String,
}

impl Parts {
    /// How many issues the release lists: its top-level items.
    pub fn issue_count(&self) -> usize {
        self.issues.lines().filter(|line| is_item(line)).count()
    }
}

/// Which part a `### …` subheading opens, when it is one of the three.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Overview,
    Changes,
    Issues,
}

fn part_named(name: &str) -> Option<Part> {
    match name.trim().to_lowercase().as_str() {
        "概览" | "overview" => Some(Part::Overview),
        "更新内容" | "changes" => Some(Part::Changes),
        "issues" | "问题" => Some(Part::Issues),
        _ => None,
    }
}

fn is_item(line: &str) -> bool {
    line.starts_with("- ") || line.starts_with("* ")
}

impl Release {
    /// The section in its parts. A section written before the parts existed —
    /// no `###` of the three — is all changes.
    pub fn parts(&self) -> Parts {
        let mut parts = Parts::default();
        let mut into = Part::Changes;
        for line in self.body.lines() {
            if let Some(name) = line.strip_prefix("### ") {
                match part_named(name) {
                    Some(part) => {
                        into = part;
                        continue;
                    }
                    // Any other subheading is a group of changes, and stays.
                    None => into = Part::Changes,
                }
            }
            let to = match into {
                Part::Overview => &mut parts.overview,
                Part::Changes => &mut parts.changes,
                Part::Issues => &mut parts.issues,
            };
            to.push_str(line);
            to.push('\n');
        }
        for text in [&mut parts.overview, &mut parts.changes, &mut parts.issues] {
            *text = text.trim().to_string();
        }
        parts
    }

    /// The release's points in a few words each, in the order written: the bold
    /// lead of each top-level list item of its changes (`- **网页端按服务商管理模型**:
    /// ……`), or the whole item, inline marks taken off, when it has none. A nested
    /// item is detail of the one above it and not a point of its own; the
    /// overview and the issues are not points.
    pub fn highlights(&self) -> Vec<String> {
        self.parts()
            .changes
            .lines()
            .filter(|line| is_item(line))
            .filter_map(|line| line.get(2..))
            .map(|item| {
                let item = item.trim();
                let lead = item
                    .strip_prefix("**")
                    .and_then(|rest| rest.split_once("**"))
                    .map(|(lead, _)| lead);
                plain(lead.unwrap_or(item))
            })
            .filter(|point| !point.is_empty())
            .collect()
    }
}

/// Inline markdown taken off: what a one-line notice can show.
fn plain(text: &str) -> String {
    text.replace("**", "").replace('`', "").trim().to_string()
}

/// Every release in `document`, newest first — by number, not by where it
/// sits in the file, so a section pasted in the wrong place still sorts. A
/// release written twice keeps its first section.
pub fn releases(document: &str) -> Vec<Release> {
    let mut out: Vec<Release> = Vec::new();
    let mut open: Option<(Version, Option<String>, Vec<&str>)> = None;
    let close = |open: Option<(Version, Option<String>, Vec<&str>)>, out: &mut Vec<Release>| {
        if let Some((version, date, body)) = open {
            if !out.iter().any(|r| r.version == version) {
                out.push(Release {
                    version,
                    date,
                    body: body.join("\n").trim().to_string(),
                });
            }
        }
    };
    for line in document.lines() {
        if let Some(heading) = release_heading(line) {
            close(open.take(), &mut out);
            open = Some((heading.0, heading.1, Vec::new()));
        } else if let Some((_, _, body)) = open.as_mut() {
            body.push(line);
        }
    }
    close(open, &mut out);
    out.sort_by(|a, b| b.version.cmp(&a.version));
    out
}

/// `## v5.2.1 (2026-10-08)` → the number and the date. Only a level-two
/// heading whose first word is a release number opens a release.
fn release_heading(line: &str) -> Option<(Version, Option<String>)> {
    let rest = line.strip_prefix("## ")?.trim();
    // The number ends at a space or at a bracket written straight after it
    // (`## v5.2.0（2026-09-30）`, as Chinese text is often typed).
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '(' || c == '（')
        .unwrap_or(rest.len());
    let (number, after) = rest.split_at(end);
    let version = Version::parse(number)?;
    let date = after
        .trim()
        .trim_start_matches(['(', '（', '-', '—', '·'])
        .trim_end_matches([')', '）'])
        .trim();
    Some((version, (!date.is_empty()).then(|| date.to_string())))
}

/// What a person has been told about, read from [`seen_path`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// The newest release they were told about.
    pub last: Option<Version>,
    /// The one they had been told about before that — what `/changelog` marks
    /// as new is everything after it.
    pub before: Option<Version>,
}

impl Seen {
    /// Whether `version` is news to this person: after what they knew before
    /// the latest notice, up to that notice. With nothing recorded before it,
    /// only the release the notice was about.
    pub fn is_new(&self, version: Version) -> bool {
        match (self.before, self.last) {
            (_, None) => false,
            (Some(before), Some(last)) => version > before && version <= last,
            (None, Some(last)) => version == last,
        }
    }
}

/// Where what has been told is kept.
pub fn seen_path(config_dir: &Path) -> PathBuf {
    config_dir.join("changelog-seen")
}

/// What has been told so far. A missing or unreadable file is nothing told.
pub fn read_seen(path: &Path) -> Seen {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines = text.lines().map(Version::parse);
    Seen {
        last: lines.next().flatten(),
        before: lines.next().flatten(),
    }
}

/// Record that `version` has been told. Never moves back: a downgrade, or an
/// older build started beside a newer one, leaves the record as it was, so the
/// newer build does not tell its news a second time. Best effort — a record
/// that cannot be written means the notice comes once more, never a failure.
pub fn record_seen(path: &Path, version: Version) {
    let seen = read_seen(path);
    if seen.last.is_some_and(|last| last >= version) {
        return;
    }
    let mut text = format!("{version}\n");
    if let Some(last) = seen.last {
        text.push_str(&format!("{last}\n"));
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, text);
}

/// Record where a fresh install starts: `version`, with nothing before it that
/// is news — someone who never used an earlier build has no "since" for
/// `/changelog` to mark. Only when nothing is recorded yet.
pub fn record_start(path: &Path, version: Version) {
    if read_seen(path).last.is_some() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{version}\n{version}\n"));
}

/// What the first launch after an upgrade says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhatsNew {
    /// The release they were last told about, when there is one.
    pub from: Option<Version>,
    /// This build.
    pub to: Version,
    /// How many releases with notes lie between the two.
    pub releases: usize,
    /// At most [`NOTICE_HIGHLIGHTS`] points, newest release first.
    pub highlights: Vec<String>,
    /// There are more points than [`highlights`](Self::highlights) names.
    pub more: bool,
}

/// Whether this launch has news to tell, and what.
///
/// Nothing when:
/// - this release was told already, or an older build is running;
/// - nobody used this machine before (`existing_user` false and nothing
///   recorded): a fresh install has the welcome, not a list of changes it never
///   lived through;
/// - none of the releases since have notes.
///
/// Someone who used an earlier build that kept no record is told about this
/// release alone: how far back they came from is not known, and guessing would
/// hand them every release ever written.
pub fn whats_new(
    releases: &[Release],
    seen: &Seen,
    current: Version,
    existing_user: bool,
) -> Option<WhatsNew> {
    if seen.last.is_none() && !existing_user {
        return None;
    }
    if seen.last.is_some_and(|last| last >= current) {
        return None;
    }
    let told: Vec<&Release> = releases
        .iter()
        .filter(|r| match seen.last {
            Some(last) => r.version > last && r.version <= current,
            None => r.version == current,
        })
        .collect();
    if told.is_empty() {
        return None;
    }
    let mut highlights: Vec<String> = told.iter().flat_map(|r| r.highlights()).collect();
    let more = highlights.len() > NOTICE_HIGHLIGHTS;
    highlights.truncate(NOTICE_HIGHLIGHTS);
    Some(WhatsNew {
        from: seen.last,
        to: current,
        releases: told.len(),
        highlights,
        more,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# 更新日志

<!-- 给写的人看的,不展示 -->

## v5.2.1 (2026-10-08)

### 新功能
- **网页端按服务商管理模型**:一张卡片填完服务商和模型。
  - 嵌套的是细节,不是要点
- **推理强度可配置**:声明档位。

### 修复
- Windows 终端恢复 `彩色`

## v5.1.0

- **后台会话**:/bg

## v5.2.0（2026-09-30）

- **只有一点**
";

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn release_numbers_are_read_the_ways_they_are_written() {
        assert_eq!(Version::parse("v5.2.1"), Some(Version(5, 2, 1)));
        assert_eq!(Version::parse("5.2.1"), Some(Version(5, 2, 1)));
        assert_eq!(Version::parse(" V5.10.0-beta.1 "), Some(Version(5, 10, 0)));
        assert_eq!(Version::parse("5.2"), None);
        assert_eq!(Version::parse("5.2.1.4"), None);
        assert_eq!(Version::parse("all"), None);
        assert!(v("5.10.0") > v("5.9.9"), "by number, not by text");
        assert_eq!(v("5.2.1").to_string(), "v5.2.1");
    }

    #[test]
    fn the_document_splits_into_releases_newest_first() {
        let all = releases(DOC);
        let numbers: Vec<String> = all.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(numbers, ["v5.2.1", "v5.2.0", "v5.1.0"], "sorted by number");
        assert_eq!(all[0].date.as_deref(), Some("2026-10-08"));
        assert_eq!(
            all[1].date.as_deref(),
            Some("2026-09-30"),
            "full-width brackets"
        );
        assert_eq!(all[2].date, None);
        assert!(
            !all[0].body.contains("给写的人看的"),
            "the preamble is nobody's"
        );
        assert!(all[0].body.starts_with("### 新功能"), "{:?}", all[0].body);
    }

    /// 一节按三个小标题分开:概览、更新内容、Issues;别的小标题(新功能、修复)
    /// 留在更新内容里;要点只取更新内容的,不取概览和 Issues。
    #[test]
    fn a_section_splits_into_overview_changes_and_issues() {
        let doc = "\
## v5.2.2

### 概览
这一版的核心是架构。

### 更新内容
- **全新架构**:分层

### 修复
- 修了一个

### Issues
- [#1182 二维码](https://example.com/1182)
- [#1183 颜色](https://example.com/1183)
  - 细节不算一条
";
        let release = &releases(doc)[0];
        let parts = release.parts();
        assert_eq!(parts.overview, "这一版的核心是架构。");
        assert!(
            parts.changes.starts_with("- **全新架构**"),
            "{:?}",
            parts.changes
        );
        assert!(
            parts.changes.contains("### 修复\n- 修了一个"),
            "{:?}",
            parts.changes
        );
        assert!(!parts.changes.contains("1182"));
        assert_eq!(parts.issue_count(), 2);
        assert_eq!(release.highlights(), ["全新架构", "修了一个"]);
        let english = &releases(
            &doc.replace("### 概览", "### Overview")
                .replace("### 更新内容", "### Changes"),
        )[0];
        assert_eq!(english.parts(), parts, "the English names mean the same");
    }

    /// 没写小标题的旧格式,整节都是更新内容。
    #[test]
    fn a_section_without_parts_is_all_changes() {
        let release = &releases(DOC)[2];
        let parts = release.parts();
        assert_eq!(parts.overview, "");
        assert_eq!(parts.issues, "");
        assert_eq!(parts.changes, release.body);
    }

    #[test]
    fn highlights_are_the_bold_leads_of_top_level_items() {
        let all = releases(DOC);
        assert_eq!(
            all[0].highlights(),
            [
                "网页端按服务商管理模型",
                "推理强度可配置",
                "Windows 终端恢复 彩色"
            ]
        );
    }

    #[test]
    fn a_fresh_install_is_told_nothing() {
        assert_eq!(
            whats_new(&releases(DOC), &Seen::default(), v("5.2.1"), false),
            None
        );
    }

    #[test]
    fn someone_from_before_the_record_hears_about_this_release_only() {
        let news = whats_new(&releases(DOC), &Seen::default(), v("5.2.1"), true).unwrap();
        assert_eq!(news.from, None);
        assert_eq!(news.releases, 1);
        assert_eq!(news.highlights.len(), NOTICE_HIGHLIGHTS);
        assert!(!news.more, "exactly three points, none left out");
        assert_eq!(news.highlights[0], "网页端按服务商管理模型");
    }

    #[test]
    fn several_releases_skipped_are_one_notice() {
        let seen = Seen {
            last: Some(v("5.1.0")),
            before: None,
        };
        let news = whats_new(&releases(DOC), &seen, v("5.2.1"), true).unwrap();
        assert_eq!(news.from, Some(v("5.1.0")));
        assert_eq!(news.releases, 2, "5.2.0 and 5.2.1, not 5.1.0 again");
        assert_eq!(news.highlights[0], "网页端按服务商管理模型", "newest first");
    }

    #[test]
    fn told_once_and_never_on_a_downgrade() {
        let seen = Seen {
            last: Some(v("5.2.1")),
            before: Some(v("5.1.0")),
        };
        assert_eq!(
            whats_new(&releases(DOC), &seen, v("5.2.1"), true),
            None,
            "told already"
        );
        assert_eq!(
            whats_new(&releases(DOC), &seen, v("5.2.0"), true),
            None,
            "older build"
        );
    }

    #[test]
    fn a_release_without_notes_says_nothing() {
        let seen = Seen {
            last: Some(v("5.2.1")),
            before: None,
        };
        assert_eq!(whats_new(&releases(DOC), &seen, v("5.3.0"), true), None);
    }

    #[test]
    fn the_record_moves_forward_only_and_remembers_the_one_before() {
        let dir = tempfile::tempdir().unwrap();
        let path = seen_path(dir.path());
        assert_eq!(read_seen(&path), Seen::default(), "nothing yet");
        record_seen(&path, v("5.1.0"));
        record_seen(&path, v("5.2.1"));
        let seen = read_seen(&path);
        assert_eq!(seen.last, Some(v("5.2.1")));
        assert_eq!(seen.before, Some(v("5.1.0")));
        record_seen(&path, v("5.2.0"));
        assert_eq!(read_seen(&path), seen, "an older build leaves it as it was");
        record_seen(&path, v("5.2.1"));
        assert_eq!(read_seen(&path), seen, "the same release does not shift it");
    }

    /// A fresh install starts with nothing marked new, and its first upgrade
    /// marks what came after it.
    #[test]
    fn a_fresh_start_marks_nothing_new_until_the_next_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = seen_path(dir.path());
        record_start(&path, v("5.2.1"));
        let seen = read_seen(&path);
        assert!(!seen.is_new(v("5.2.1")) && !seen.is_new(v("5.2.0")));
        record_start(&path, v("5.3.0"));
        assert_eq!(read_seen(&path), seen, "only where nothing is recorded");
        record_seen(&path, v("5.3.0"));
        let after = read_seen(&path);
        assert!(after.is_new(v("5.3.0")) && !after.is_new(v("5.2.1")));
    }

    #[test]
    fn what_is_new_is_what_came_after_the_record_before() {
        let seen = Seen {
            last: Some(v("5.2.1")),
            before: Some(v("5.1.0")),
        };
        assert!(seen.is_new(v("5.2.1")) && seen.is_new(v("5.2.0")));
        assert!(!seen.is_new(v("5.1.0")));
        let first = Seen {
            last: Some(v("5.2.1")),
            before: None,
        };
        assert!(first.is_new(v("5.2.1")) && !first.is_new(v("5.2.0")));
        assert!(!Seen::default().is_new(v("5.2.1")));
    }

    /// The shipped notes parse in both languages, and every release in them has
    /// something to say.
    #[test]
    fn the_shipped_notes_parse() {
        for (name, document) in [("CHANGELOG.md", DOCUMENT), ("CHANGELOG.en.md", DOCUMENT_EN)] {
            let all = releases(document);
            assert!(!all.is_empty(), "{name} has at least one release");
            for release in &all {
                assert!(
                    !release.body.trim().is_empty(),
                    "{name}: {} has an empty section",
                    release.version
                );
            }
        }
    }

    /// In English, a release nobody translated yet is shown in Chinese rather
    /// than missing; in Chinese, the English file is never read.
    #[test]
    fn english_falls_back_to_chinese_per_release() {
        use crate::locale::Locale;
        let en = "## v5.2.1\n- **Providers on the web page**\n";
        let english = localized(DOC, en, Locale::En);
        let numbers: Vec<String> = english.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(numbers, ["v5.2.1", "v5.2.0", "v5.1.0"]);
        assert_eq!(english[0].highlights(), ["Providers on the web page"]);
        assert_eq!(
            english[1].highlights(),
            ["只有一点"],
            "untranslated: Chinese"
        );
        let chinese = localized(DOC, en, Locale::ZhCn);
        assert_eq!(chinese[0].highlights()[0], "网页端按服务商管理模型");
    }

    /// Issues 只写在中文那份里:英文界面里同一个版本照样有它们,标题是原文。
    #[test]
    fn english_shows_the_issues_written_once_in_chinese() {
        use crate::locale::Locale;
        let zh = "## v5.2.1\n### 概览\n中文\n### 更新内容\n- **一点**\n### Issues\n- [#1 二维码](https://example.com/1)\n";
        let en = "## v5.2.1\n### Overview\nEnglish\n### Changes\n- **A point**\n";
        let english = &localized(zh, en, Locale::En)[0];
        let parts = english.parts();
        assert_eq!(parts.overview, "English");
        assert_eq!(english.highlights(), ["A point"], "the English changes");
        assert_eq!(parts.issues, "- [#1 二维码](https://example.com/1)");
        let own =
            "## v5.2.1\n### Changes\n- **A point**\n### Issues\n- [#1 QR](https://example.com/1)\n";
        assert_eq!(
            localized(zh, own, Locale::En)[0].parts().issues,
            "- [#1 QR](https://example.com/1)",
            "an English Issues part of its own wins"
        );
    }
}
