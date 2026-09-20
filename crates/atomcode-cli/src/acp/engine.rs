//! Kernel-native session agent for ACP sessions.
//!
//! Builds native provider + runtime configuration without depending on
//! `atomcode-core`. The single entry point [`spawn_session`]
//! runs the two-phase `prepare → assemble → spawn` pipeline and hands back a live
//! [`CodingRuntime`] the session table can drive.

use std::path::PathBuf;
use std::sync::Arc;

use atomcode_capabilities::mcp::McpServerConfig;
use atomcode_coding::config::CodingAgentConfig;
use atomcode_coding::parts::PrepareOptions;
use atomcode_coding::{
    CodingProviderFactory, CodingRuntime, CodingRuntimeStart, DefaultCodingProviderFactory,
    RuntimeStartError, SessionMode, StaticPluginHookSource,
};

/// Complete agent configuration template for ACP sessions.
///
/// Constructed by the session dispatcher from the ACP `initialize` handshake and
/// the global provider configuration.
#[derive(Clone)]
pub struct EngineConfig {
    config: CodingAgentConfig,
    /// How a model id becomes a configuration, when this server was given a way
    /// to do it.
    ///
    /// Here rather than threaded through the session handlers because it is a
    /// property of the server, not of one session: every session this engine
    /// spawns resolves models the same way. It reaches the contract as the
    /// host's `for_model` (`sessions::AcpHost`), which is what
    /// `HostCommand::SwitchModel` asks of a host.
    resolve_model: Option<Arc<crate::acp::SessionModelResolver>>,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("config", &self.config)
            .field("resolve_model", &self.resolve_model.is_some())
            .finish()
    }
}

impl EngineConfig {
    pub fn from_coding_config(config: CodingAgentConfig) -> Self {
        Self {
            config,
            resolve_model: None,
        }
    }

    /// Give this engine a way to turn a model id into a configuration.
    pub fn with_model_resolver(
        mut self,
        resolve: Option<Arc<crate::acp::SessionModelResolver>>,
    ) -> Self {
        self.resolve_model = resolve;
        self
    }

    /// What a session registered from this engine hands the host.
    pub fn model_resolver(&self) -> Option<Arc<crate::acp::SessionModelResolver>> {
        self.resolve_model.clone()
    }

    /// Build the `CodingAgentConfig` for this session's working directory.
    ///
    /// `request_timeout` is cleared (`None`) so approval prompts park until the
    /// ACP client answers — the interactive contract, not the headless fail-closed one.
    pub fn to_coding_config(&self, cwd: PathBuf) -> CodingAgentConfig {
        let mut cfg = self.config.clone();
        cfg.working_dir = cwd;
        // ACP sessions are long-lived and interactive: park on approval, not fail-closed, and a
        // human in the editor reviews edits — so mark them attended (mirrors the request_timeout
        // clear above; keeps the two intents in sync for the verify-cadence gate).
        cfg.request_timeout = None;
        cfg.interactive = true;
        cfg
    }
}

/// Spawn a kernel-native agent for an ACP session.
///
/// Runs the two-phase `prepare → assemble → spawn` pipeline and returns a live
/// [`CodingRuntime`] the session dispatcher can drive. `session` selects a
/// fresh session (`SessionMode::Fresh`, the `session/new` path) or a resume of
/// an existing native session (`SessionMode::Resume(native_id)`, the
/// `session/resume` path) — the coding runtime owns lease acquisition, native
/// aggregate loading, and snapshot version checks, failing closed on any
/// problem (missing session, `SessionInUse`, corrupt snapshot).
///
/// `extra_mcp_servers` are client-injected ACP `mcpServers` (stdio), connected
/// alongside the config-derived catalog; they carry `McpConfigSource::Driver`
/// and are not project-trust gated.
/// The front end comes back with the runtime because it has to be handed in
/// **before** the runtime is built: `front-end-feed` is a row, mounted only
/// where a front end exists (`coding/src/on_harness.rs`), and it is that row
/// which fills `FrontEnd::app`. A front end made afterwards — which is what
/// this channel used to do — connects to the runtime's event stream and gets
/// turn events, but its `app` is never filled, so every `Subscribe` is refused
/// and the agent's description never arrives. That is why this channel knew
/// nothing about the commands its own agent registered.
pub async fn spawn_session(
    engine: &EngineConfig,
    cwd: PathBuf,
    provider_factory: Option<Arc<dyn CodingProviderFactory>>,
    extra_mcp_servers: Vec<McpServerConfig>,
    session: SessionMode,
) -> Result<(CodingRuntime, Arc<atomcode_coding::front_end::FrontEnd>), RuntimeStartError> {
    let cfg = engine.to_coding_config(cwd);
    let provider_factory = provider_factory.unwrap_or_else(|| {
        Arc::new(DefaultCodingProviderFactory::new(concat!(
            "atomcode/",
            env!("CARGO_PKG_VERSION")
        )))
    });
    let front_end = atomcode_coding::front_end::FrontEnd::new();
    let runtime = CodingRuntime::start(CodingRuntimeStart {
        agent: cfg,
        prepare: PrepareOptions {
            session,
            front_end: Some(front_end.clone()),
            tools: true,
            subagents: atomcode_coding::SubagentPolicy::Enabled,
            // SDK 2.0.0 的 stable v1 已支持通用 elicitation(表单/URL)。ACP 端通过
            // `elicitation/create`(form) 回环把 `request_user_input` 工具的
            // 结构化提问呈现给客户端,见 `crate::acp::elicitation`。
            request_user_input: true,
            extra_mcp_servers,
            ..PrepareOptions::default()
        },
        provider_factory,
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    })
    .await?;
    Ok((runtime, front_end))
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

        let first = spawn_session(
            &engine,
            cwd.path().to_path_buf(),
            Some(factory.clone()),
            Vec::new(),
            SessionMode::Fresh,
        )
        .await
        .unwrap();
        let second = spawn_session(
            &engine,
            cwd.path().to_path_buf(),
            Some(factory.clone()),
            Vec::new(),
            SessionMode::Fresh,
        )
        .await
        .unwrap();

        let ids = factory.session_ids.lock().unwrap().clone();
        assert_eq!(ids.len(), 2);
        assert!(ids[0].is_some());
        assert!(ids[1].is_some());
        assert_ne!(ids[0], ids[1]);

        first.0.handle.shutdown().await.unwrap();
        second.0.handle.shutdown().await.unwrap();
    }
}
