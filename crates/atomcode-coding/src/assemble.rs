//! The assembly: wire L1 capabilities into a kernel [`Agent`] per the coding policy.

use crate::config::CodingAgentConfig;
use crate::discipline::VerifyCadenceHook;
use crate::execution_policy::TurnExecutionPolicy;
use crate::persona::coding_persona_with_language;
use atomcode_capabilities::codeintel::{
    codeintel_tool_names, register_codeintel_tools, register_lsp_tool, LspSettings,
};
use atomcode_capabilities::provider::{OpenAiCompatConfig, OpenAiCompatProvider};
use atomcode_capabilities::session::SessionContextHook;
use atomcode_capabilities::tools::{
    coding_tool_names, register_coding_tools_with_vision, ApprovalMiddleware,
    OpenFileWorkspaceGate, RepairToolArgsMiddleware, WriteApprovalGate,
};
use atomcode_kernel::agent::Agent;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{MountedTools, ToolRegistry};
use std::sync::Arc;

/// Assemble a runnable, self-correcting coding agent from `cfg` — the MINIMAL sync
/// path (tools + codeintel only). For the FULL agent (web / skills / mcp / session
/// persistence / memory) use the two-phase [`crate::prepare`] → [`crate::assemble`].
///
/// Wires, all through existing kernel seams (no kernel change):
/// - **provider**: OpenAI-compatible adapter (L1) from the config's creds.
/// - **tools**: the neutral fs/bash toolset + codeintel (L1), all mounted.
/// - **argument repair**: normalize model-produced tool arguments before policy gates.
/// - **approval**: an in-memory [`ApprovalMiddleware`] gate over the arguments that execute.
/// - **persona**: the coding system prompt ([`coding_persona`]).
/// - **discipline**: the [`VerifyCadenceHook`] edit-then-verify loop.
/// - **liveness**: stream + request timeouts from the config (never unbounded).
///
/// Returns `Err` only if the provider fails to construct (e.g. a bad HTTP client config).
pub fn build_coding_agent(cfg: CodingAgentConfig) -> Result<Agent, String> {
    let mut provider_cfg = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
    provider_cfg.context_window = cfg.context_window;
    // Thread the coding layer's liveness knob down to the L1 adapter's byte-idle
    // watchdog. Without this, `OpenAiCompatConfig::new`'s hardcoded 120s default
    // stays in effect even when the user raised `ATOMCODE_STREAM_TIMEOUT_SECS`
    // (or relied on the 300s default documented in `config.rs`). Thinking models
    // (GLM-5.2, DeepSeek V4 Flash, …) go quiet for >2min during hidden reasoning
    // after a large prompt; the 120s ceiling cut them off mid-think and surfaced
    // as a spurious `[Error: stream idle timeout]` even though the connection
    // was healthy. `cfg.stream_timeout` already carries the env-overridable value
    // (default 300s), so propagating it here makes the documented tunable actually
    // govern the L1 watchdog end-to-end.
    provider_cfg.idle_timeout = cfg.stream_timeout;
    // Text-only models must NOT receive image content — a resumed conversation whose
    // history contains an image would otherwise 400 every turn. SAME canonical detector
    // as the tool-mount / read_file vision gate above.
    provider_cfg.supports_vision = cfg.supports_vision;
    let provider = OpenAiCompatProvider::new(provider_cfg)
        .map_err(|e| format!("provider init failed: {}", e.message))?;
    try_build_coding_agent_with(&cfg, Arc::new(provider))
}

/// The SAME coding policy as [`build_coding_agent`] but with a CALLER-SUPPLIED provider
/// (a mock for tests, or any custom [`LlmProvider`]). Use this when you construct the
/// provider yourself; otherwise prefer [`build_coding_agent`].
///
/// This compatibility entry point keeps its historical infallible signature. If optional
/// AtomGit client setup fails, the agent remains usable without those tools and receives an
/// explicit persona warning. New callers that need startup failure propagation should use
/// [`try_build_coding_agent_with`].
pub fn build_coding_agent_with(cfg: &CodingAgentConfig, provider: Arc<dyn LlmProvider>) -> Agent {
    let todo_enabled = crate::persona::todo_switch_enabled_for(cfg.todo.enabled);
    match mount_coding_tools(cfg.supports_vision, todo_enabled, &cfg.lsp) {
        Ok(tools) => build_coding_agent_from_tools(cfg, provider, tools, None),
        Err(_error) => build_coding_agent_from_tools(
            cfg,
            provider,
            mount_base_coding_tools(cfg.supports_vision, todo_enabled, &cfg.lsp),
            Some("AtomGit tools are unavailable because capability setup failed.".to_string()),
        ),
    }
}

/// Fallible variant of [`build_coding_agent_with`] for production callers that require
/// every feature-enabled capability to be present before accepting work.
pub fn try_build_coding_agent_with(
    cfg: &CodingAgentConfig,
    provider: Arc<dyn LlmProvider>,
) -> Result<Agent, String> {
    let tools = mount_coding_tools(
        cfg.supports_vision,
        crate::persona::todo_switch_enabled_for(cfg.todo.enabled),
        &cfg.lsp,
    )?;
    Ok(build_coding_agent_from_tools(cfg, provider, tools, None))
}

fn build_coding_agent_from_tools(
    cfg: &CodingAgentConfig,
    provider: Arc<dyn LlmProvider>,
    tools: MountedTools,
    startup_warning: Option<String>,
) -> Agent {
    let summary_provider = provider.clone(); // tier-2 overflow summary uses the same provider
                                             // Single source of truth for the todo switch (`ATOMCODE_TODO` env overrides the
                                             // default-on config). Used for BOTH the persona usage-guidance section AND the
                                             // TodoHook below, so the system prompt never tells the model to use `todowrite`
                                             // when the tool + hook aren't mounted (and vice-versa). The `todowrite` TOOL
                                             // itself is registered on the same env gate in `atomcode-capabilities`.
    let todo_enabled = crate::persona::todo_switch_enabled_for(cfg.todo.enabled);
    let mut persona = coding_persona_with_language(
        &cfg.model,
        cfg.preferred_language,
        todo_enabled,
        crate::persona::request_user_input_switch_enabled(),
    );
    if let Some(warning) = startup_warning {
        persona.push_str("\n\n<system-reminder>");
        persona.push_str(&warning);
        persona.push_str("</system-reminder>");
    }
    let turn_execution_policy = Arc::new(TurnExecutionPolicy::new());
    let builder = Agent::builder()
        .provider(provider)
        .tools(tools)
        .persona(persona)
        // Repair model-produced arguments before approval inspects them.
        .middleware(Arc::new(RepairToolArgsMiddleware))
        .middleware(turn_execution_policy.clone())
        // Keep hard credential policy ahead of every middleware that may return
        // `Allow` and short-circuit the remaining before-chain.
        .middleware(Arc::new(
            atomcode_capabilities::tools::CredentialBashGate::new(cfg.credential_shell_policy),
        ));
    #[cfg(feature = "atomgit")]
    let builder = builder.middleware(Arc::new(
        atomcode_capabilities::tools::AtomgitBashGate::new(),
    ));
    let mut builder = builder
        // Auto-approve in-workspace open_file (it's Risky → would otherwise prompt on every
        // preview). This path pins an immutable working_dir, so the gate pins the same root.
        // BEFORE approval so its `Allow` short-circuits the prompt.
        .middleware(Arc::new(OpenFileWorkspaceGate::pinned(
            cfg.working_dir.clone(),
        )))
        // Workspace-aware, per-path approval for the file-mutation tools (v1 granularity):
        // in-workspace non-sensitive writes auto-approve, sensitive writes always re-prompt,
        // out-of-workspace writes prompt with a per-path "Always". BEFORE the generic approval
        // gate so its `Allow` short-circuits the prompt. Pins the same immutable root.
        .middleware(Arc::new(WriteApprovalGate::pinned(cfg.working_dir.clone())))
        // Approval runs after all argument rewriting.
        .middleware(Arc::new(ApprovalMiddleware::in_memory()))
        // Env / project-instructions / git context at session start (after persona).
        .hook(Arc::new(SessionContextHook::new(cfg.working_dir.clone())))
        .hook(turn_execution_policy.clone())
        .hook(Arc::new(
            VerifyCadenceHook::with_execution_policy(
                cfg.working_dir.clone(),
                turn_execution_policy,
            )
            // Attended (a present human who reviews edits — CodingAgentConfig::is_attended) →
            // don't FORCE post-edit checks; headless / scheduled keep the cadence.
            // `ATOMCODE_VERIFY` overrides. (Mirrors parts.rs.)
            .attended(cfg.is_attended()),
        ))
        .working_dir(cfg.working_dir.clone())
        // Cache-friendly task-boundary stub + hard-overflow recovery ladder (stub→truncate
        // →drain+LLM-summary). The overflow path is off the normal path (typed error only).
        .compaction(Arc::new(
            atomcode_capabilities::compaction::OverflowCompaction::new(
                atomcode_capabilities::compaction::StubCompaction::default(),
                Some(summary_provider),
            ),
        ))
        .compact_threshold(cfg.compact_threshold)
        .stream_timeout(cfg.stream_timeout)
        .max_continuations(cfg.max_continuations)
        // Ctrl-C semantics: false = UNDO (default), true = PRESERVE the interrupted turn.
        .keep_interrupted_context(cfg.keep_interrupted_context);
    if let Some(policy) = cfg.tool_loop_policy {
        builder = builder.tool_loop_policy(policy);
    }
    // Coarse round-cap backstop: the repetition guards catch exact loops quickly, while this
    // also bounds varying-call runaways. `0` leaves the neutral kernel fuse unwired.
    if cfg.max_rounds != 0 {
        builder = builder.max_rounds(cfg.max_rounds);
    }
    builder = builder.round_cap_checkpoint(cfg.round_cap_checkpoint);
    // Approval liveness: `Some(d)` ⇒ fail-closed after `d` (headless); `None` ⇒ PARK until
    // answered (interactive). Kernel defaults to unbounded when unset, so None = park.
    if let Some(d) = cfg.request_timeout {
        builder = builder.request_timeout(d);
    }
    // Todo-list hook: injects the current todo list as a per-turn <system-reminder> so
    // the model always sees progress even after compaction. Gated on ATOMCODE_TODO env
    // (overrides config); cfg_value=true reflects the default-on config.ui.todo default.
    // CodingAgentConfig doesn't carry ui.todo, so we use the config default (true) here;
    // the env var ATOMCODE_TODO=0 / =false / =off can disable it without a config change.
    if todo_enabled {
        builder = builder.hook(Arc::new(crate::todo::TodoHook));
        builder = builder.hook(Arc::new(crate::todo::TodoEagerHook::new(
            &cfg.model,
            &cfg.provider_type,
            cfg.todo.eager,
        )));
    }
    #[cfg(feature = "atomgit")]
    {
        builder = builder.middleware(Arc::new(
            atomcode_capabilities::tools::GitPushLabelMiddleware::new(cfg.working_dir.clone()),
        ));
    }
    builder.build()
}

/// Register the neutral coding tools + codeintel into a fresh registry and mount the
/// union (everything visible to the model).
fn mount_coding_tools(
    vision: bool,
    todo_enabled: bool,
    lsp: &LspSettings,
) -> Result<MountedTools, String> {
    let (registry, names) = base_coding_tools(vision, todo_enabled, lsp);
    #[cfg(feature = "atomgit")]
    let (registry, names) = {
        let (mut registry, mut names) = (registry, names);
        register_atomgit_capabilities(&mut registry, &mut names)?;
        (registry, names)
    };
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    Ok(registry.mount(&refs))
}

fn mount_base_coding_tools(vision: bool, todo_enabled: bool, lsp: &LspSettings) -> MountedTools {
    let (registry, names) = base_coding_tools(vision, todo_enabled, lsp);
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    registry.mount(&refs)
}

fn base_coding_tools(
    vision: bool,
    todo_enabled: bool,
    lsp: &LspSettings,
) -> (ToolRegistry, Vec<String>) {
    let mut registry = ToolRegistry::new();
    register_coding_tools_with_vision(&mut registry, vision);
    register_codeintel_tools(&mut registry);
    let mut names: Vec<String> = coding_tool_names()
        .iter()
        .filter(|name| todo_enabled || **name != "todowrite")
        .chain(codeintel_tool_names().iter())
        .map(|name| (*name).to_string())
        .collect();
    if register_lsp_tool(&mut registry, lsp) {
        names.push("lsp".into());
    }
    (registry, names)
}

/// Register the shipped AtomGit REST capabilities into a coding tool catalog.
///
/// Both the minimal builder above and the production `parts::prepare → assemble`
/// path use this helper so a feature-enabled build cannot expose different tools
/// depending on which assembly entry point the driver uses.
#[cfg(feature = "atomgit")]
pub(crate) fn register_atomgit_capabilities(
    registry: &mut ToolRegistry,
    names: &mut Vec<String>,
) -> Result<(), String> {
    use atomcode_capabilities::tools::{
        atomgit_tool_names, register_atomgit_tools, AtomgitClient, AtomgitConfig, LiveTokenProvider,
    };

    let client = AtomgitClient::new(AtomgitConfig {
        base_url: "https://api.atomgit.com/api/v5".to_string(),
        user_agent: format!("atomcode/{}", env!("CARGO_PKG_VERSION")),
        token: Arc::new(LiveTokenProvider),
    })?;
    register_atomgit_tools(registry, Arc::new(client));
    names.extend(atomgit_tool_names().iter().map(|name| (*name).to_string()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::mount_base_coding_tools;
    use atomcode_capabilities::codeintel::{LspServerSetting, LspSettings};

    #[test]
    fn disabled_todo_is_not_exposed_to_the_model() {
        let names: Vec<String> = mount_base_coding_tools(false, false, &Default::default())
            .defs()
            .into_iter()
            .map(|def| def.name)
            .collect();
        assert!(!names.iter().any(|name| name == "todowrite"));
    }

    #[test]
    fn lsp_is_mounted_only_when_runtime_policy_enables_it() {
        let disabled: Vec<_> = mount_base_coding_tools(false, true, &LspSettings::default())
            .defs()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert!(!disabled.iter().any(|name| name == "lsp"));

        let mut enabled = LspSettings {
            enabled: true,
            auto_detect: false,
            ..Default::default()
        };
        enabled.servers.insert(
            "rs".into(),
            LspServerSetting {
                command: "rust-analyzer".into(),
                args: Vec::new(),
                root_markers: vec!["Cargo.toml".into()],
            },
        );
        let mounted: Vec<_> = mount_base_coding_tools(false, true, &enabled)
            .defs()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert!(mounted.iter().any(|name| name == "lsp"));
        assert!(!mounted.iter().any(|name| name == "diagnostics"));
    }
}
