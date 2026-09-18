//! Library surface for the `atomcode` binary.
//!
//! Exists so integration tests (e.g. `tests/script_parity.rs`) and the binary
//! share testable modules. The bulk of the CLI still lives in `main.rs`; only
//! modules that need to be reachable from `tests/` belong here.

// Redirect ATOMCODE_HOME to a temp dir before this lib crate's tests run, so they
// don't pollute the real ~/.atomcode (mirrors the bin's ctor in main.rs).
#[cfg(test)]
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

#[cfg(unix)]
pub mod askpass;
pub mod uninstall;

/// ACP (Agent Client Protocol) stdio server — lets atomcode be driven by Zed /
/// multi-agent orchestrators over stdin/stdout. Wired up by the `atomcode acp`
/// subcommand in `main.rs`; the engine/dispatch/translate/permission internals
/// live here. Does not depend on `atomcode-core` (v2 stack only).
pub mod acp;

/// Host control for the coding runtime: the adapter behind `HostControl`.
/// Lives here because this binary is the host (`docs/architecture-target.md`
/// §2.4), and a Product crate must not carry a front-end contract.
pub mod host;

/// `atomcode --tui`: the full-screen UI of `atomcode-tui`, in an App of its own,
/// driving the product runtime through the handle protocol and host control.
pub mod tui_front {
    use std::sync::Arc;

    use atomcode_coding::front_end::FrontEnd;

    use crate::host::connect;
    use atomcode_coding::{CodingAgentConfig, CodingRuntime};
    use atomcode_tui::launch::{self, Screen};

    /// The screen, mounted and connected to `runtime` — which was started with
    /// `front_end` in its prepare options — and not yet running.
    pub async fn mount(
        runtime: CodingRuntime,
        front_end: Arc<FrontEnd>,
        config: CodingAgentConfig,
        host_config: Option<Arc<dyn crate::host::HostConfig>>,
        screen: &Screen,
    ) -> Result<launch::Mounted, String> {
        let connection = connect(runtime, front_end, config, host_config)?;
        launch::mount(screen, &[], connection).await
    }

    /// What host control resolves configuration with for `atomcode --tui`: the
    /// config file as it is when asked — so signing in again reads credentials
    /// written since start — and a model resolved the way startup resolves
    /// `--provider` (`docs/adr/0021`, M5.4 addendum).
    pub struct ConfigFile {
        pub path: std::path::PathBuf,
        pub working_dir: std::path::PathBuf,
        pub telemetry: Option<Arc<atomcode_telemetry::Telemetry>>,
        pub skip_permissions: bool,
        /// A launch-time `--provider`, which signing in again keeps.
        pub provider_override: Option<String>,
    }

    /// Which screen this launch opens: what was asked for now, else what the
    /// configuration says, else this build's default
    /// (`docs/tui-replaces-tuix-plan.md` M6.2).
    ///
    /// One place rather than a condition at the launch site, because the two
    /// halves — the flag and the setting — are what makes the default movable:
    /// the setting is a decision a person makes once, the flag is the escape
    /// hatch for one launch, and `Screen::Default` is the only thing that has to
    /// change when the default moves.
    pub fn screen_for(
        asked_for_rows: bool,
        asked_for_classic: bool,
        configured: atomcode_config::config::Screen,
    ) -> atomcode_config::config::Screen {
        use atomcode_config::config::Screen;
        if asked_for_rows {
            return Screen::Rows;
        }
        if asked_for_classic {
            return Screen::Classic;
        }
        match configured {
            // What this build opens when nobody said. It moves once, here.
            Screen::Default => Screen::Classic,
            chosen => chosen,
        }
    }

    impl ConfigFile {
        fn resolve(&self, model: Option<&str>) -> Result<CodingAgentConfig, String> {
            use atomcode_config::config::Config;
            let config = if self.path.exists() {
                Config::load(&self.path).map_err(|e| e.to_string())?
            } else {
                Config::default()
            };
            if let Some(model) = model {
                config
                    .resolve_model(Some(model))
                    .map_err(|_| format!("no model `{model}` is configured"))?;
            }
            let runtime = atomcode_coding::CodingRuntimeConfig::from_config(
                &config,
                &self.working_dir,
                model,
                self.telemetry.clone(),
                self.skip_permissions,
                true,
            );
            // As the interactive runtime's is built at start.
            let mut agent = runtime.agent_config();
            agent.round_cap_checkpoint = true;
            Ok(agent)
        }
    }

    impl crate::host::HostConfig for ConfigFile {
        fn for_model(&self, model: &str) -> Result<CodingAgentConfig, String> {
            self.resolve(Some(model))
        }
        fn current(&self) -> Result<CodingAgentConfig, String> {
            self.resolve(self.provider_override.as_deref())
        }

        /// The settings catalog, with what the file says each is set to.
        ///
        /// The same catalog `/config` in the settings UI reads and the one the
        /// agent is told about, so three readers cannot disagree about what is
        /// editable.
        fn settings(&self) -> Vec<atomcode_host_api::Setting> {
            use atomcode_config::config::Config;
            use atomcode_config::settings::{ApplyPolicy, SettingKind, SETTINGS};
            let config = if self.path.exists() {
                Config::load(&self.path).unwrap_or_default()
            } else {
                Config::default()
            };
            SETTINGS
                .iter()
                .map(|spec| atomcode_host_api::Setting {
                    id: spec.id.to_string(),
                    label: spec.label_zh.to_string(),
                    value: spec.value(&config),
                    accepts: match spec.kind {
                        SettingKind::Boolean => "true | false".into(),
                        SettingKind::OptionalBoolean => "true | false | 不设".into(),
                        SettingKind::Integer { min, max } => format!("{min}–{max}"),
                        SettingKind::Choice(values) => values.join(" | "),
                        SettingKind::Text => String::new(),
                    },
                    applies: match spec.apply {
                        ApplyPolicy::ImmediateUi => "立刻".into(),
                        ApplyPolicy::NextTurn => "下一回合".into(),
                        ApplyPolicy::AgentReassemble => "重装 agent 之后".into(),
                        ApplyPolicy::CapabilityReprepare => "重载能力之后".into(),
                        ApplyPolicy::NextStartup => "下次启动".into(),
                    },
                })
                .collect()
        }

        /// Write one into the file, in place.
        ///
        /// `toml_edit` rather than serialize-the-whole-config: a person's file
        /// has their comments and their ordering in it, and rewriting it whole
        /// would quietly throw both away.
        fn set_setting(&self, id: &str, value: &str) -> Result<(), String> {
            use atomcode_config::settings::SETTINGS;
            let spec = SETTINGS
                .iter()
                .find(|spec| spec.id == id)
                .ok_or_else(|| format!("没有 `{id}` 这一项"))?;
            let text = std::fs::read_to_string(&self.path).unwrap_or_default();
            let mut document: toml_edit::DocumentMut =
                text.parse().map_err(|e| format!("配置文件读不动:{e}"))?;
            spec.patch(&mut document, value)
                .map_err(|e| format!("`{value}` 不合适:{e}"))?;
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(&self.path, document.to_string()).map_err(|e| e.to_string())
        }

        /// The configured providers, as choices — id, kind, model.
        ///
        /// **The key is not read.** A `ProviderConfig` carries an `api_key`, and
        /// this answer is printed on a screen and kept in a log; the only defence
        /// that holds is that the credential never enters the value, which is
        /// why `about` is built field by field rather than from the struct.
        fn providers(&self) -> Vec<atomcode_host_api::ProviderChoice> {
            use atomcode_config::config::Config;
            let Ok(config) = Config::load(&self.path) else {
                return Vec::new();
            };
            let mut out: Vec<atomcode_host_api::ProviderChoice> = config
                .providers
                .iter()
                .map(|(id, p)| atomcode_host_api::ProviderChoice {
                    id: id.clone(),
                    about: format!("{} · {}", p.provider_type, p.model),
                })
                .collect();
            // By name, so the list is the same list every time it is opened —
            // a `HashMap`'s order is not.
            out.sort_by(|a, b| a.id.cmp(&b.id));
            out
        }

        /// Who is signed in, from the stored credentials.
        ///
        /// The name and the email, never the token — this answer is printed on
        /// a screen and kept in a log.
        fn identity(&self) -> Option<crate::host::Identity> {
            let auth = atomcode_auth::get_stored_auth()?;
            Some(crate::host::Identity {
                who: auth.user.name.unwrap_or(auth.user.username),
                detail: auth.user.email,
            })
        }

        /// The file's bytes, hashed. A file that has not been edited reads the
        /// same, and a reload then leaves the running graph where it is.
        ///
        /// The contents rather than the modification time: a config written by
        /// a tool that rewrites the whole file on every save (which is what an
        /// editor does) would otherwise look changed every time it was opened.
        /// No file is its own answer — `default` is a configuration too, and it
        /// is the same one until a file appears.
        fn fingerprint(&self) -> Option<String> {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            match std::fs::read(&self.path) {
                Ok(bytes) => bytes.hash(&mut hasher),
                Err(_) => "no file".hash(&mut hasher),
            }
            // What a model id resolves to also depends on what was asked for at
            // launch, which is not in the file.
            self.provider_override.hash(&mut hasher);
            self.skip_permissions.hash(&mut hasher);
            Some(format!("{:x}", hasher.finish()))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::host::HostConfig;

        const CONFIG: &str = r#"
default_model = "custom/a"

[provider_accounts.custom]
provider = "openai-compatible"
base_url = "https://example.invalid/v1"
api_key = "k"

[models."custom/a"]
account = "custom"
model = "vendor-a"

[models."custom/b"]
account = "custom"
model = "vendor-b"
"#;

        /// The settings a person may change are read from the file and written
        /// back into it — keeping their comments and their ordering
        /// (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` A3).
        ///
        /// Rewriting the file from a serialized config would pass this if it
        /// only checked the value; the comment is what proves it was edited in
        /// place, which is what a person's config file deserves.
        #[test]
        fn a_setting_is_read_from_the_file_and_written_back_into_it() {
            use crate::host::HostConfig;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            std::fs::write(&path, "# 我自己写的注释\n[ui]\ntheme = \"dark\"\n").unwrap();
            let file = ConfigFile {
                path: path.clone(),
                working_dir: dir.path().to_path_buf(),
                telemetry: None,
                skip_permissions: false,
                provider_override: None,
            };

            let listed = file.settings();
            let theme = listed
                .iter()
                .find(|s| s.id == "ui.theme")
                .expect("the theme is a setting a person may change");
            assert_eq!(theme.value, "dark", "read from the file as it is");
            assert!(
                theme.accepts.contains("light"),
                "and says what it accepts: {theme:?}"
            );
            assert!(!theme.applies.is_empty(), "and when it takes effect");

            file.set_setting("ui.theme", "light").unwrap();
            let after = std::fs::read_to_string(&path).unwrap();
            assert!(after.contains("\"light\""), "written: {after}");
            assert!(
                after.contains("# 我自己写的注释"),
                "the person's own file survived the edit: {after}"
            );
            assert_eq!(
                file.settings()
                    .iter()
                    .find(|s| s.id == "ui.theme")
                    .map(|s| s.value.clone()),
                Some("light".into()),
                "and reading it again says the new value"
            );

            // A value the setting does not accept, and an id nobody offers, are
            // both refused rather than written.
            assert!(file.set_setting("ui.theme", "chartreuse").is_err());
            assert!(file.set_setting("no.such.setting", "1").is_err());
            assert!(std::fs::read_to_string(&path)
                .unwrap()
                .contains("\"light\""));
        }

        /// Which screen opens: the flag beats the setting, the setting beats the
        /// build's default, and `--classic` is the escape hatch that keeps
        /// working after the default moves
        /// (`docs/tui-replaces-tuix-plan.md` M6.2).
        #[test]
        fn what_was_asked_for_beats_what_was_configured() {
            use atomcode_config::config::Screen;
            // Nobody said anything: this build's default.
            assert_eq!(
                screen_for(false, false, Screen::Default),
                Screen::Classic,
                "the default this build opens"
            );
            // The setting decides once.
            assert_eq!(screen_for(false, false, Screen::Rows), Screen::Rows);
            assert_eq!(screen_for(false, false, Screen::Classic), Screen::Classic);
            // A flag decides this launch, either way — including against a
            // setting that says the opposite, which is what an escape hatch is.
            assert_eq!(screen_for(true, false, Screen::Classic), Screen::Rows);
            assert_eq!(screen_for(false, true, Screen::Rows), Screen::Classic);
        }

        /// What a reload compares: the same file reads the same, an edited one
        /// reads differently, and so does a launch flag that changes what a
        /// model id resolves to (`docs/adr/0022` §2).
        ///
        /// This is what keeps a reload from rebuilding the running graph when
        /// nothing about the configuration moved — and from *not* rebuilding it
        /// when something did.
        #[test]
        fn the_configuration_reads_the_same_until_it_is_edited() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            let file = |provider_override: Option<&str>| ConfigFile {
                path: path.clone(),
                working_dir: dir.path().to_path_buf(),
                telemetry: None,
                skip_permissions: false,
                provider_override: provider_override.map(str::to_string),
            };
            // No file yet: still an answer, and a stable one.
            let none = file(None).fingerprint();
            assert!(none.is_some());
            assert_eq!(none, file(None).fingerprint());

            std::fs::write(&path, CONFIG).unwrap();
            let written = file(None).fingerprint();
            assert_ne!(written, none, "a file appearing is a configuration change");
            assert_eq!(
                written,
                file(None).fingerprint(),
                "reading it twice is not a change"
            );

            std::fs::write(
                &path,
                CONFIG.replace(
                    "default_model = \"custom/a\"",
                    "default_model = \"custom/b\"",
                ),
            )
            .unwrap();
            assert_ne!(file(None).fingerprint(), written, "an edit is a change");

            // Not everything a model id resolves through is in the file.
            assert_ne!(
                file(Some("custom/b")).fingerprint(),
                file(None).fingerprint(),
                "what was asked for at launch counts too"
            );
        }

        /// A model is resolved from the config file by the id a person picks it
        /// by; one it does not configure is refused; and what signing in reads
        /// is the file as it is now, not as it was at start.
        #[test]
        fn models_and_signing_in_are_resolved_from_the_config_file_as_it_is_now() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            std::fs::write(&path, CONFIG).unwrap();
            let file = ConfigFile {
                path: path.clone(),
                working_dir: dir.path().to_path_buf(),
                telemetry: None,
                skip_permissions: false,
                provider_override: None,
            };
            assert_eq!(file.for_model("custom/b").unwrap().model, "vendor-b");
            assert!(file.for_model("custom/nope").is_err());
            assert_eq!(file.current().unwrap().model, "vendor-a");

            std::fs::write(
                &path,
                CONFIG.replace(
                    "default_model = \"custom/a\"",
                    "default_model = \"custom/b\"",
                ),
            )
            .unwrap();
            assert_eq!(file.current().unwrap().model, "vendor-b");
        }
    }

    /// Run the screen until the person leaves.
    pub async fn run(
        runtime: CodingRuntime,
        front_end: Arc<FrontEnd>,
        config: CodingAgentConfig,
        host_config: Option<Arc<dyn crate::host::HostConfig>>,
        screen: &Screen,
    ) -> Result<(), String> {
        let mounted = mount(runtime, front_end, config, host_config, screen).await?;
        let ctx = mounted.app.context();
        mounted.ui.run(&ctx, None).await
    }
}
