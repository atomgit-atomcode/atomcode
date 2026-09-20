//! The settings port for `atomcode --tui`, filled by the launcher.
//!
//! The screen draws the settings and works them; it does not know where they
//! live. This is the other half: which file, how to write it, and when a change
//! has to be handed to the runtime. The split is `docs/adr/0022` §3 — the screen
//! is an App apart, and the configuration is the product's.
//!
//! Two facts decide the shape:
//!
//! - **The catalog is `atomcode_config::settings`'.** The same list `/config`
//!   has always been, the same one the agent is told about, so a setting added
//!   there appears on screen with no edit here. This module only *presents* it —
//!   label, current value, and which gesture edits it.
//! - **A write is a document patch, and the effect is a reload.** The file is
//!   the product's state; the running graph learns about it the way it learns
//!   about any other edit on disk, through `HostCommand::Reload`, which reads
//!   the configuration again and rebuilds only what changed
//!   (`atomcode-coding/src/front_end.rs`).

use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::module::{Modules, Mounted};
use atomcode_tui::plugin::ModulesSvc;
use atomcode_tui::settings::{Applies, SettingKind, SettingRow, Settings, SettingsView};
use serde_json::Value;

/// The row that puts the settings panel on screen.
///
/// **The launcher's row, not the screen's**, and that is the point of it living
/// here: `atomcode-tui` ships the panel's *view* — how a list of settings is
/// drawn, searched and edited — and knows nothing about a configuration file, a
/// catalog or a product. The row exists because *this* product has settings to
/// show, so this is where it is written, and `launch::catalog_with` is how it
/// reaches the tree.
///
/// It mounts and nothing else, the shape `tui-panel-ask` has: place is the
/// screen's business (`host::TAIL`), and what to draw comes over `tui-settings`.
pub struct SettingsRow;

#[async_trait]
impl Plugin for SettingsRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn description(&self) -> &'static str {
        "the settings: search, the list as this launcher reads it, and an edit going back over the seam"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let view = Arc::new(Mounted::<atomcode_tui::modules::settings::Settings>::new());
        let id = <atomcode_tui::modules::settings::Settings as atomcode_tui::module::View>::id();
        mods.add_view(view)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_view(id));
        Ok(())
    }
}

/// The row's name, one string shared by the plugin and the layer that names it.
pub const ROW: &str = "tui-panel-settings";

/// The layer that puts the row on screen.
///
/// `[[insert]]` rather than a patch: the screen's own tree does not name this row
/// at all, because a screen with no settings port has no settings — see
/// `rows.rs`. So the launcher inserts the row it owns, and the two travel
/// together: a row with no port behind it would be a panel drawing an empty list.
///
/// Built from [`ROW`] rather than written out, so the name a plugin answers to
/// and the name a layer asks for cannot drift — a drift that shows up as a mount
/// failure naming a row nobody registered.
pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// Where the settings live, and who to tell when one changes.
///
/// Holds the config file rather than the loaded `Config`: a value read once at
/// start-up is a value that goes stale the moment anything else writes the file,
/// and the whole point of the panel is that it shows what is *there*.
pub struct ConfigSettings {
    pub path: PathBuf,
}

impl ConfigSettings {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self { path })
    }

    /// The catalog as rows, read from the file now.
    ///
    /// A file that will not parse or cannot be read is not an error here: the
    /// panel shows the settings the build would use, which is what a person sees
    /// before anything is written. Refusing to open the panel because the file
    /// is unreadable would leave them with no way to see what is wrong.
    fn read(&self) -> SettingsView {
        use atomcode_config::config::Config;
        let config = Config::load(&self.path).unwrap_or_default();
        SettingsView::new(
            atomcode_config::settings::SETTINGS
                .iter()
                .map(|spec| SettingRow {
                    id: spec.id.to_string(),
                    // The catalog carries both languages and `label()` picks
                    // the one in force. Every caller used to reach for
                    // `label_zh` directly, so an English session read this
                    // panel in Chinese.
                    label: spec.label().to_string(),
                    value: spec.value(&config),
                    kind: kind_of(spec.kind),
                    applies: applies_of(spec.apply),
                })
                .chain(retry_row(&config))
                .collect(),
        )
    }
}

/// The id of the one setting that is not in the static catalog.
///
/// It cannot be: what it reads and writes lives under the *current selection* —
/// `[models.<id>]` or `[providers.<id>]` — and which one that is changes with
/// `/model`. A static spec would have to name a path, and there is no one path.
const RETRY: &str = "model.retry_max_attempts";

/// How many times the current model's requests are retried, when it says.
///
/// Built here rather than in the catalog for the reason above, and read through
/// `selection_retry_max_attempts` rather than off the provider table, because a
/// provider entry carries an `api_key` and nothing that ends up on a screen may
/// go near one (`docs/plans/2026-09-19-remaining-gaps.md`, and the same rule
/// `ProviderChoice::about` follows).
///
/// Absent when nothing is selected: a row about "the current model" with no
/// current model is a row whose value nobody can explain.
fn retry_row(config: &atomcode_config::config::Config) -> Option<SettingRow> {
    let selection = config.default_model.clone()?;
    let value = atomcode_config::settings::selection_retry_max_attempts(config, &selection)
        .map(|n| n.to_string())
        .unwrap_or_default();
    Some(SettingRow {
        id: RETRY.to_string(),
        label: tr(SMsg::RetryCountFor {
            selection: &selection,
        })
        .into_owned(),
        value,
        kind: SettingKind::Integer { min: 0, max: 10 },
        applies: Applies::Reprepare,
    })
}

/// Write the current selection's retry override, or take it away.
///
/// `None` is the reset: `patch_selection_retry_max_attempts` removes the key so
/// the per-layer defaults stand again, which is the same meaning
/// [`Settings::reset`] has everywhere else.
fn write_retry(path: &PathBuf, value: Option<&str>) -> Result<(), String> {
    use atomcode_config::config::Config;
    let config = Config::load(path).map_err(|error| format!("{error:#}"))?;
    let Some(selection) = config.default_model.clone() else {
        return Err(tr(SMsg::NoModelSelected).into_owned());
    };
    atomcode_config::ConfigStore::new(path.clone())
        .update_document(|document| {
            atomcode_config::settings::patch_selection_retry_max_attempts(
                document, &selection, value,
            )
        })
        .map_err(|error| format!("{error:#}"))?;
    Ok(())
}

impl Settings for ConfigSettings {
    fn rows(&self) -> SettingsView {
        self.read()
    }

    /// Patch the document, and answer with what the file now says.
    ///
    /// A document patch rather than a rewrite of the typed `Config`: the file is
    /// a person's, comments and ordering included, and `Config` → TOML would
    /// throw away everything this program does not model. `SettingSpec::patch`
    /// is the same call the previous front end made, and it validates the value
    /// — which is why its refusal is passed through verbatim instead of being
    /// second-guessed here.
    fn set(&self, id: &str, value: &str) -> Result<SettingsView, String> {
        if id == RETRY {
            write_retry(&self.path, Some(value))?;
            return Ok(self.read());
        }
        let Some(spec) = atomcode_config::settings::SETTINGS
            .iter()
            .find(|spec| spec.id == id)
        else {
            return Err(tr(SMsg::NoSuchSetting { id }).into_owned());
        };
        let store = atomcode_config::ConfigStore::new(self.path.clone());
        store
            .update_document(|document| spec.patch(document, value))
            .map_err(|error| format!("{error:#}"))?;
        if id == "language" {
            apply_language(value);
        }
        Ok(self.read())
    }

    /// Take the key out of the file, so this build's own answer stands again.
    ///
    /// `SettingSpec::reset` removes it rather than writing today's default in:
    /// a setting that was unset follows the build from then on, and one written
    /// with the default's current value stops following. Only the first is what
    /// "restore the default" means, and the two look identical on screen the
    /// day it is done.
    fn reset(&self, id: &str) -> Result<SettingsView, String> {
        if id == RETRY {
            write_retry(&self.path, None)?;
            return Ok(self.read());
        }
        let Some(spec) = atomcode_config::settings::SETTINGS
            .iter()
            .find(|spec| spec.id == id)
        else {
            return Err(tr(SMsg::NoSuchSetting { id }).into_owned());
        };
        atomcode_config::ConfigStore::new(self.path.clone())
            .update_document(|document| {
                spec.reset(document);
                Ok(())
            })
            .map_err(|error| format!("{error:#}"))?;
        if id == "language" {
            // Cleared means the build's own answer stands again, and for this key
            // that answer is "ask the environment".
            apply_language("auto");
        }
        Ok(self.read())
    }
}

/// The one setting whose change is not only a file edit.
///
/// `language` is declared `ImmediateUi`, and "immediately" for it means the
/// process's i18n table — what `/config language zh_CN` promises is the screen
/// changing language now, not at the next start. Writing the file and stopping
/// there leaves the file saying one thing and every string drawn from the table
/// saying another until the process restarts.
///
/// Called from both the settings panel and host control, because both write this
/// key: one implementation, so the two paths cannot come to mean different
/// things by the same command.
pub fn apply_language(value: &str) {
    // `auto` means "follow the environment again" — the resolver re-reads
    // `LC_ALL`/`LANG` rather than pinning today's answer.
    let wanted = (value != "auto")
        .then(|| value.parse::<atomcode_config::locale::Locale>().ok())
        .flatten();
    atomcode_config::i18n::set_locale(atomcode_config::i18n::resolve_initial_locale(None, wanted));
}

/// The catalog's kind, as the gesture that edits it.
///
/// Deliberately a translation and not a re-export: the screen must not learn the
/// configuration's schema, or every setting added there would be a change here.
fn kind_of(kind: atomcode_config::settings::SettingKind) -> SettingKind {
    use atomcode_config::settings::SettingKind as Catalog;
    match kind {
        Catalog::Boolean => SettingKind::Boolean,
        Catalog::OptionalBoolean => SettingKind::OptionalBoolean,
        Catalog::Integer { min, max } => SettingKind::Integer { min, max },
        Catalog::Choice(values) => {
            SettingKind::Choice(values.iter().map(|v| v.to_string()).collect())
        }
        Catalog::Text => SettingKind::Text,
    }
}

/// When a change takes effect, in the screen's words for it.
fn applies_of(policy: atomcode_config::settings::ApplyPolicy) -> Applies {
    use atomcode_config::settings::ApplyPolicy as Policy;
    match policy {
        Policy::ImmediateUi => Applies::Immediately,
        Policy::NextTurn => Applies::NextTurn,
        Policy::AgentReassemble => Applies::Reload,
        Policy::CapabilityReprepare => Applies::Reprepare,
        Policy::NextStartup => Applies::Restart,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one the whole thing is for: `/language` moves the **screen**.
    ///
    /// It used to move only the product's table, because the screen had a
    /// table of its own that nothing here could reach — so a person who
    /// switched to English got an English welcome block and a Chinese status
    /// bar. Both tables read one locale now, and this is the call that sets it
    /// (`/language <x>` and the settings panel both land here).
    #[test]
    fn setting_the_language_moves_the_screen_and_not_only_the_product() {
        use atomcode_i18n::screen::{t as tr, Msg as SMsg};
        let _guard = atomcode_config::i18n::test_lock();

        apply_language("zh_CN");
        let zh = (
            tr(SMsg::StatusStopping).into_owned(),
            atomcode_config::i18n::t(atomcode_config::i18n::Msg::ApprovalDeny).into_owned(),
        );

        apply_language("en");
        let en = (
            tr(SMsg::StatusStopping).into_owned(),
            atomcode_config::i18n::t(atomcode_config::i18n::Msg::ApprovalDeny).into_owned(),
        );

        let cjk = |s: &str| s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        assert!(
            cjk(&zh.0) && !cjk(&en.0),
            "the screen did not move: {zh:?} → {en:?}"
        );
        assert!(
            cjk(&zh.1) && !cjk(&en.1),
            "the product did not move: {zh:?} → {en:?}"
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "atomcode-settings-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("config.toml")
    }

    #[test]
    fn every_setting_in_the_catalog_reaches_the_screen() {
        // The property that makes the panel worth having: a setting added to the
        // catalog appears here with no edit, so the two lists cannot drift.
        let path = scratch("all");
        let port = ConfigSettings::new(path);
        let rows = port.rows();
        for spec in atomcode_config::settings::SETTINGS {
            assert!(
                rows.rows().iter().any(|row| row.id == spec.id),
                "`{}` is in the catalog but not on the panel",
                spec.id
            );
        }
        // Every catalog setting and **only** rows this file knows why it added:
        // the count is not `SETTINGS.len()` any more, because one row cannot be
        // in the catalog at all (see `RETRY`). An assertion on the count alone
        // would either forbid that row or say nothing.
        let extra: Vec<&str> = rows
            .rows()
            .iter()
            .map(|row| row.id.as_str())
            .filter(|id| {
                !atomcode_config::settings::SETTINGS
                    .iter()
                    .any(|spec| spec.id == *id)
            })
            .collect();
        assert!(
            extra.iter().all(|id| *id == RETRY),
            "a row from nowhere: {extra:?}"
        );
    }

    /// The one setting the catalog cannot hold: the current model's retries.
    ///
    /// It has no static path — what it writes lives under `[models.<id>]` or
    /// `[providers.<id>]`, and which one that is changes with `/model`. So it
    /// is built from the selection, and the panel must show it, write it and
    /// unset it like any other row.
    ///
    /// **And the file must come back without a credential in it**: the value is
    /// read through `selection_retry_max_attempts` rather than off the provider
    /// table for the same reason `ProviderChoice::about` is built field by
    /// field — a provider entry carries an `api_key`.
    #[test]
    fn the_current_model_s_retries_are_a_row_even_though_the_catalog_cannot_hold_one() {
        let path = scratch("retry");
        std::fs::write(
            &path,
            "default_model = \"glm\"\n\n[providers.glm]\ntype = \"openai_compat\"\nmodel = \"glm-5\"\napi_key = \"sk-SECRET\"\n",
        )
        .unwrap();
        let port = ConfigSettings::new(path.clone());

        let row = port
            .rows()
            .rows()
            .iter()
            .find(|row| row.id == RETRY)
            .cloned()
            .expect("the selection's retries are on the panel");
        assert!(row.label.contains("glm"), "it says which model: {row:?}");
        assert_eq!(row.value, "", "nothing written yet is nothing shown");
        assert!(
            !format!("{row:?}").contains("sk-SECRET"),
            "the key never goes near the screen: {row:?}"
        );

        port.set(RETRY, "7").expect("writing it");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("retry_max_attempts"), "{written}");
        assert_eq!(
            port.rows()
                .rows()
                .iter()
                .find(|row| row.id == RETRY)
                .map(|row| row.value.clone()),
            Some("7".into()),
            "and reading it back says so"
        );

        port.reset(RETRY).expect("unsetting it");
        let unset = std::fs::read_to_string(&path).unwrap();
        assert!(
            !unset.contains("retry_max_attempts"),
            "the key is gone: {unset}"
        );
        assert!(
            unset.contains("sk-SECRET"),
            "and the rest of the person's file is untouched: {unset}"
        );
    }

    /// With nothing selected there is nothing to say.
    #[test]
    fn with_no_model_selected_the_retry_row_is_not_invented() {
        let path = scratch("retry-none");
        std::fs::write(&path, "# 什么也没选\n").unwrap();
        let port = ConfigSettings::new(path);
        assert!(port.rows().rows().iter().all(|row| row.id != RETRY));
    }

    #[test]
    fn a_row_carries_a_label_a_value_and_a_gesture_and_none_of_them_are_blank() {
        let path = scratch("shape");
        let port = ConfigSettings::new(path);
        let rows = port.rows();
        for row in rows.rows() {
            assert!(!row.label.is_empty(), "`{}` has no label", row.id);
            // A value may legitimately be empty (an unset text setting); what
            // must never be empty is the *id*, which is what a write names.
            assert!(!row.id.is_empty());
        }
    }

    #[test]
    fn a_write_lands_in_the_file_and_comes_back_on_the_next_read() {
        let path = scratch("write");
        std::fs::write(&path, "").unwrap();
        let port = ConfigSettings::new(path.clone());
        let before = port.rows();
        let before_value = before
            .rows()
            .iter()
            .find(|r| r.id == "coding.max_rounds")
            .unwrap()
            .value
            .clone();

        let after = port
            .set("coding.max_rounds", "123")
            .expect("a valid integer is taken");
        let now = after
            .rows()
            .iter()
            .find(|r| r.id == "coding.max_rounds")
            .unwrap()
            .value
            .clone();
        assert_eq!(now, "123", "the answer already shows the new value");
        assert_ne!(now, before_value, "and it is not what it was");

        // And it is on disk, not just in the answer: a fresh read agrees.
        let re_read = ConfigSettings::new(path).rows();
        assert_eq!(
            re_read
                .rows()
                .iter()
                .find(|r| r.id == "coding.max_rounds")
                .unwrap()
                .value,
            "123"
        );
    }

    #[test]
    fn a_value_the_catalog_refuses_is_a_refusal_with_the_reason() {
        // The refusal is the catalog's, passed through: the screen has no idea
        // what an integer is allowed to be, and must not guess.
        let path = scratch("refuse");
        std::fs::write(&path, "").unwrap();
        let port = ConfigSettings::new(path);
        let err = port
            .set("coding.max_rounds", "not-a-number")
            .expect_err("a non-number is refused");
        assert!(err.contains("integer") || err.contains("between"), "{err}");
    }

    #[test]
    fn an_id_nothing_knows_is_refused_rather_than_written() {
        // A panel row and a catalog entry are one string; a mismatch is a
        // refusal that says so, not a silent write of something invented.
        let path = scratch("unknown");
        std::fs::write(&path, "").unwrap();
        let port = ConfigSettings::new(path);
        let err = port.set("no.such.setting", "1").expect_err("refused");
        assert!(err.contains("no.such.setting"), "{err}");
    }

    #[test]
    fn writing_preserves_what_the_program_does_not_model() {
        // The reason this patches a document instead of re-serialising `Config`:
        // a person's comments and ordering are theirs, and a settings panel that
        // ate them would be a settings panel nobody uses twice.
        let path = scratch("comments");
        std::fs::write(
            &path,
            "# why this is here\nauto_update = false\n\n# and this\n[ui]\ntheme = \"dark\"\n",
        )
        .unwrap();
        let port = ConfigSettings::new(path.clone());
        port.set("coding.max_rounds", "42").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# why this is here"), "{text}");
        assert!(text.contains("# and this"), "{text}");
        assert!(text.contains("auto_update = false"), "{text}");
        assert!(text.contains("theme = \"dark\""), "{text}");
        assert!(text.contains("max_rounds = 42"), "{text}");
    }

    #[test]
    fn an_unreadable_file_shows_the_builds_defaults_rather_than_refusing() {
        // A person whose config will not parse still has to be able to open the
        // panel and see what the build would do — refusing would leave them with
        // no way to find out what is wrong.
        let path = scratch("broken");
        std::fs::write(&path, "this is not toml {{{").unwrap();
        let port = ConfigSettings::new(path);
        let rows = port.rows();
        assert_eq!(rows.len(), atomcode_config::settings::SETTINGS.len());
    }

    #[test]
    fn every_kind_the_catalog_has_is_a_kind_the_screen_can_edit() {
        use atomcode_config::settings::SettingKind as Catalog;
        // A catalog kind with no gesture would be a row that cannot be changed,
        // drawn as though it could.
        for kind in [
            Catalog::Boolean,
            Catalog::OptionalBoolean,
            Catalog::Integer { min: 0, max: 1 },
            Catalog::Choice(&["a", "b"]),
            Catalog::Text,
        ] {
            let mapped = kind_of(kind);
            assert!(
                mapped.needs_typing() || mapped.cycled("a").is_some(),
                "{kind:?} maps to a kind with no gesture: {mapped:?}"
            );
        }
    }
}
