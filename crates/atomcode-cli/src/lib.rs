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
pub mod tui_settings;
pub mod uninstall;

/// ACP (Agent Client Protocol) stdio server — lets atomcode be driven by Zed /
/// multi-agent orchestrators over stdin/stdout. Wired up by the `atomcode acp`
/// subcommand in `main.rs`; the engine/dispatch/translate/permission internals
/// live here. Does not depend on `atomcode-core` (v2 stack only).
pub mod acp;

/// `atomcode --tui`: the full-screen UI of `atomcode-tui`, in an App of its own,
/// driving the product runtime through the handle protocol and host control.
pub mod tui_front {
    use std::sync::Arc;

    use atomcode_coding::front_end::{connect, FrontEnd};
    use atomcode_coding::{CodingAgentConfig, CodingRuntime};
    use atomcode_tui::launch::{self, Screen};

    /// The settings panel: the row, and the port behind it.
    ///
    /// Both, from one place, because they are one decision: the row exists
    /// because this launcher has settings to show, so a launcher that mounted the
    /// row without the port would be a panel drawing an empty list, and the port
    /// without the row is never reached. `atomcode-tui` names neither — the view
    /// is the screen's, the row is the product's.
    ///
    /// The screen, mounted and connected to `runtime` — which was started with
    /// `front_end` in its prepare options — and not yet running.
    pub async fn mount(
        runtime: CodingRuntime,
        front_end: Arc<FrontEnd>,
        config: CodingAgentConfig,
        screen: &Screen,
        config_path: std::path::PathBuf,
    ) -> Result<launch::Mounted, String> {
        let connection = connect(runtime, front_end, config)?;
        let layer = crate::tui_settings::row_layer();
        launch::mount_with(
            screen,
            &[&layer],
            &[Arc::new(crate::tui_settings::SettingsRow)],
            Some(crate::tui_settings::ConfigSettings::new(config_path)),
            connection,
        )
        .await
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

    impl atomcode_coding::front_end::HostConfig for ConfigFile {
        fn for_model(&self, model: &str) -> Result<CodingAgentConfig, String> {
            self.resolve(Some(model))
        }
        fn current(&self) -> Result<CodingAgentConfig, String> {
            self.resolve(self.provider_override.as_deref())
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
        use atomcode_coding::front_end::HostConfig;

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
        screen: &Screen,
        config_path: std::path::PathBuf,
    ) -> Result<(), String> {
        let mounted = mount(runtime, front_end, config, screen, config_path).await?;
        let ctx = mounted.app.context();
        mounted.ui.run(&ctx, None).await
    }
}
