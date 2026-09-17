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

/// `atomcode --tui`: the full-screen UI of `atomcode-tui`, in an App of its own,
/// driving the product runtime through the handle protocol and host control.
pub mod tui_front {
    use std::sync::Arc;

    use atomcode_coding::front_end::{connect, FrontEnd};
    use atomcode_coding::{CodingAgentConfig, CodingRuntime};
    use atomcode_tui::launch::{self, Screen};

    /// The screen, mounted and connected to `runtime` — which was started with
    /// `front_end` in its prepare options — and not yet running.
    pub async fn mount(
        runtime: CodingRuntime,
        front_end: Arc<FrontEnd>,
        config: CodingAgentConfig,
        screen: &Screen,
    ) -> Result<launch::Mounted, String> {
        let connection = connect(runtime, front_end, config)?;
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
    ) -> Result<(), String> {
        let mounted = mount(runtime, front_end, config, screen).await?;
        let ctx = mounted.app.context();
        mounted.ui.run(&ctx, None).await
    }
}
