//! The welcome block's words, from the product's own localisation.
//!
//! The screen owns the *shape* of the opening block; it does not own the
//! sentences in it. Those live in `atomcode_config::i18n` — the same table the
//! previous front end reads and the same one `/language` switches — so this
//! module is the one hop between the two, and it is a row so the screen can be
//! assembled without it (see `atomcode_tui::plugin::WelcomeWordsSvc`).
//!
//! **Why a seam and not a copy.** The alternative is a second list of tip
//! descriptions inside `atomcode-tui`, and a second list is how the welcome
//! screen comes to describe `/init` differently from `/init`'s own help. The
//! screen asks by command name; whatever answers reads the product's table.
//!
//! This crate is also the one that can see both sides: `atomcode-tui` must stay
//! free of `atomcode-config` (the screen is an App apart, `docs/adr/0022` §3),
//! and `atomcode-config` must not learn about a screen. The launcher is where
//! the two are introduced, which is the same place `tui_settings` and
//! `tui_onboarding` live for the same reason.

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::content::WelcomeWords;
use atomcode_tui::plugin::WelcomeWordsSvc;
use serde_json::Value;
use std::sync::Arc;

/// The row's name.
pub const ROW: &str = "tui-welcome-words";

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// Fills [`WelcomeWordsSvc`] from the i18n table.
pub struct WelcomeWordsRow;

#[async_trait]
impl Plugin for WelcomeWordsRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tui-welcome-words"]
    }
    fn description(&self) -> &'static str {
        "the welcome block's heading and tip descriptions, read from the product's own language table"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<WelcomeWordsSvc>(Arc::new(I18nWords))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// The table, read at the moment a block is built.
///
/// Read **per call** rather than captured at mount: `/language` changes the
/// table's answer mid-session, and a value copied when the row came up would
/// keep the old language until the next start. The screen builds its block once
/// per session, so a session opened after the switch reads the new language and
/// the one already on screen keeps the language it opened in — which is what a
/// block of history should do.
struct I18nWords;

impl WelcomeWords for I18nWords {
    fn heading(&self) -> String {
        use atomcode_config::i18n::{t, Msg};
        t(Msg::WelcomeTipsHeading).into_owned()
    }

    fn about(&self, command: &str) -> Option<String> {
        use atomcode_config::i18n::{t, Msg};
        // One arm per command the welcome block suggests. The names are the
        // screen's (`atomcode_tui::modules::welcome`'s `CANDIDATES`); a name
        // that is not here gets `None` and the tip falls back to the command's
        // own description, which is why this list may lag without breaking
        // anything.
        let msg = match command {
            "login" => Msg::WelcomeTipLogin,
            "provider" => Msg::WelcomeTipProvider,
            "model" => Msg::WelcomeTipModel,
            "resume" => Msg::WelcomeTipResume,
            "setup" => Msg::WelcomeTipSetup,
            "skills" => Msg::WelcomeTipSkills,
            "plugin" => Msg::WelcomeTipPlugin,
            "webui" => Msg::WelcomeTipWebui,
            "mcp" => Msg::WelcomeTipMcp,
            "plan" => Msg::WelcomeTipPlan,
            "session" => Msg::WelcomeTipSession,
            "loop" => Msg::WelcomeTipLoop,
            "goal" => Msg::WelcomeTipGoal,
            "init" => Msg::WelcomeTipInit,
            "language" => Msg::WelcomeTipLanguage,
            "usage" => Msg::WelcomeTipUsage,
            _ => return None,
        };
        Some(t(msg).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_config::i18n::{set_locale, test_lock, Locale};

    /// The words follow the locale, which is the property the seam exists for:
    /// one table, two languages, no second list to keep in step.
    #[test]
    fn the_same_block_reads_differently_in_another_language() {
        let _guard = test_lock();

        set_locale(Locale::ZhCn);
        assert_eq!(I18nWords.heading(), "上手提示");
        assert_eq!(
            I18nWords.about("init").as_deref(),
            Some("扫描代码库生成 AGENTS.md")
        );
        assert_eq!(
            I18nWords.about("session").as_deref(),
            Some("管理与切换会话")
        );

        set_locale(Locale::En);
        assert_eq!(I18nWords.heading(), "Tips for getting started");
        assert_eq!(
            I18nWords.about("init").as_deref(),
            Some("scan the codebase into AGENTS.md")
        );
    }

    /// A command the table has no line for is not an error and not a blank: the
    /// caller falls back to the command's own description. Asserted here so the
    /// contract the screen relies on is pinned next to the implementation.
    #[test]
    fn a_command_not_in_the_table_answers_nothing_rather_than_empty_text() {
        let _guard = test_lock();
        assert_eq!(I18nWords.about("mouse"), None);
        assert_eq!(I18nWords.about(""), None);
    }

    /// Every name the screen's pool can put on the welcome screen has a line in
    /// the table.
    ///
    /// The two lists live in different crates and cannot see each other, so this
    /// is the join: a name added to the pool with no message here would silently
    /// degrade to the command's own `about` — readable, but no longer the
    /// sentence the other front end shows for the same row.
    #[test]
    fn every_command_the_pool_offers_has_a_line() {
        let _guard = test_lock();
        // `atomcode_tui::modules::welcome`'s `CANDIDATES` — restated, because
        // this crate cannot read a private const of a module it does not own.
        for command in [
            "login", "provider", "model", "resume", "setup", "skills", "plugin", "webui", "mcp",
            "plan", "session", "loop", "goal", "init", "language", "usage",
        ] {
            assert!(
                I18nWords.about(command).is_some(),
                "`/{command}` is in the welcome pool but has no line in the i18n table"
            );
        }
    }
}
