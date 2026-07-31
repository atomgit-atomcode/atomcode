//! Kernel-native session agent for ACP sessions.
//!
//! Builds native provider + runtime configuration without depending on
//! `atomcode-core`. The single entry point [`spawn_session`]
//! runs the two-phase `prepare → assemble → spawn` pipeline and hands back a live
//! [`CodingRuntime`] the session table can drive.

use std::path::PathBuf;
use std::sync::Arc;

use atomcode_coding::config::CodingAgentConfig;
use atomcode_coding::parts::PrepareOptions;
use atomcode_coding::{
    CodingProviderFactory, CodingRuntime, CodingRuntimeStart, DefaultCodingProviderFactory,
    StaticPluginHookSource,
};

/// Complete agent configuration template for ACP sessions.
///
/// Constructed by the session dispatcher from the ACP `initialize` handshake and
/// the global provider configuration.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    config: CodingAgentConfig,
}

impl EngineConfig {
    pub fn from_coding_config(config: CodingAgentConfig) -> Self {
        Self { config }
    }

    /// Build the `CodingAgentConfig` for this session's working directory.
    ///
    /// `request_timeout` is cleared (`None`) so approval prompts park until the
    /// ACP client answers — the interactive contract, not the headless fail-closed one.
    pub fn to_coding_config(&self, cwd: PathBuf) -> CodingAgentConfig {
        let mut cfg = self.config.clone();
        cfg.working_dir = cwd;
        // ACP sessions are long-lived and interactive: park on approval, not fail-closed.
        cfg.request_timeout = None;
        cfg
    }
}

/// Spawn a kernel-native agent for a new ACP session.
///
/// Runs the two-phase `prepare → assemble → spawn` pipeline and returns a live
/// [`CodingRuntime`] the session dispatcher can drive.
///
pub async fn spawn_session(
    engine: &EngineConfig,
    cwd: PathBuf,
    provider_factory: Option<Arc<dyn CodingProviderFactory>>,
) -> anyhow::Result<CodingRuntime> {
    let cfg = engine.to_coding_config(cwd);
    let provider_factory = provider_factory.unwrap_or_else(|| {
        Arc::new(DefaultCodingProviderFactory::new(concat!(
            "atomcode/",
            env!("CARGO_PKG_VERSION")
        )))
    });
    CodingRuntime::start(CodingRuntimeStart {
        agent: cfg,
        prepare: PrepareOptions::default(),
        provider_factory,
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    })
    .await
    .map_err(|e| anyhow::anyhow!("acp runtime start failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingProviderFactory {
        session_ids: std::sync::Mutex<Vec<Option<String>>>,
    }

    impl CodingProviderFactory for RecordingProviderFactory {
        fn build(
            &self,
            _config: &CodingAgentConfig,
            session_id: Option<&str>,
        ) -> Result<
            Arc<dyn atomcode_kernel::provider::LlmProvider>,
            atomcode_coding::ProviderBuildError,
        > {
            self.session_ids
                .lock()
                .unwrap()
                .push(session_id.map(str::to_owned));
            Ok(Arc::new(atomcode_kernel::testkit::MockProvider::new(
                Vec::new(),
            )))
        }
    }

    #[test]
    fn engine_config_builds_coding_config() {
        let mut base = CodingAgentConfig::new("k", "https://x", "m", "/original");
        base.context_window = 200_000;
        base.chat_options.max_tokens = Some(8192);
        base.provider_type = "openai".into();
        let e = EngineConfig::from_coding_config(base);
        let cfg = e.to_coding_config(std::path::PathBuf::from("/tmp/work"));
        assert_eq!(cfg.model, "m");
        assert_eq!(cfg.context_window, 200_000);
        assert_eq!(cfg.provider_type, "openai");
        assert_eq!(cfg.working_dir, std::path::PathBuf::from("/tmp/work"));
        assert_eq!(cfg.chat_options.max_tokens, Some(8192));
    }

    #[test]
    fn engine_config_preserves_provider_and_runtime_semantics() {
        let mut original = CodingAgentConfig::new(
            "k",
            "https://internal.example/v1",
            "reasoning-model",
            "/original",
        );
        original.provider_type = "anthropic".into();
        original.skip_tls_verify = true;
        original.user_agent = Some("custom-agent/1".into());
        original.reasoning_history = Some("preserve".into());
        original.chat_options.reasoning_effort =
            Some(atomcode_kernel::provider::ReasoningEffort::High);
        original.thinking_enabled = Some(true);
        original.thinking_type = Some("enabled".into());
        original.thinking_keep = Some("all".into());
        original.keep_interrupted_context = true;
        original.loop_max_rounds = 41;

        let cfg = EngineConfig::from_coding_config(original)
            .to_coding_config(std::path::PathBuf::from("/session"));

        assert!(cfg.skip_tls_verify);
        assert_eq!(cfg.user_agent.as_deref(), Some("custom-agent/1"));
        assert_eq!(cfg.reasoning_history.as_deref(), Some("preserve"));
        assert_eq!(
            cfg.chat_options.reasoning_effort,
            Some(atomcode_kernel::provider::ReasoningEffort::High)
        );
        assert_eq!(cfg.thinking_enabled, Some(true));
        assert_eq!(cfg.thinking_type.as_deref(), Some("enabled"));
        assert_eq!(cfg.thinking_keep.as_deref(), Some("all"));
        assert!(cfg.keep_interrupted_context);
        assert_eq!(cfg.loop_max_rounds, 41);
        assert_eq!(cfg.working_dir, std::path::PathBuf::from("/session"));
        assert_eq!(cfg.request_timeout, None);
    }

    #[tokio::test]
    async fn shared_factory_builds_each_session_with_its_own_identity() {
        let mut base = CodingAgentConfig::new("k", "https://example.test/v1", "m", "/original");
        base.context_window = 200_000;
        base.chat_options.max_tokens = Some(8192);
        let engine = EngineConfig::from_coding_config(base);
        let cwd = tempfile::tempdir().unwrap();
        let factory = Arc::new(RecordingProviderFactory::default());

        let first = spawn_session(&engine, cwd.path().to_path_buf(), Some(factory.clone()))
            .await
            .unwrap();
        let second = spawn_session(&engine, cwd.path().to_path_buf(), Some(factory.clone()))
            .await
            .unwrap();

        let ids = factory.session_ids.lock().unwrap().clone();
        assert_eq!(ids.len(), 2);
        assert!(ids[0].is_some());
        assert!(ids[1].is_some());
        assert_ne!(ids[0], ids[1]);

        first.handle.shutdown().await.unwrap();
        second.handle.shutdown().await.unwrap();
    }
}
