//! What this launch has to say for itself, handed to the screen as a row.
//!
//! Three things are decided before the screen exists and cannot be asked of the
//! host afterwards, because none of them is a fact about the session:
//!
//! - the configuration file did not parse, and the defaults are in force;
//! - `resume <id>` named a session belonging to another project, so the working
//!   directory moved — and with it that project's hooks and MCP servers;
//! - the session asked for was busy, so this one is a fork of it.
//!
//! The previous front end took them as an argument to its `run`
//! (`atomcode_tuix::run`'s `startup_notice`). This screen is an App apart
//! (`docs/adr/0022` §3): what a launcher has to contribute, it contributes as a
//! row, so it takes its place in the tree like every other one — `--audit` sees
//! it, `[[remove]]` can take it out, and the screen is assembled the same way
//! with or without it.
//!
//! **Why this is not stderr.** It was, briefly, and that is the same as silence:
//! entering the alternate screen clears what was written before it. A launch
//! that printed "your config did not parse" and then opened a full-screen UI had
//! told nobody.

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::content::{OpeningNotices, WelcomeNoteSeen};
use atomcode_tui::plugin::{OpeningNoticesSvc, WelcomeNoteSeenSvc};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The row's name.
pub const ROW: &str = "tui-opening-notices";

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// Carries this launch's notices to [`OpeningNoticesSvc`].
pub struct OpeningRow {
    /// The merged notice as the launcher built it, `None` on an ordinary launch.
    pub notice: Option<String>,
    /// The first-launch keys line ([`keys_note`]), for the foot of the first
    /// welcome block. `None` on every launch after the first.
    pub keys: Option<KeysNote>,
    /// The first launch on a new release: its one line about what changed
    /// (`crate::tui_changelog`), at the foot of the welcome too — under the
    /// working directory and the model, an aside about this build, not news
    /// standing over the screen. Recorded as told when the welcome is drawn.
    pub news: Option<crate::tui_changelog::Launch>,
}

/// The first-launch keys line, and where to record that it was drawn.
#[derive(Clone, Debug)]
pub struct KeysNote {
    pub text: String,
    pub marker: PathBuf,
}

/// Records what the welcome's foot said as shown when the screen says it was
/// drawn — not when the screen came up, which is before the welcome it rides
/// on, and not at all on a launch whose welcome stood down (a resumed session
/// with history): the keys line and the release's news both come round again.
struct RememberWhenSeen {
    keys: Option<PathBuf>,
    news: Option<crate::tui_changelog::Launch>,
}

impl WelcomeNoteSeen for RememberWhenSeen {
    fn seen(&self) {
        if let Some(marker) = &self.keys {
            remember_keys_notice(marker);
        }
        if let Some(news) = &self.news {
            news.told();
        }
    }
}

/// The welcome's foot: the keys line, then the release's news, one per line.
fn welcome_note(
    keys: Option<&KeysNote>,
    news: Option<&crate::tui_changelog::Launch>,
) -> Option<String> {
    let lines: Vec<&str> = keys
        .map(|keys| keys.text.as_str())
        .into_iter()
        .chain(news.and_then(|news| news.notice.as_deref()))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// One notice per line.
///
/// The launcher joins what it has with `\n` (`merge_startup_notices`), and each
/// piece is its own sentence about its own thing — a config file, a directory, a
/// fork. The screen draws one block per notice, so splitting here is what keeps
/// three unrelated pieces of news from reading as one paragraph. Blank lines are
/// dropped rather than drawn as empty blocks.
pub fn notices(notice: Option<&str>) -> OpeningNotices {
    OpeningNotices {
        notices: notice
            .into_iter()
            .flat_map(|text| text.lines())
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect(),
        welcome_note: None,
    }
}

/// Where the first-launch keys notice records that it has been said.
pub fn keys_notice_marker(config_dir: &Path) -> PathBuf {
    config_dir.join("tui-keys-notice-shown")
}

/// A line of keys for the foot of the welcome block — on the first launch of
/// this screen only, except where the terminal reports no mouse (HarmonyOS).
///
/// There it is every launch: the mouse is the terminal's, the wheel does not
/// scroll the conversation, and PageUp/PageDown and `/raw` are how to read back
/// — keys nothing else on that screen names, and not ones a person keeps from
/// a single first launch.
///
/// Reasoning is hidden and tool output has its own key, and neither says so on
/// screen. It is an aside about the screen, so it sits under the working
/// directory and the model, dim, rather than above the welcome with the news
/// about this launch (a config that did not parse) — where it read as the
/// most important thing on the screen.
///
/// Only decides: the marker is written by [`remember_keys_notice`] when the
/// screen reports the line drawn ([`WelcomeNoteSeen`]), so a launch that never
/// drew it — no welcome yet when it quit, a config that stopped the agent from
/// describing itself — has not spent it.
pub fn keys_note(marker: &Path) -> Option<KeysNote> {
    keys_note_for(marker, atomcode_tui::caps::mouse_reported())
}

fn keys_note_for(marker: &Path, mouse_reported: bool) -> Option<KeysNote> {
    (!mouse_reported || !marker.exists()).then(|| KeysNote {
        // Where the terminal reports no mouse (HarmonyOS) there is no ctrl-g to
        // name, and the conversation scrolls with PageUp/PageDown.
        text: atomcode_config::i18n::t(if mouse_reported {
            atomcode_config::i18n::Msg::TuiKeysHint
        } else {
            atomcode_config::i18n::Msg::TuiKeysHintNoMouse
        })
        .into_owned(),
        marker: marker.to_path_buf(),
    })
}

/// Record that the keys notice has been shown. If this cannot be written the
/// notice comes back next launch, which is the better way to be wrong.
pub fn remember_keys_notice(marker: &Path) {
    if let Some(parent) = marker.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(marker, b"");
}

/// Set by the launcher when it re-executes into a newly installed binary; the
/// version it came from.
pub const UPGRADED_FROM_ENV: &str = "ATOMCODE_UPGRADED_FROM";

/// This launch's notice, with "upgraded from vA to vB" added when the launch is
/// the restart an upgrade made. Last, like any standing news about the launch.
pub fn with_upgrade_notice(
    notice: Option<String>,
    upgraded_from: Option<String>,
) -> Option<String> {
    let Some(from) = upgraded_from.filter(|v| !v.trim().is_empty()) else {
        return notice;
    };
    let to = format!("v{}", env!("CARGO_PKG_VERSION"));
    let said = atomcode_config::i18n::t(atomcode_config::i18n::Msg::UpgradeSuccess {
        from: &from,
        to: &to,
    })
    .into_owned();
    Some(match notice {
        Some(notice) => format!("{notice}\n{said}"),
        None => said,
    })
}

#[async_trait]
impl Plugin for OpeningRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-opening-notices", "tui-welcome-note-seen"]
    }
    fn description(&self) -> &'static str {
        "what this launch has to say for itself: a config that did not parse, a working directory that moved, a session that was forked"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<OpeningNoticesSvc>(Arc::new(OpeningNotices {
                welcome_note: welcome_note(self.keys.as_ref(), self.news.as_ref()),
                ..notices(self.notice.as_deref())
            }))
            .map_err(|e| e.to_string())?;
        let news = self.news.clone().filter(|news| news.notice.is_some());
        if self.keys.is_some() || news.is_some() {
            let _ = ctx
                .provide::<WelcomeNoteSeenSvc>(Arc::new(RememberWhenSeen {
                    keys: self.keys.as_ref().map(|keys| keys.marker.clone()),
                    news,
                }))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each piece of news is its own notice.
    ///
    /// The launcher merges with `\n` and the three producers are unrelated: a
    /// config file, a working directory, a fork. One block each is what lets a
    /// person read the one that concerns them; joined, the middle one is the
    /// line nobody finishes.
    #[test]
    fn a_merged_notice_becomes_one_notice_per_piece() {
        let merged = "resume moved to ~/other\nconfig.toml did not parse\nforked from s-1";
        assert_eq!(
            notices(Some(merged)).notices,
            vec![
                "resume moved to ~/other",
                "config.toml did not parse",
                "forked from s-1",
            ]
        );
    }

    /// The keys line is offered on the first launch only, and for the foot of
    /// the welcome — not as one more notice over it, where it read as the most
    /// important thing on the screen.
    #[test]
    fn the_keys_line_is_for_the_first_launch_and_the_welcomes_foot() {
        let dir = std::env::temp_dir().join(format!(
            "atomcode-keys-notice-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        let marker = keys_notice_marker(&dir);

        let keys = keys_note(&marker).expect("the first launch has it").text;
        assert!(keys.contains("/keys"), "{keys}");
        assert!(
            keys_note(&marker).is_some(),
            "deciding does not spend it: a launch that never drew says it again"
        );

        // Carried apart from the news about this launch, never merged into it.
        let row = OpeningRow {
            notice: Some("config.toml did not parse".into()),
            keys: keys_note(&marker),
            news: None,
        };
        let said = OpeningNotices {
            welcome_note: welcome_note(row.keys.as_ref(), row.news.as_ref()),
            ..notices(row.notice.as_deref())
        };
        assert_eq!(said.notices, vec!["config.toml did not parse".to_string()]);
        assert_eq!(said.welcome_note.as_deref(), Some(keys.as_str()));

        assert!(!marker.exists(), "not spent by being handed to the screen");
        // Spent when the screen says it drew it.
        RememberWhenSeen {
            keys: Some(marker.clone()),
            news: None,
        }
        .seen();
        assert!(marker.exists(), "remembered once it was drawn");
        assert!(keys_note_for(&marker, true).is_none(), "never again");
        // Where the terminal reports no mouse (HarmonyOS) the line is how to
        // scroll and select at all, so it is there every launch — and names
        // PageUp/PageDown rather than a ctrl-g that does nothing there.
        let always = keys_note_for(&marker, false)
            .expect("every launch there")
            .text;
        assert!(
            always.contains("PageUp") && !always.contains("ctrl-g"),
            "{always}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A launch that is the restart an upgrade made says so, after whatever
    /// else it has to say; an ordinary launch adds nothing.
    #[test]
    fn a_launch_after_an_upgrade_says_where_it_came_from() {
        assert_eq!(with_upgrade_notice(None, None), None);
        assert_eq!(
            with_upgrade_notice(Some("forked from s-1".into()), Some(" ".into())).as_deref(),
            Some("forked from s-1")
        );
        let said = with_upgrade_notice(Some("forked from s-1".into()), Some("v5.1.0".into()))
            .expect("something to say");
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(lines.len(), 2, "{said}");
        assert_eq!(lines[0], "forked from s-1");
        assert!(lines[1].contains("v5.1.0"), "{said}");
    }

    /// An ordinary launch says nothing, and says it as nothing rather than as an
    /// empty block: a blank notice is a `⚑` with no words after it.
    #[test]
    fn an_ordinary_launch_has_nothing_to_say() {
        assert_eq!(notices(None).notices, Vec::<String>::new());
        assert_eq!(notices(Some("")).notices, Vec::<String>::new());
        assert_eq!(notices(Some("\n  \n")).notices, Vec::<String>::new());
    }
}
