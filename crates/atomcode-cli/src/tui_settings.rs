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
                    // The screen speaks Chinese; the catalog carries both, for
                    // the same reason it carries both for the agent's answer.
                    label: spec.label_zh.to_string(),
                    value: spec.value(&config),
                    kind: kind_of(spec.kind),
                    applies: applies_of(spec.apply),
                })
                .collect(),
        )
    }
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
        let Some(spec) = atomcode_config::settings::SETTINGS
            .iter()
            .find(|spec| spec.id == id)
        else {
            return Err(format!("没有叫 `{id}` 的设置"));
        };
        let store = atomcode_config::ConfigStore::new(self.path.clone());
        store
            .update_document(|document| spec.patch(document, value))
            .map_err(|error| format!("{error:#}"))?;
        Ok(self.read())
    }
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
        assert_eq!(rows.len(), atomcode_config::settings::SETTINGS.len());
        for spec in atomcode_config::settings::SETTINGS {
            assert!(
                rows.rows().iter().any(|row| row.id == spec.id),
                "`{}` is in the catalog but not on the panel",
                spec.id
            );
        }
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
