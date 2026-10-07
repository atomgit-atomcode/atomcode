//! `/changelog`, and the one line the first launch after an upgrade says.
//!
//! **Why tell at all.** Most people never ran an upgrade: the launcher stages a
//! new build and the next launch is it (`main.rs`'s `latest.json` path). They
//! find out a release happened, if ever, by stumbling on something that moved.
//!
//! **Why only one line, and only once.** Releases come every few days, and a
//! launch that opens on a page of notes every time is the thing people asked
//! not to get. So the first interactive launch on a new release says what it
//! brought in a few words and where to read the rest; every launch after that
//! says nothing ([`atomcode_config::changelog`] keeps what was told). A fresh
//! install is told nothing — it has the welcome, and no "before" to compare to.
//! `[ui] whats_new = false` keeps every launch quiet; `/changelog` still answers.
//!
//! **What `/changelog` looks like.** The bottom sheet, the way `/resume` is: the
//! releases newest first, the ones that are news to this person marked, and under
//! the list the points of the selected one — what makes a person decide to open
//! it. Enter puts that release's notes in the conversation as a document, drawn
//! the way an answer is. `/changelog v5.2.1` goes straight there.

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_config::changelog::{self, Release, Seen, Version};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::command::{Command, CommandSet, Outcome};
use atomcode_tui::keymap::Action;
use atomcode_tui::plugin::CommandsSvc;
use atomcode_tui::sheet::{Doc, DocTab, List, Piece, Row, Sheet, Tone};
use serde_json::Value;

/// The row's name.
pub const ROW: &str = "tui-changelog";

/// How many points of the selected release show under the list.
const PREVIEW_POINTS: usize = 6;

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// Mounts `/changelog`.
pub struct ChangelogRow {
    /// Where what has been told is kept ([`changelog::seen_path`]): read, never
    /// written, by the command — opening the list is not being told.
    pub seen: PathBuf,
}

#[async_trait]
impl Plugin for ChangelogRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "/changelog: what changed in each release, picked from a list"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let commands = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        commands.add(Arc::new(Changelog {
            seen: self.seen.clone(),
        }))?;
        Ok(())
    }
}

struct Changelog {
    seen: PathBuf,
}

#[async_trait]
impl CommandSet for Changelog {
    fn id(&self) -> &'static str {
        ROW
    }

    fn commands(&self) -> Vec<Command> {
        vec![Command::said_taking(
            "changelog",
            "[version]".into(),
            tr(SMsg::CmdAboutChangelog),
        )]
    }

    async fn run(&self, _name: &str, args: &str, _ctx: &Context) -> Outcome {
        answer(
            args,
            &changelog::shipped(),
            Version::current(),
            changelog::read_seen(&self.seen),
        )
    }
}

/// `/changelog [version]` against `releases`, as `current` with `seen` told.
fn answer(args: &str, releases: &[Release], current: Version, seen: Seen) -> Outcome {
    // A section written ahead of the release it is for is not this build's.
    let releases: Vec<Release> = releases
        .iter()
        .filter(|release| release.version <= current)
        .cloned()
        .collect();
    if releases.is_empty() {
        return Outcome::Said(tr(SMsg::ChangelogEmpty).into_owned());
    }
    let asked = args.trim();
    if asked.is_empty() || asked.eq_ignore_ascii_case("all") {
        return Outcome::Do(Action::OpenSheet(Sheet::list(picker(
            &releases, current, seen,
        ))));
    }
    match Version::parse(asked).and_then(|v| releases.iter().find(|r| r.version == v)) {
        Some(release) => Outcome::Do(Action::OpenSheet(Sheet::doc(release_doc(release, current)))),
        None => Outcome::Refused(tr(SMsg::ChangelogNoSuchRelease { asked }).into_owned()),
    }
}

/// One release as a document on the sheet: 概览 (what it is about, then what
/// changed) and, when it lists any, Issues — each a title that opens its link.
/// Picked out of the list, Esc comes back to the list with the cursor on it
/// (`Host::settle_sheet_pick` keeps the list behind a page of the same command).
fn release_doc(release: &Release, current: Version) -> Doc {
    let parts = release.parts();
    let overview = [parts.overview.as_str(), parts.changes.as_str()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut tabs = vec![DocTab::new(tr(SMsg::ChangelogOverviewTab), overview)];
    let issues = parts.issue_count();
    if issues > 0 {
        tabs.push(DocTab::new(
            tr(SMsg::ChangelogIssuesTab { count: issues }),
            parts.issues.clone(),
        ));
    }
    let mut title = vec![Piece::new(release.version.to_string(), Tone::Plain)];
    if let Some(date) = &release.date {
        title.push(Piece::new(format!("  {date}"), Tone::Muted));
    }
    if release.version == current {
        title.push(Piece::new(
            format!("  {}", tr(SMsg::ChangelogThisBuild)),
            Tone::Muted,
        ));
    }
    Doc::new("changelog", title, tabs)
}

/// The releases as a list to pick from: newest first, the cursor on the newest.
/// Picking one runs `/changelog <version>`, the same as typing it.
fn picker(releases: &[Release], current: Version, seen: Seen) -> List {
    let rows = releases
        .iter()
        .map(|release| {
            let mut about = release.date.clone().unwrap_or_default();
            if release.version == current {
                if !about.is_empty() {
                    about.push_str("  ");
                }
                about.push_str(&tr(SMsg::ChangelogThisBuild));
            }
            let mut row = Row::new(
                format!("/changelog {}", release.version),
                release.version.to_string(),
            )
            .about(about)
            .preview(
                release
                    .highlights()
                    .into_iter()
                    .take(PREVIEW_POINTS)
                    .map(|point| format!("- {point}"))
                    .collect(),
            );
            if seen.is_new(release.version) {
                row = row.figures(vec![Piece::new(tr(SMsg::ChangelogNewTag), Tone::Added)]);
            }
            row
        })
        .collect();
    List::new("changelog", tr(SMsg::ChangelogPickerTitle), rows)
}

/// What an interactive launch has to say about the release it is, and the
/// record to make once it has.
pub struct Launch {
    /// The one line, when there is news and telling is on.
    pub notice: Option<String>,
    record: Option<Record>,
}

/// What [`Launch::told`] writes.
enum Record {
    /// A release told about, or passed over in silence on an upgrade.
    Seen(PathBuf, Version),
    /// The first launch on a machine nobody used before.
    Start(PathBuf, Version),
}

impl Launch {
    /// Decide for this launch. `whats_new` is `[ui] whats_new`; `had_config`
    /// whether a configuration file was there before this process could seed
    /// or write one — the sign that someone used this machine before.
    pub fn decide(whats_new: bool, had_config: bool, config_dir: &Path) -> Self {
        Self::decide_for(
            &changelog::shipped(),
            Version::current(),
            whats_new,
            had_config,
            config_dir,
        )
    }

    fn decide_for(
        releases: &[Release],
        current: Version,
        whats_new: bool,
        had_config: bool,
        config_dir: &Path,
    ) -> Self {
        let path = changelog::seen_path(config_dir);
        let seen = changelog::read_seen(&path);
        // Recorded even when nothing is said — a fresh install, telling turned
        // off, a release without notes — so the next upgrade knows where this
        // person came from rather than guessing.
        let record = if seen.last.is_none() && !had_config {
            Some(Record::Start(path, current))
        } else {
            seen.last
                .is_none_or(|last| last < current)
                .then_some(Record::Seen(path, current))
        };
        let notice = whats_new
            .then(|| changelog::whats_new(releases, &seen, current, had_config))
            .flatten()
            .map(|news| {
                let from = news.from.map(|v| v.to_string());
                let to = news.to.to_string();
                tr(SMsg::WhatsNewNotice {
                    from: from.as_deref(),
                    to: &to,
                    releases: news.releases,
                    highlights: &news.highlights,
                    more: news.more,
                })
                .into_owned()
            });
        Self { notice, record }
    }

    /// The notice is being handed to the screen: this release has been told.
    /// The screen draws the launch's notices the moment it opens — they wait on
    /// nothing, unlike the welcome's note — so the launch that gets this far is
    /// the launch that shows it. A headless run, or the classic screen (which
    /// has no `/changelog` to point at), never decides at all, and so never
    /// spends it.
    pub fn told(&self) {
        match &self.record {
            Some(Record::Seen(path, version)) => changelog::record_seen(path, *version),
            Some(Record::Start(path, version)) => changelog::record_start(path, *version),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
## v5.2.1 (2026-10-08)
- **网页端按服务商管理模型**:一张卡片。
- **推理强度**
- **第三点**
- **第四点**

## v5.2.0
- **后台会话**

## v5.3.0
- **还没发布**
";

    fn answer_doc(args: &str, document: &str, current: Version, seen: Seen) -> Outcome {
        answer(args, &changelog::releases(document), current, seen)
    }

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn seen(last: Option<&str>, before: Option<&str>) -> Seen {
        Seen {
            last: last.map(v),
            before: before.map(v),
        }
    }

    /// 不带参数:像 `/resume` 那样升起一张版本单子,最新的在最上,光标在它上面;
    /// 还没发布的版本不列。
    #[test]
    fn bare_changelog_opens_the_list_of_releases() {
        let Outcome::Do(Action::OpenSheet(sheet)) =
            answer_doc("", DOC, v("5.2.1"), seen(Some("5.2.1"), Some("5.1.0")))
        else {
            panic!("a list");
        };
        let atomcode_tui::sheet::Page::List(list) = sheet.page else {
            panic!("a list page");
        };
        let labels: Vec<&str> = list.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            ["v5.2.1", "v5.2.0"],
            "newest first, nothing unreleased"
        );
        assert_eq!(list.cursor, 0);
        assert_eq!(
            list.rows[0].value, "/changelog v5.2.1",
            "a pick is the typed command"
        );
        assert!(list.rows[0].about.contains("2026-10-08"));
        assert!(list.rows[0].preview[0].contains("网页端按服务商管理模型"));
        // 5.1.0 之后的都算新的。
        assert!(!list.rows[0].figures.is_empty() && !list.rows[1].figures.is_empty());
    }

    #[test]
    fn what_was_told_before_is_not_marked_new() {
        let Outcome::Do(Action::OpenSheet(sheet)) =
            answer_doc("all", DOC, v("5.2.1"), seen(Some("5.2.1"), Some("5.2.0")))
        else {
            panic!("a list");
        };
        let atomcode_tui::sheet::Page::List(list) = sheet.page else {
            panic!("a list page");
        };
        assert!(!list.rows[0].figures.is_empty(), "5.2.1 is new");
        assert!(list.rows[1].figures.is_empty(), "5.2.0 was told before");
    }

    fn doc_of(outcome: Outcome) -> Doc {
        let Outcome::Do(Action::OpenSheet(sheet)) = outcome else {
            panic!("a sheet: {outcome:?}");
        };
        let atomcode_tui::sheet::Page::Doc(doc) = sheet.page else {
            panic!("a document page");
        };
        doc
    }

    /// 选中(或直接打)一个版本:在面板里打开它 —— 概览一页;列了 Issues 的再多一页。
    #[test]
    fn a_release_named_opens_as_a_document_with_its_tabs() {
        let _locale = atomcode_config::i18n::test_lock();
        atomcode_config::i18n::set_locale(atomcode_config::locale::Locale::ZhCn);
        let said = answer_doc("v5.2.0", DOC, v("5.2.1"), Seen::default());
        let doc = doc_of(said.clone());
        assert_eq!(doc.tabs.len(), 1, "no issues, no Issues tab");
        assert_eq!(doc.tabs[0].markdown, "- **后台会话**");
        assert_eq!(answer_doc("5.2.0", DOC, v("5.2.1"), Seen::default()), said);

        let with_parts = "## v5.2.1 (2026-10-08)\n\n### 概览\n核心是架构。\n\n### 更新内容\n- **全新架构**\n\n### Issues\n- [#1182 二维码](https://example.com/1182)\n";
        let doc = doc_of(answer_doc(
            "v5.2.1",
            with_parts,
            v("5.2.1"),
            Seen::default(),
        ));
        let names: Vec<&str> = doc.tabs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["概览", "Issues (1)"]);
        assert_eq!(
            doc.tabs[0].markdown, "核心是架构。\n\n- **全新架构**",
            "overview, then changes"
        );
        assert!(doc.tabs[1]
            .markdown
            .contains("[#1182 二维码](https://example.com/1182)"));
        let title: String = doc.title.iter().map(|p| p.text.as_str()).collect();
        assert!(
            title.contains("v5.2.1") && title.contains("2026-10-08"),
            "{title}"
        );
        assert!(matches!(
            answer_doc("v4.0.0", DOC, v("5.2.1"), Seen::default()),
            Outcome::Refused(_)
        ));
        assert!(
            matches!(
                answer_doc("v5.3.0", DOC, v("5.2.1"), Seen::default()),
                Outcome::Refused(_)
            ),
            "not this build's"
        );
    }

    #[test]
    fn a_build_with_no_notes_says_so() {
        assert!(matches!(
            answer_doc("", "# nothing", v("5.2.1"), Seen::default()),
            Outcome::Said(_)
        ));
    }

    /// 升级后第一次:一行,点出亮点和去哪儿看;记下来以后就不再说。
    #[test]
    fn the_first_launch_after_an_upgrade_tells_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = changelog::seen_path(dir.path());
        changelog::record_seen(&path, v("5.2.0"));

        let launch = Launch::decide_for(
            &changelog::releases(DOC),
            v("5.2.1"),
            true,
            true,
            dir.path(),
        );
        let notice = launch.notice.clone().expect("news");
        assert!(notice.contains("v5.2.1"), "{notice}");
        assert!(notice.contains("网页端按服务商管理模型"), "{notice}");
        assert!(notice.contains("/changelog"), "{notice}");
        assert!(
            !notice.contains("第四点"),
            "three points, not all: {notice}"
        );

        // Deciding is not telling: a launch that never got the screen up says it again.
        let again = Launch::decide_for(
            &changelog::releases(DOC),
            v("5.2.1"),
            true,
            true,
            dir.path(),
        );
        assert!(again.notice.is_some());

        launch.told();
        let after = Launch::decide_for(
            &changelog::releases(DOC),
            v("5.2.1"),
            true,
            true,
            dir.path(),
        );
        assert_eq!(after.notice, None, "told once");
    }

    #[test]
    fn a_fresh_install_is_told_nothing_but_remembers_where_it_started() {
        let dir = tempfile::tempdir().unwrap();
        let launch = Launch::decide_for(
            &changelog::releases(DOC),
            v("5.2.1"),
            true,
            false,
            dir.path(),
        );
        assert_eq!(launch.notice, None);
        launch.told();
        let seen = changelog::read_seen(&changelog::seen_path(dir.path()));
        assert_eq!(seen.last, Some(v("5.2.1")));
        assert!(
            !seen.is_new(v("5.2.1")),
            "nothing is news to a fresh install"
        );
    }

    #[test]
    fn turned_off_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        changelog::record_seen(&changelog::seen_path(dir.path()), v("5.2.0"));
        let launch = Launch::decide_for(
            &changelog::releases(DOC),
            v("5.2.1"),
            false,
            true,
            dir.path(),
        );
        assert_eq!(launch.notice, None);
    }
}
