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
use atomcode_tui::content::OpeningNotices;
use atomcode_tui::plugin::OpeningNoticesSvc;
use serde_json::Value;
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
}

/// One notice per line.
///
/// The launcher joins what it has with `\n` (`merge_startup_notices`), and each
/// piece is its own sentence about its own thing — a config file, a directory, a
/// fork. The screen draws one block per notice, so splitting here is what keeps
/// three unrelated pieces of news from reading as one paragraph. Blank lines are
/// dropped rather than drawn as empty blocks.
pub fn notices(notice: Option<&str>) -> OpeningNotices {
    OpeningNotices(
        notice
            .into_iter()
            .flat_map(|text| text.lines())
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect(),
    )
}

#[async_trait]
impl Plugin for OpeningRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-opening-notices"]
    }
    fn description(&self) -> &'static str {
        "what this launch has to say for itself: a config that did not parse, a working directory that moved, a session that was forked"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<OpeningNoticesSvc>(Arc::new(notices(self.notice.as_deref())))
            .map_err(|e| e.to_string())?;
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
            notices(Some(merged)).0,
            vec![
                "resume moved to ~/other",
                "config.toml did not parse",
                "forked from s-1",
            ]
        );
    }

    /// An ordinary launch says nothing, and says it as nothing rather than as an
    /// empty block: a blank notice is a `⚑` with no words after it.
    #[test]
    fn an_ordinary_launch_has_nothing_to_say() {
        assert_eq!(notices(None).0, Vec::<String>::new());
        assert_eq!(notices(Some("")).0, Vec::<String>::new());
        assert_eq!(notices(Some("\n  \n")).0, Vec::<String>::new());
    }
}
