//! The two-phase FULL assembly: `prepare` (async, does I/O: MCP background-start,
//! skill loading, session binding) → `assemble` (pure composition, no I/O).
//!
//! WHY two phases (pre-C1 design review, all four confirmed findings):
//! - **sync/async**: MCP connection is supplemental readiness and must not block a
//!   session transition. `prepare` starts it; an updatable MountedTools publishes
//!   discovered tools atomically for the next turn.
//! - **session_id 单一 owner**: the binding is allocated ONCE here and fanned out to
//!   the builder + every session hook — no driver hand-threading, no divergence.
//! - **状态句柄外露**: `CodingParts` keeps `Arc`s to the approval middleware (grant
//!   store) and hooks, so a RESPAWN (B2 model swap → `assemble` again on the SAME
//!   parts) preserves every allow-always grant and all hook state.
//! - **config 不膨胀**: capability inputs live in [`PrepareOptions`], not in
//!   [`CodingAgentConfig`].
//!
//! A `/mcp reload` rebuilds parts (`prepare` again — reconnect is the point) and
//! respawns; a model swap reuses the SAME parts with a new provider.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use atomcode_capabilities::cc_hooks::{CCExternalHooks, HookConfig};
use atomcode_capabilities::codeintel::register_codeintel_tools;
use atomcode_capabilities::datalog::DatalogHook;
use atomcode_capabilities::mcp::{self, McpConnectEvent, McpRegistry, McpServerConfig};
use atomcode_capabilities::memory::MemoryHook;
use atomcode_capabilities::session::snapshot::SnapshotPersistenceStatus;
use atomcode_capabilities::session::{
    PresentationFile, RecallTool, SessionContextHook, SessionLease, SessionManager, SessionMeta,
    SnapshotHook, StatusReminderHook, StorageOwner, TranscriptHook,
};
use atomcode_capabilities::skills::{
    register_skill_tools, runtime_skill_dirs, SkillCatalogHook, SkillRegistry,
};
use atomcode_capabilities::tools::{
    register_coding_tools_with_vision, ApprovalMiddleware, ArtifactMiddleware, ArtifactStore,
    BashWorkspaceGate, FetchOutputTool, OpenFileWorkspaceGate, ReadFileTool,
    RepairToolArgsMiddleware, SensitivePathGate, WebFetchTool, WebSearchTool, WriteApprovalGate,
};
use atomcode_kernel::agent::Agent;
use atomcode_kernel::checkpoint::CompactionCheckpoint;
use atomcode_kernel::hook::LifecycleHooks;
use atomcode_kernel::message::{Message, Role, SessionSnapshot};
use atomcode_kernel::middleware::ToolMiddleware;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{MountedTools, MountedToolsPublisher, ToolRegistry};
use atomcode_review::{ReviewTool, ReviewToolConfig, SharedReviewProvider};

use crate::config::CodingAgentConfig;
use crate::discipline::VerifyCadenceHook;
use crate::execution_policy::TurnExecutionPolicy;
use crate::mcp_instructions::McpInstructionsHook;
#[cfg(test)]
use crate::persona::coding_persona;
use crate::persona::coding_persona_with_capabilities;
use crate::plugin_hooks::PluginHookSource;
use crate::rate_limit::RateLimitWindowSource;

/// How `prepare` binds the agent to on-disk session persistence.
#[derive(Clone, Debug, Default)]
pub enum SessionMode {
    /// Allocate a fresh session id (uuid v4) and persist from turn 1.
    #[default]
    Fresh,
    /// Resume the given session id from its complete native aggregate. `prepare`
    /// errors unless metadata, snapshot, and presentation are all present and the
    /// metadata owner is `Native`; compatibility callers must import first.
    Resume(String),
    /// Bind an externally-loaded snapshot after proving it exactly matches the
    /// complete native aggregate. Compatibility drivers must import first; this
    /// variant cannot create or repair persistent session state.
    ExternalSnapshot {
        id: String,
        snapshot: SessionSnapshot,
    },
    /// No persistence (CI / one-shot / review-style runs).
    Disabled,
}

/// Driver-owned policy for mounting child-agent capabilities. The library default
/// is fail-closed; interactive drivers must opt in explicitly.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SubagentPolicy {
    #[default]
    Disabled,
    Enabled,
}

impl SubagentPolicy {
    fn resolve(self, env: Option<&str>) -> bool {
        if self == Self::Disabled {
            return false;
        }
        env.is_none_or(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "off" | "no"
            )
        })
    }
}

/// Capability inputs for [`prepare`] — what to wire beyond the always-on core
/// (fs/bash tools + codeintel). Defaults = the full production-parity agent.
#[derive(Clone, Debug)]
pub struct PrepareOptions {
    pub session: SessionMode,
    /// Skill dirs in LOW→HIGH priority order; `None` = the standard home+project
    /// precedence ([`standard_skill_dirs`]).
    pub skill_dirs: Option<Vec<PathBuf>>,
    /// Plugin-contributed skill directories, each paired with its namespace
    /// (the plugin manifest's `name`). Loaded AFTER `skill_dirs` so plugin
    /// skills are registered as `<namespace>:<skill-name>` — same convention
    /// the slash menu uses. Empty = no plugin skills. The registry remains
    /// source-neutral; the driver discovers installed-plugin directories and
    /// feeds them in.
    pub plugin_skill_dirs: Vec<(PathBuf, String)>,
    /// Connect MCP servers from `<working_dir>/.mcp.json` (+ global config).
    pub mcp: bool,
    /// Driver-supplied MCP servers connected alongside config servers — e.g.
    /// ACP client-injected `mcpServers` from `session/new`. Entries must use
    /// `McpConfigSource::Driver`: they bypass the project trust gate because
    /// the injecting driver is the trust boundary. Ignored when `mcp` is
    /// false (no registry is created).
    pub extra_mcp_servers: Vec<McpServerConfig>,
    /// External-agent subagent instances (`[[subagent.external]]` profiles) to
    /// mount as named `subagent_<name>` tools. Each drives Claude Code / Codex as
    /// a subagent. A profile whose binary is missing on PATH is skipped. Empty =
    /// none. Independent of `subagents` (which gates the in-process `task`/`team`).
    pub external_subagents: Vec<atomcode_capabilities::subagent::ExternalSubagentProfile>,
    /// Inject `memory.md` (global + project) at session start. KEEP THIS CONSISTENT
    /// across resumes of one session: the injected block is persisted in the
    /// snapshot, and only a registered MemoryHook reconciles/removes it on resume —
    /// resuming a memory-bearing session with `memory: false` leaves the stale
    /// block frozen in the prefix.
    pub memory: bool,
    /// Mount `web_fetch` / `web_search`.
    pub web: bool,
    /// Mount the `code_review` sub-agent tool (lets the agent review the current changes
    /// in-session via the review specialization). Reuses the host provider (set at
    /// assemble), so it works on a signing gateway. `false` ⇒ not mounted.
    pub review: bool,
    /// Mount `task` and `team`. Drivers must opt in explicitly; the environment
    /// may still disable an enabled driver with `ATOMCODE_SUBAGENT=0`.
    pub subagents: SubagentPolicy,
    /// Mount the structured `request_user_input` tool. Drivers without a typed
    /// request/response protocol must set this false instead of advertising a tool
    /// they can only fail with `Null`.
    pub request_user_input: bool,
    /// Provider-specific quota source supplied by the host. `None` keeps 429 handling generic.
    pub rate_limit_source: Option<Arc<dyn RateLimitWindowSource>>,
}

impl Default for PrepareOptions {
    fn default() -> Self {
        Self {
            session: SessionMode::Fresh,
            skill_dirs: None,
            plugin_skill_dirs: Vec::new(),
            mcp: true,
            extra_mcp_servers: Vec::new(),
            external_subagents: Vec::new(),
            memory: true,
            web: true,
            review: true,
            subagents: SubagentPolicy::Disabled,
            request_user_input: true,
            rate_limit_source: None,
        }
    }
}

/// Convert `[[subagent.external]]` config entries into resolved
/// [`ExternalSubagentProfile`]s for [`PrepareOptions::external_subagents`].
///
/// Disabled entries and entries with an unknown `kind` are skipped (with a
/// warning). An unknown `permission` falls back to `read-only`. The dangerous
/// `bypass` mode survives ONLY when the profile opts in (`allow_dangerous`) AND
/// the caller's context permits it (`allow_dangerous_context`, false for
/// non-interactive/headless/scheduled runs); otherwise it is downgraded to
/// `read-only` — the fail-closed default.
pub fn external_subagent_profiles(
    configs: &[atomcode_config::config::ExternalSubagentConfig],
    allow_dangerous_context: bool,
) -> Vec<atomcode_capabilities::subagent::ExternalSubagentProfile> {
    use atomcode_capabilities::subagent::{ExternalSubagentProfile, PermissionMode, SubagentKind};
    let mut out = Vec::new();
    for c in configs {
        if !c.enabled {
            continue;
        }
        let Some(kind) = SubagentKind::from_config_str(&c.kind) else {
            eprintln!(
                "subagent: skipping external agent `{}` — unknown kind `{}`",
                c.name, c.kind
            );
            continue;
        };
        let mut permission = match &c.permission {
            Some(p) => PermissionMode::from_config_str(p).unwrap_or_else(|| {
                eprintln!(
                    "subagent: `{}` has unknown permission `{p}`; using read-only",
                    c.name
                );
                PermissionMode::ReadOnly
            }),
            None => PermissionMode::ReadOnly,
        };
        // Bypass double-guard: honored only if the profile opts in AND the
        // context permits. Otherwise downgrade to the fail-closed default.
        let allow_dangerous = c.allow_dangerous && allow_dangerous_context;
        if permission.is_dangerous() && !allow_dangerous {
            eprintln!(
                "subagent: `{}` requested bypass without allowance in this context; \
                 downgrading to read-only",
                c.name
            );
            permission = PermissionMode::ReadOnly;
        }
        out.push(ExternalSubagentProfile {
            name: c.name.clone(),
            kind,
            model: c.model.clone(),
            permission,
            allow_dangerous,
            timeout: c.timeout_secs.map(std::time::Duration::from_secs),
        });
    }
    out
}

/// Resolve ALL external-agent subagent profiles for a `[subagent]` config: the
/// `codex`/`claude` convenience switches (`/config`-editable) plus any explicit
/// `[[subagent.external]]` entries. Explicit entries win on a name clash (the
/// built-in `codex` / `claude-code` names are only added if not already taken),
/// so an advanced user can override a switch with a full profile.
pub fn resolve_external_subagents(
    sub: &atomcode_config::config::SubAgentConfig,
    allow_dangerous_context: bool,
) -> Vec<atomcode_capabilities::subagent::ExternalSubagentProfile> {
    let mut out = external_subagent_profiles(&sub.external, allow_dangerous_context);
    // Reserve EVERY explicitly-named instance — including entries that were
    // dropped for being disabled or having an unknown kind — so a `/config`
    // convenience switch never silently overrides (or resurrects) an explicit
    // `[[subagent.external]]` the user named the same thing.
    let mut names: std::collections::HashSet<String> =
        sub.external.iter().map(|e| e.name.clone()).collect();
    let builtins = [
        (
            "codex",
            atomcode_capabilities::subagent::SubagentKind::Codex,
            sub.codex.as_str(),
        ),
        (
            "claude-code",
            atomcode_capabilities::subagent::SubagentKind::ClaudeCode,
            sub.claude.as_str(),
        ),
    ];
    for (name, kind, level) in builtins {
        if let Some(profile) = builtin_external_profile(name, kind, level) {
            if names.insert(profile.name.clone()) {
                out.push(profile);
            }
        }
    }
    out
}

/// Build a built-in convenience profile from a `/config` level string. `off` (or
/// any unrecognized value) yields `None`. Built-ins are always fail-closed
/// (never `allow_dangerous`); `bypass` is intentionally not offered here.
fn builtin_external_profile(
    name: &str,
    kind: atomcode_capabilities::subagent::SubagentKind,
    level: &str,
) -> Option<atomcode_capabilities::subagent::ExternalSubagentProfile> {
    use atomcode_capabilities::subagent::{ExternalSubagentProfile, PermissionMode};
    // Reuse the single permission parser (normalizes case + `_`→`-`, so
    // `Read-Only`/`readonly`/`accept_edits` all work like the explicit path).
    // `off`/`""`/unknown → None; `bypass` is never offered via the switch.
    let permission =
        PermissionMode::from_config_str(level).filter(|p| !p.is_dangerous())?;
    Some(ExternalSubagentProfile {
        name: name.to_string(),
        kind,
        model: None,
        permission,
        allow_dangerous: false,
        timeout: None,
    })
}

/// The session identity + persistence wiring, allocated ONCE by [`prepare`] —
/// the single owner the design review asked for.
pub struct SessionBinding {
    pub id: String,
    pub manager: Arc<SessionManager>,
    /// Active-runtime ownership. Clones share the same OS lock and release it
    /// only when the last runtime generation drops.
    pub(crate) lease: SessionLease,
    /// Canonical snapshot on resume/external binding; `None` on fresh.
    pub resume: Option<SessionSnapshot>,
    /// Accepted user input recovered from an interrupted turn. It is replayed
    /// through the normal runtime submit path after an agent becomes available.
    pub(crate) pending_resume_prompt: Option<Message>,
    /// Fresh metadata prepared in memory but not yet catalog-visible. CodingRuntime
    /// publishes it only after the complete candidate graph has assembled.
    staged_fresh: Option<SessionMeta>,
}

struct McpWorkGuard {
    registry: Option<Arc<McpRegistry>>,
    publication_enabled: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for McpWorkGuard {
    fn drop(&mut self) {
        self.publication_enabled
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(registry) = &self.registry {
            registry.cancel_pending_work();
        }
    }
}

/// Everything `assemble` composes — and everything a respawn must REUSE so state
/// survives (approval grants, hook state, session identity).
pub struct CodingParts {
    registry: ToolRegistry,
    tool_names: Vec<String>,
    /// Capability-graph decision made at prepare time. Like request-user-input,
    /// changing the master switch requires a capability reprepare; provider-only
    /// reassembly must not advertise a tool absent from the mounted catalog.
    todo_enabled: bool,
    request_user_input_enabled: bool,
    /// At least one external-agent subagent tool (`subagent_<name>`) is mounted;
    /// drives the persona's external-delegation guidance (kept in sync on model
    /// swap via `reconcile_coding_persona`).
    has_external_subagents: bool,
    mcp_tool_names: Arc<std::sync::RwLock<Vec<String>>>,
    mounted_tools: Option<MountedTools>,
    mounted_tools_publisher: Option<MountedToolsPublisher>,
    mcp_connect_rx: Option<tokio::sync::mpsc::UnboundedReceiver<McpConnectEvent>>,
    mcp_publish_lock: Arc<tokio::sync::Mutex<()>>,
    mcp_publication_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// True only after the publisher has reconciled every initial connection into
    /// the mounted kernel catalog. This is distinct from transport readiness.
    mcp_catalog_ready: tokio::sync::watch::Sender<bool>,
    _mcp_work_guard: McpWorkGuard,
    /// The approval gate, handle EXPOSED: respawning on the same parts keeps every
    /// allow-always grant (the in_memory-buried-in-the-assembly bug from the review).
    pub approval: Arc<ApprovalMiddleware>,
    /// Lifecycle hooks in the CANONICAL ORDER (see [`assemble`]).
    hooks: Vec<Arc<dyn LifecycleHooks>>,
    /// The same session snapshot writer used by `SnapshotHook`, exposed through
    /// the kernel's manual-compaction checkpoint seam.
    compaction_checkpoint: Option<Arc<dyn CompactionCheckpoint>>,
    /// Concrete handle retained so provider-only reassembly can update the
    /// per-turn cost attribution without rebuilding session-owned hooks.
    snapshot_hook: Option<Arc<SnapshotHook>>,
    snapshot_persistence_status: Option<SnapshotPersistenceStatus>,
    pub session: Option<SessionBinding>,
    /// Runtime-owned resume for sessionless drivers during an in-process reassembly.
    /// Persistent sessions reload their canonical snapshot through `SessionBinding` instead.
    runtime_resume: Option<SessionSnapshot>,
    /// Connected MCP servers (None when `opts.mcp` was false or no config exists).
    pub mcp_registry: Option<Arc<McpRegistry>>,
    /// The agent's tool working dir as a LIVE handle (kernel Seam 1b): the driver
    /// mutates it to implement `/cd` — tools resolve against the new dir from the
    /// next call. Session/memory/recall stay anchored to the PREPARE-time project
    /// root by design (the per-project stores don't follow a mid-session cd).
    pub shared_cwd: std::sync::Arc<std::sync::RwLock<std::path::PathBuf>>,
    /// Plan-mode toggle (read-only exploration). The runtime flips it on `SetMode`;
    /// the [`PlanModeGate`](crate::PlanModeGate) middleware reads it to
    /// block mutating tools. Shared (not rebuilt) so a respawn preserves the mode.
    pub plan_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Runtime auto-approve (bypass) flag. `SetMode(Auto)` sets it; the runtime
    /// approval seam auto-allows while set. Mirrors `plan_mode`.
    pub bypass_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Auto-accept-edits flag. `SetMode(AcceptEdits)` sets it; the
    /// [`WriteApprovalGate`](crate::tools) reads it to auto-approve NON-sensitive
    /// file edits without a prompt (bash + sensitive paths still prompt). Unlike
    /// `bypass_mode` this is enforced in middleware, mirroring `plan_mode`. Shared
    /// (not rebuilt) so a respawn preserves the mode.
    pub accept_edits: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Current real-user turn's explicit execution restriction. The same instance is
    /// a lifecycle hook and a pre-approval Bash middleware.
    pub(crate) turn_execution_policy: Arc<TurnExecutionPolicy>,
    /// Session grant store for mutating MCP tools the user approved "always" while in
    /// PLAN mode. Owned here (not rebuilt in [`assemble`]) so a respawn / model-swap
    /// preserves the grants — the same reason the mode flags above are shared.
    pub mcp_plan_grants: std::sync::Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    pub write_approval_grants: std::sync::Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    pub bash_workspace_grants: std::sync::Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    pub sensitive_path_grants: std::sync::Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    /// "Always allow" grants for the credential shell gate. Shared so an approved
    /// credential command survives a model swap / capability re-prepare (mirrors the
    /// sibling gates above); otherwise the user re-approves it every time.
    pub credential_shell_grants: std::sync::Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    /// Provider slot for the `code_review` sub-agent tool, FILLED by [`assemble`] (the tool
    /// is built in `prepare` before the provider exists). Shared so a respawn/model-swap
    /// updates the reviewer's provider too. `None` when `opts.review` was false.
    pub review_provider: Option<SharedReviewProvider>,
    /// Host-provider fallback slot for the `task` subagent tool, filled by [`assemble`].
    /// Configured fast/capable tiers are resolved through the runtime-owned cells on
    /// [`CodingAgentConfig`].
    /// `None` when the driver policy or `ATOMCODE_SUBAGENT` override disables it.
    pub subagent_provider: Option<SharedReviewProvider>,
    /// Child-agent assembly prepared from the same live tier-provider cells and
    /// execution policy as `task`. The `team` tool is mounted in the next phase.
    pub team_runner: Option<crate::team::TeamRunnerFactory>,
    /// Runtime-owned Team Agent orchestration. The manager is shared with the
    /// mounted tool, while lifecycle termination is driven only by CodingRuntime.
    pub team_manager: crate::team::TeamRunManager,
    /// User/project CC external hooks (`$ATOMCODE_HOME/hooks.json` + `<root>/.hooks.json`).
    /// ONE instance is registered as BOTH a [`LifecycleHooks`] (already pushed into `hooks`)
    /// and a [`ToolMiddleware`](atomcode_kernel::middleware::ToolMiddleware) (registered by
    /// [`assemble`], before approval). `None` when no hooks are configured — the common path
    /// adds zero overhead (no registration at all).
    pub cc_external_hooks: Option<Arc<CCExternalHooks>>,
    rate_limit_source: Option<Arc<dyn RateLimitWindowSource>>,
}

/// Phase 1 — gather + connect everything the agent needs (async: MCP connect,
/// snapshot load, skill-dir scans). Errors only on a broken EXPLICIT persistent
/// session request whose native aggregate is invalid; everything optional degrades
/// gracefully (no `.mcp.json` → no MCP tools; empty skill dirs → none).
pub async fn prepare(cfg: &CodingAgentConfig, opts: PrepareOptions) -> io::Result<CodingParts> {
    prepare_with_plugin_hooks(cfg, opts, Vec::new()).await
}

/// Like [`prepare`], plus `plugin_cc_hooks` — CC hooks contributed INLINE by installed
/// plugins, which the DRIVER resolves through `atomcode-capabilities::plugin` and
/// threads in here. They are merged with the
/// user/project `hooks.json` into the one [`CCExternalHooks`] runner. Drivers without a
/// plugin system (or with none installed) pass an empty vec and get the same result as
/// [`prepare`].
pub async fn prepare_with_plugin_hooks(
    cfg: &CodingAgentConfig,
    opts: PrepareOptions,
    plugin_cc_hooks: Vec<HookConfig>,
) -> io::Result<CodingParts> {
    prepare_with_plugin_hooks_reusing_lease(cfg, opts, plugin_cc_hooks, None, false).await
}

async fn prepare_with_plugin_hooks_reusing_lease(
    cfg: &CodingAgentConfig,
    opts: PrepareOptions,
    plugin_cc_hooks: Vec<HookConfig>,
    reuse_lease: Option<SessionLease>,
    stage_fresh: bool,
) -> io::Result<CodingParts> {
    let mut registry = ToolRegistry::new();
    let mut names: Vec<String> = Vec::new();
    let turn_execution_policy = Arc::new(TurnExecutionPolicy::new());

    // Always-on core: neutral fs/bash toolset + codeintel. Vision gating: a VL model
    // (e.g. Qwen3-VL) makes read_file hand image files to the model as pictures. Uses the
    // SAME canonical detector as the user-paste path (`model_suggests_vision`) so one
    // model can't accept a pasted image yet refuse a read_file image. NOTE: this is the
    // PREPARE-time flag; `assemble` re-registers read_file on every model swap (see there)
    // so a `/model` change to/from a VL model can't leave it stale.
    register_coding_tools_with_vision(&mut registry, cfg.supports_vision);
    names.extend(
        atomcode_capabilities::tools::coding_tool_names()
            .iter()
            .map(|s| s.to_string()),
    );
    let request_user_input_enabled =
        opts.request_user_input && crate::persona::request_user_input_switch_enabled();
    let todo_enabled = crate::persona::todo_switch_enabled_for(cfg.todo.enabled);
    let subagents_enabled = opts
        .subagents
        .resolve(std::env::var("ATOMCODE_SUBAGENT").ok().as_deref());
    if !todo_enabled {
        names.retain(|name| name != "todowrite");
    }
    if !request_user_input_enabled {
        names.retain(|name| name != "request_user_input");
    }
    register_codeintel_tools(&mut registry);
    names.extend(
        atomcode_capabilities::codeintel::codeintel_tool_names()
            .iter()
            .map(|s| s.to_string()),
    );
    if atomcode_capabilities::codeintel::register_lsp_tool(&mut registry, &cfg.lsp) {
        names.push("lsp".into());
    }

    // External-agent subagents (Claude Code / Codex as named subagent tools).
    // Each enabled profile whose binary is present on PATH mounts one tool.
    let has_external_subagents = if opts.external_subagents.is_empty() {
        false
    } else {
        let mounted = atomcode_capabilities::subagent::tool::register_external_subagent_tools(
            &mut registry,
            &opts.external_subagents,
        );
        let any = !mounted.is_empty();
        names.extend(mounted);
        any
    };

    #[cfg(feature = "atomgit")]
    crate::assemble::register_atomgit_capabilities(&mut registry, &mut names)
        .map_err(|error| io::Error::other(format!("AtomGit tool setup failed: {error}")))?;

    if opts.web && !atomcode_config::config::offline::is_offline_active() {
        registry.register(Arc::new(WebFetchTool));
        // web_search backend: explicit config wins; else the `ATOMCODE_WEB_SEARCH_PROVIDER`
        // env knob; else Exa. `with_provider` maps unknown values to Exa, the safe default.
        let provider = cfg
            .web_search_provider
            .clone()
            .or_else(|| std::env::var("ATOMCODE_WEB_SEARCH_PROVIDER").ok())
            .filter(|p| !p.trim().is_empty());
        let web_search = match provider {
            Some(p) => WebSearchTool::with_provider(&p),
            None => WebSearchTool::new(),
        };
        registry.register(Arc::new(web_search));
        names.push("web_fetch".into());
        names.push("web_search".into());
    }

    // Review-as-capability: a `code_review` sub-agent tool. The provider is filled at
    // assemble (the tool is built here, before the provider exists) via this shared slot,
    // so the reviewer reuses the host's correctly-built — possibly signed — provider.
    let review_provider: Option<SharedReviewProvider> = if opts.review {
        let slot: SharedReviewProvider = Arc::new(std::sync::RwLock::new(None));
        registry.register(Arc::new(
            ReviewTool::new(
                slot.clone(),
                ReviewToolConfig {
                    model: cfg.model.clone(),
                    context_window: cfg.context_window,
                    stream_timeout: cfg.stream_timeout,
                    request_timeout: cfg
                        .request_timeout
                        .unwrap_or_else(|| std::time::Duration::from_secs(300)),
                    max_commits_without_confirmation: 20,
                    max_files_without_confirmation: 40,
                    max_changed_lines_without_confirmation: 4_000,
                    max_diff_bytes_without_confirmation: 256 * 1024,
                    rules_dir: None,
                },
            )
            .with_tool_loop_policy(cfg.tool_loop_policy),
        ));
        names.push("code_review".into());
        Some(slot)
    } else {
        None
    };

    // `task`/`team` are mounted only when the driver policy resolves enabled. An
    // explicit ATOMCODE_SUBAGENT value overrides that policy for compatibility.
    // Configured fast/capable tiers use
    // runtime-owned provider cells; missing/same-as-host tiers reuse the host slot.
    // Child tools: read-only `explore` vs edit-capable `worker`.
    // One runtime-owned manager projects both persistent `team` runs and
    // synchronous legacy `task` batches into the same typed event stream.
    let subagent_cfg = cfg
        .subagent_config
        .as_ref()
        .map(|config| config.subagent.clone())
        .unwrap_or_default();
    let (subagent_max_concurrent, subagent_max_rounds) = subagent_runtime_knobs(
        &subagent_cfg,
        std::env::var("ATOMCODE_SUBAGENT_MAX_ROUNDS")
            .ok()
            .as_deref(),
    );
    let team_manager = crate::team::TeamRunManager::new(crate::team::TeamRuntimeConfig {
        max_concurrent: subagent_max_concurrent,
        ..Default::default()
    });
    let subagent_provider: Option<SharedReviewProvider> = if subagents_enabled {
        use atomcode_capabilities::tools::TaskTool;

        let slot: SharedReviewProvider = Arc::new(std::sync::RwLock::new(None));

        // Child subagent tool registry (mount a subset per type).
        let mut child_reg = atomcode_kernel::tool::ToolRegistry::new();
        atomcode_capabilities::tools::register_coding_tools_with_vision(&mut child_reg, false);
        let child_reg = Arc::new(child_reg);

        let explore_names: Vec<String> = ["read_file", "grep", "glob", "list_directory"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let worker_names: Vec<String> = [
            "read_file",
            "edit_file",
            "write_file",
            "bash",
            "grep",
            "glob",
            "search_replace",
            "list_directory",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

            let reg_e = child_reg.clone();
            let reg_w = child_reg.clone();
            let make_explore_tools = move || {
                let refs: Vec<&str> = explore_names.iter().map(|s| s.as_str()).collect();
                reg_e.mount(&refs)
            };
            let make_worker_tools = move || {
                let refs: Vec<&str> = worker_names.iter().map(|s| s.as_str()).collect();
                reg_w.mount(&refs)
            };

            // Prefer a runtime-injected tier provider, else fall back to the host-provider
            // slot (filled at assemble — the single-model / same-as-host collapse path). The
            // tier provider is a SHARED, swap-aware cell ([`TierProvider`]): it builds lazily on
            // first `task` use (startup never pays the reqwest-client cost) and its cache is
            // reset by the runtime on a `/model` swap, so routing re-resolves without a respawn.
            let fast_cell = cfg.subagent_fast_provider.clone();
            let cap_cell = cfg.subagent_capable_provider.clone();
            let slot_fast = slot.clone();
            let slot_cap = slot.clone();
            let slot_host = slot.clone();
            let make_fast = move || {
                fast_cell.as_ref().and_then(|c| c.get()).unwrap_or_else(|| {
                    slot_fast
                        .read()
                        .ok()
                        .and_then(|g| g.clone())
                        .expect("subagent provider slot filled at assemble before any turn")
                })
            };
            let make_capable = move || {
                cap_cell.as_ref().and_then(|c| c.get()).unwrap_or_else(|| {
                    slot_cap
                        .read()
                        .ok()
                        .and_then(|g| g.clone())
                        .expect("subagent provider slot filled at assemble before any turn")
                })
            };
            let make_host = move || {
                slot_host
                    .read()
                    .ok()
                    .and_then(|provider| provider.clone())
                    .expect("subagent provider slot filled at assemble before any turn")
            };

            // `[subagent]` live knobs. `timeout_secs` remains parse-only compatibility:
            // a productive child is never cancelled for total wall-clock age.
            let task_team_manager = team_manager.clone();
            let mut task_tool = TaskTool::new(
                make_fast,
                make_capable,
                make_explore_tools,
                make_worker_tools,
            )
                .with_host_provider(make_host)
                .with_max_concurrent(subagent_max_concurrent)
                .with_max_rounds(subagent_max_rounds)
                .with_tool_loop_policy(cfg.tool_loop_policy)
                .with_credential_shell_policy(cfg.credential_shell_policy)
                .with_worker_middleware(turn_execution_policy.clone())
                .with_team_event_sink(Arc::new(move |event| {
                    task_team_manager.publish_external(event);
                }));
            if let Some(models) = cfg.subagent_model_providers.clone() {
                task_tool = task_tool.with_named_provider(move |selection| models.get(selection));
            }
            registry.register(Arc::new(task_tool));
            names.push("task".to_string());
            Some(slot)
        } else {
            None
        };

    let team_runner = subagent_provider.as_ref().map(|slot| {
        use atomcode_capabilities::team::{TeamDifficulty, TeamPermission};

        let mut child_registry = atomcode_kernel::tool::ToolRegistry::new();
        atomcode_capabilities::tools::register_coding_tools_with_vision(&mut child_registry, false);
        let child_registry = Arc::new(child_registry);
        let provider_slot = slot.clone();
        let fast_cell = cfg.subagent_fast_provider.clone();
        let capable_cell = cfg.subagent_capable_provider.clone();
        let providers = Arc::new(move |difficulty| {
            let tier = match difficulty {
                TeamDifficulty::Simple => fast_cell.as_ref(),
                TeamDifficulty::Hard => capable_cell.as_ref(),
            };
            tier.and_then(|cell| cell.get()).unwrap_or_else(|| {
                provider_slot
                    .read()
                    .ok()
                    .and_then(|provider| provider.clone())
                    .expect("team provider slot filled at assemble before any turn")
            })
        });
        let tools_registry = Arc::clone(&child_registry);
        let tools = Arc::new(move |permission| {
            let names: &[&str] = match permission {
                TeamPermission::Explore => &["read_file", "grep", "glob", "list_directory"],
                // Bash is intentionally absent. DenyTeamBash remains a second
                // fail-closed gate if this registry is broadened later.
                TeamPermission::Worker => &[
                    "read_file",
                    "edit_file",
                    "write_file",
                    "grep",
                    "glob",
                    "search_replace",
                    "list_directory",
                ],
            };
            tools_registry.mount(names)
        });
        let runner = crate::team::TeamRunnerFactory::new(providers, tools, cfg.working_dir.clone())
            .with_runtime_policy(
                (subagent_max_rounds > 0).then_some(subagent_max_rounds),
                cfg.tool_loop_policy,
                Some(cfg.stream_timeout),
                cfg.request_timeout,
            )
            .with_credential_shell_policy(cfg.credential_shell_policy)
            .with_worker_middleware(turn_execution_policy.clone());
        registry.register(Arc::new(crate::team::TeamTool::new(
            team_manager.clone(),
            runner.job_factory(),
            runner.model_factory(),
        )));
        names.push("team".to_string());
        runner
    });

    // Build the context hook once so skill-catalog ranking and later context
    // injection observe the exact same instruction-file precedence and bytes.
    let session_context_hook = Arc::new(SessionContextHook::new(&cfg.working_dir));
    let instruction_text = session_context_hook.instruction_text();

    // Skills: standard home+project precedence unless the caller supplied dirs.
    let skill_dirs = opts.skill_dirs.clone().unwrap_or_else(|| {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        runtime_skill_dirs(&home, &cfg.working_dir)
    });
    // Plugin-contributed skills: each (dir, namespace) pair registered as
    // `<namespace>:<skill-name>`, matching the slash-menu's core registry
    // convention. Empty when the driver saw no installed plugins (the L1
    // capabilities crate cannot reach the core plugin loader by design).
    let mut skills = SkillRegistry::load(&skill_dirs);
    for (dir, ns) in &opts.plugin_skill_dirs {
        skills.load_dir(dir, Some(ns));
    }
    let skills = Arc::new(skills);
    // Render the catalog BEFORE the registry is moved into the tools; injected as a
    // leading system message by SkillCatalogHook below (without it the model never
    // learns which skills exist — only the use_skill/list_skills tools were mounted).
    let skill_catalog = skills.render_catalog_prioritizing(&instruction_text);
    register_skill_tools(&mut registry, skills);
    names.extend(
        atomcode_capabilities::skills::skill_tool_names()
            .iter()
            .map(|s| s.to_string()),
    );

    // MCP readiness is supplemental: start connections now, but never await them on
    // the session candidate path. `mount()` publishes each connected server's tools
    // atomically for the next turn, then publishes once more when the initial pass
    // reaches its bounded terminal state.
    let (mcp_registry, mcp_connect_rx) = if opts.mcp {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Some(Arc::new(McpRegistry::from_config_background_with_extra(
                &cfg.working_dir,
                Some(event_tx),
                opts.extra_mcp_servers.clone(),
            ))),
            Some(event_rx),
        )
    } else {
        (None, None)
    };
    let mcp_tool_names = Arc::new(std::sync::RwLock::new(Vec::new()));

    // Session binding: the id's single owner.
    let session = match &opts.session {
        SessionMode::Disabled => None,
        SessionMode::Fresh => {
            let id = uuid::Uuid::new_v4().to_string();
            let manager = Arc::new(SessionManager::for_project(&cfg.working_dir));
            let lease = session_lease(&manager, &id, reuse_lease.as_ref())?;
            let now = atomcode_capabilities::session::now_ms();
            let mut meta = SessionMeta::new(&id, cfg.working_dir.to_string_lossy().as_ref(), now);
            meta.owner = StorageOwner::Native;
            if !stage_fresh {
                manager
                    .commit_native_import(
                        &lease,
                        Some(&SessionSnapshot::new(Vec::new())),
                        Some(&PresentationFile::default()),
                        &meta,
                    )
                    .map_err(io::Error::from)?;
            }
            Some(SessionBinding {
                id,
                manager,
                lease,
                resume: None,
                pending_resume_prompt: None,
                staged_fresh: stage_fresh.then_some(meta),
            })
        }
        SessionMode::Resume(id) => {
            let manager = Arc::new(SessionManager::for_project(&cfg.working_dir));
            let lease = session_lease(&manager, id, reuse_lease.as_ref())?;
            // Resume is a native-only boundary. Legacy/unconfirmed data must first
            // converge through a driver importer; accepting a lone snapshot here
            // would bypass ownership and manufacture an incomplete native session.
            let (loaded, pending_resume_prompt) = manager
                .load_native_session_for_resume(&lease)
                .map_err(io::Error::from)?;
            // A version-mismatched snapshot must FAIL here, not fall through to the
            // kernel's empty-start seam — that would silently fresh-start under the
            // SAME session id and corrupt on-disk state.
            check_snapshot_version(&loaded.snapshot)?;
            Some(SessionBinding {
                id: id.clone(),
                manager,
                lease,
                resume: Some(loaded.snapshot),
                pending_resume_prompt,
                staged_fresh: None,
            })
        }
        SessionMode::ExternalSnapshot { id, snapshot } => {
            check_snapshot_version(snapshot)?;
            let manager = Arc::new(SessionManager::for_project(&cfg.working_dir));
            let lease = session_lease(&manager, id, reuse_lease.as_ref())?;
            let loaded = manager.load_native_session(id).map_err(io::Error::from)?;
            check_snapshot_version(&loaded.snapshot)?;
            if loaded.snapshot != *snapshot {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "external snapshot for session {id:?} does not match the canonical native snapshot"
                    ),
                ));
            }
            Some(SessionBinding {
                id: id.clone(),
                manager,
                lease,
                resume: Some(loaded.snapshot),
                pending_resume_prompt: None,
                staged_fresh: None,
            })
        }
    };

    if let Some(b) = &session {
        registry.register(Arc::new(
            RecallTool::new().with_sessions_dir(b.manager.root()),
        ));
        names.push("recall".into());
    }

    // Hooks in the CANONICAL ORDER (registration order = HookChain execution order):
    // 1. SessionContextHook — session_start: inject env + project-instructions + git
    //    snapshot after persona. Rewrites the leading-system run (like MemoryHook); runs
    //    FIRST so the order is persona → context → memory.
    // 2. MemoryHook    — session_start: inject memory.md after the leading-system run
    //    (fresh inject / resume reconcile). Both 1 and 2 reconcile by their own header
    //    prefix, so they compose (the insert position is computed live each time).
    // 2b. SkillCatalogHook — session_start: inject the AVAILABLE SKILLS catalog after
    //    memory (persona → context → memory → skills). Same header-prefix reconcile.
    // 2c. McpInstructionsHook — pre_request append-only projection of live,
    //     server-scoped instructions for currently mounted MCP tools. Ephemeral:
    //     never persists external server guidance into the session snapshot.
    // 3. SnapshotHook  — turn_complete: persist .snapshot + .meta.
    // 4. TranscriptHook— turn_complete: append the .jsonl record. (No coupling with
    //    3 — the order is fixed purely for determinism.)
    // 5. StatusReminderHook — pre_request tail-append (cache red-line: tail only, and
    //    SKIPPED on a turn's round 1 so it never pairs with the user message).
    // 6. VerifyCadenceHook — offer_continuation; FIRST `Some` wins in the chain, so
    //    keep it last: any earlier hook's continuation outranks the cadence nudge.
    let mut hooks: Vec<Arc<dyn LifecycleHooks>> = Vec::new();
    let mut compaction_checkpoint: Option<Arc<dyn CompactionCheckpoint>> = None;
    let mut snapshot_hook_handle = None;
    let mut snapshot_persistence_status = None;
    // Env / project-instructions / git context — unconditional (v1 parity: always present).
    hooks.push(session_context_hook);
    if opts.memory {
        hooks.push(Arc::new(MemoryHook::for_project(&cfg.working_dir)));
    }
    // Skill catalog — leading system message (persona → context → memory → skills), so
    // the model sees which skills are installed and can trigger one on a description
    // match. `None` (no skills) makes the hook a no-op. Reconciles in place on resume.
    // Capture whether any skill is installed BEFORE the catalog is moved — SkillFirstHook
    // (registered below) uses it to stay a no-op when there's nothing to trigger.
    let has_skills = skill_catalog.as_ref().is_some_and(|c| !c.trim().is_empty());
    hooks.push(Arc::new(SkillCatalogHook::new(skill_catalog)));
    if let Some(registry) = &mcp_registry {
        hooks.push(Arc::new(McpInstructionsHook::new(
            Arc::clone(registry),
            Arc::clone(&mcp_tool_names),
        )));
    }
    if let Some(b) = &session {
        let wd = cfg.working_dir.to_string_lossy().into_owned();
        let snapshot_hook = Arc::new(
            SnapshotHook::new(b.manager.clone(), &b.id, &wd)
                .with_lease(b.lease.clone())
                .with_model_attribution(&cfg.provider_name, &cfg.model),
        );
        snapshot_persistence_status = Some(snapshot_hook.persistence_status());
        compaction_checkpoint = Some(snapshot_hook.clone());
        snapshot_hook_handle = Some(snapshot_hook.clone());
        hooks.push(snapshot_hook.clone());
        hooks.push(Arc::new(
            TranscriptHook::new(b.manager.clone(), &b.id)
                .with_persistence_status(snapshot_hook.persistence_status()),
        ));
    }
    // Date awareness is UNCONDITIONAL (production parity): a per-turn <system-reminder> serves
    // recall's relative-date resolution. Runtime pressure (context usage and round counters) is
    // deliberately kept internal; projecting it made weak models invent urgency and rush work.
    // Injected from round 2 of each turn (round 1 is skipped — see StatusReminderHook — to avoid
    // a user-after-user wire pair).
    hooks.push(Arc::new(StatusReminderHook::new()));
    // Pin the workspace root the cadence uses to gate out-of-workspace edits (e.g. a throwaway
    // /tmp write must not arm the "run cargo check" nudge). INVARIANT: this must equal the dir
    // the edit/write tools resolve relative `file_path` against — they stay in lockstep because
    // `/cd` respawns the agent (rebuilding this hook with the new dir), not by mutating cwd in
    // place. If `/cd` ever moves to an in-place cwd mutation, thread the live cwd in here too.
    hooks.push(turn_execution_policy.clone());
    hooks.push(Arc::new(VerifyCadenceHook::with_execution_policy(
        cfg.working_dir.clone(),
        turn_execution_policy.clone(),
    )));
    // Todo hook (native runtime path — the live TUI + webui): per-turn <system-reminder> of the
    // current list so the model keeps it accurate after compaction, PLUS an `offer_continuation`
    // that nudges once to close out open items when the model tries to stop. Gated on the SAME
    // ATOMCODE_TODO switch as the todowrite/todo tools + persona guidance (so the reminder never
    // references tools that aren't mounted). Pushed AFTER VerifyCadenceHook so verify's
    // "first Some wins" continuation outranks the todo-completion nudge. This is the ONLY
    // production registration of TodoHook — every real entrypoint (CLI, daemon, clix) goes
    // through prepare()/assemble() here; `assemble.rs::build_coding_agent` (which also registers
    // it) is reachable only from tests + examples, so there is no double-registration.
    if crate::persona::todo_switch_enabled_for(cfg.todo.enabled) {
        hooks.push(Arc::new(crate::todo::TodoHook));
    }
    // DeepSeek-only opening-turn skill-first reminder. A weak model (deepseek) skips
    // use_skill and dives straight into exploring/solutioning; a static persona line did
    // not hold. This injects a forceful <system-reminder> on the opening turn only, where
    // recency is high. Gated to deepseek (model_needs_firm_execution) + a non-empty skill
    // catalog (never nudge use_skill when no skills are installed). No-op otherwise.
    hooks.push(Arc::new(crate::skill_first::SkillFirstHook::new(
        &cfg.model, has_skills,
    )));
    // NOTE: the `RateLimitHook` is NOT built here. It gates CodingPlan-specific 429
    // messaging on `cfg.base_url` being the gateway, so — like the turn-level
    // `TelemetryHook` — it must be built in `assemble` (which re-runs on a /model
    // swap), NOT here in `prepare` (which does not). A prepare-frozen base_url would
    // keep mislabelling an external-model 429 as a CodingPlan quota after a switch.
    // CC external hooks: user/project `hooks.json` + plugin-contributed inline hooks
    // (`plugin_cc_hooks`, resolved by the host) on the kernel seams — the port of core's
    // CC-parity hook engine onto CodingRuntime. ONE instance serves both seams: pushed here
    // for its LifecycleHooks side (session_start / user_prompt_submit / session_end) and
    // stored in `cc_external_hooks` for its ToolMiddleware side (assemble registers it
    // before approval). Only when hooks actually exist — no hooks at all registers nothing,
    // so the no-hooks path stays free. Its session_start context append sits after the
    // built-in context/status hooks (later = appended after), and it implements no
    // offer_continuation, so VerifyCadenceHook's "first Some wins" contract is untouched.
    let cc_external = {
        let mut cc = CCExternalHooks::load_with_extra(&cfg.working_dir, plugin_cc_hooks);
        // Stamp the persistent session id into every CC payload (CC `session_id`), so a
        // hook can correlate its events with the session. Empty for non-persistent runs.
        if let Some(b) = &session {
            cc = cc.with_session_id(b.id.as_str());
            // CC `transcript_path` = the session's append-only JSONL transcript, so a
            // Stop/StopFailure hook can open the finished turn's full record. The path
            // resolves even before the file is written; unresolvable (session dir gone)
            // → left `None` → the payload carries `null`, never a wedge.
            if let Ok(p) = b.manager.jsonl_path(&b.id) {
                cc = cc.with_transcript_path(p.to_string_lossy().into_owned());
            }
        }
        if cc.is_empty() {
            None
        } else {
            let cc = Arc::new(cc);
            hooks.push(cc.clone() as Arc<dyn LifecycleHooks>);
            Some(cc)
        }
    };
    // NOTE: the turn-level `TelemetryHook` is NOT built here. Its envelope fixes the
    // model + provider_host at construction, and a `/login` or `/model` swap re-runs
    // `assemble` ONLY (never `prepare`) — so building it here froze the prepare-time
    // values (most visibly model="" + the openai host default `api.openai.com` for a
    // session launched before a provider was resolvable). It is built in `assemble`
    // instead, alongside `ToolTelemetryMiddleware`, so every reload re-attributes to
    // the currently active model. (v1 sourced the model live from the running provider;
    // this is the v2 equivalent.)

    let mcp_publication_enabled = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let mcp_work_guard = McpWorkGuard {
        registry: mcp_registry.clone(),
        publication_enabled: Arc::clone(&mcp_publication_enabled),
    };

    Ok(CodingParts {
        shared_cwd: std::sync::Arc::new(std::sync::RwLock::new(cfg.working_dir.clone())),
        plan_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bypass_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        accept_edits: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        turn_execution_policy,
        mcp_plan_grants: std::sync::Arc::new(
            atomcode_capabilities::tools::InMemoryPermissionStore::new(),
        ),
        write_approval_grants: std::sync::Arc::new(
            atomcode_capabilities::tools::InMemoryPermissionStore::new(),
        ),
        bash_workspace_grants: std::sync::Arc::new(
            atomcode_capabilities::tools::InMemoryPermissionStore::new(),
        ),
        sensitive_path_grants: std::sync::Arc::new(
            atomcode_capabilities::tools::InMemoryPermissionStore::new(),
        ),
        credential_shell_grants: std::sync::Arc::new(
            atomcode_capabilities::tools::InMemoryPermissionStore::new(),
        ),
        registry,
        tool_names: names,
        todo_enabled,
        request_user_input_enabled,
        has_external_subagents,
        mcp_tool_names,
        mounted_tools: None,
        mounted_tools_publisher: None,
        mcp_connect_rx,
        mcp_publish_lock: Arc::new(tokio::sync::Mutex::new(())),
        mcp_publication_enabled,
        mcp_catalog_ready: tokio::sync::watch::channel(mcp_registry.is_none()).0,
        _mcp_work_guard: mcp_work_guard,
        approval: Arc::new(ApprovalMiddleware::in_memory()),
        hooks,
        compaction_checkpoint,
        snapshot_hook: snapshot_hook_handle,
        snapshot_persistence_status,
        session,
        runtime_resume: None,
        mcp_registry,
        review_provider,
        subagent_provider,
        team_runner,
        team_manager,
        cc_external_hooks: cc_external,
        rate_limit_source: opts.rate_limit_source,
    })
}

/// Load plugin-contributed hooks for every prepare/reprepare instead of freezing the startup
/// vector. Source failures are explicit because silently dropping security hooks would make a
/// reload appear successful with weaker policy.
pub async fn prepare_with_plugin_hook_source(
    cfg: &CodingAgentConfig,
    opts: PrepareOptions,
    source: &dyn PluginHookSource,
) -> io::Result<CodingParts> {
    prepare_with_plugin_hook_source_reusing_lease(cfg, opts, source, None, false).await
}

pub(crate) async fn prepare_with_plugin_hook_source_reusing_lease(
    cfg: &CodingAgentConfig,
    opts: PrepareOptions,
    source: &dyn PluginHookSource,
    reuse_lease: Option<SessionLease>,
    stage_fresh: bool,
) -> io::Result<CodingParts> {
    let hooks = source
        .load()
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
    prepare_with_plugin_hooks_reusing_lease(cfg, opts, hooks, reuse_lease, stage_fresh).await
}

fn session_lease(
    manager: &SessionManager,
    id: &str,
    reuse_lease: Option<&SessionLease>,
) -> io::Result<SessionLease> {
    match reuse_lease.filter(|lease| lease.id() == id) {
        Some(lease) => {
            manager
                .validate_active_lease(lease)
                .map_err(io::Error::from)?;
            Ok(lease.clone())
        }
        None => manager.acquire_lease(id).map_err(Into::into),
    }
}

impl CodingParts {
    /// The host-owned CodingPlan quota source, if any. Used at `/goal` start to
    /// size the round budget from the live request quota.
    pub(crate) fn rate_limit_source(&self) -> Option<&Arc<dyn RateLimitWindowSource>> {
        self.rate_limit_source.as_ref()
    }

    pub(crate) fn take_snapshot_persistence_uncertain(&self) -> Option<String> {
        self.snapshot_persistence_status
            .as_ref()
            .and_then(SnapshotPersistenceStatus::take_uncertain_commit)
    }

    pub(crate) fn take_cost_persistence_warning(&self) -> Option<String> {
        self.snapshot_persistence_status
            .as_ref()
            .and_then(SnapshotPersistenceStatus::take_cost_warning)
    }

    pub(crate) fn snapshot_persistence_status(&self) -> Option<SnapshotPersistenceStatus> {
        self.snapshot_persistence_status.clone()
    }

    pub(crate) fn snapshot_hook(&self) -> Option<Arc<SnapshotHook>> {
        self.snapshot_hook.clone()
    }

    #[cfg(test)]
    pub(crate) fn report_snapshot_persistence_uncertain(&mut self, message: impl Into<String>) {
        self.snapshot_persistence_status
            .as_ref()
            .expect("persistent test parts must have a snapshot status")
            .report_uncertain_commit(message);
    }

    /// Make a prepared fresh session durable and catalog-visible. This is the
    /// session transition's persistence commit point; preparation and assembly
    /// deliberately leave the catalog untouched.
    pub(crate) fn publish_staged_session(&mut self) -> io::Result<()> {
        let Some(binding) = self.session.as_mut() else {
            return Ok(());
        };
        let Some(meta) = binding.staged_fresh.as_ref() else {
            return Ok(());
        };
        binding
            .manager
            .commit_native_import(
                &binding.lease,
                Some(&SessionSnapshot::new(Vec::new())),
                Some(&PresentationFile::default()),
                meta,
            )
            .map_err(io::Error::from)?;
        binding.staged_fresh = None;
        Ok(())
    }

    /// Carry session-scoped runtime decisions across a capability-graph rebuild.
    /// Fresh/resume/project switches deliberately keep their newly prepared stores.
    pub(crate) fn inherit_runtime_continuity(&mut self, previous: &CodingParts) {
        self.plan_mode = Arc::clone(&previous.plan_mode);
        self.bypass_mode = Arc::clone(&previous.bypass_mode);
        self.accept_edits = Arc::clone(&previous.accept_edits);
        self.approval = Arc::clone(&previous.approval);
        self.mcp_plan_grants = Arc::clone(&previous.mcp_plan_grants);
        self.write_approval_grants = Arc::clone(&previous.write_approval_grants);
        self.bash_workspace_grants = Arc::clone(&previous.bash_workspace_grants);
        self.sensitive_path_grants = Arc::clone(&previous.sensitive_path_grants);
        self.credential_shell_grants = Arc::clone(&previous.credential_shell_grants);
    }

    /// Preserve the exact current conversation across a sessionless provider reassembly.
    pub(crate) fn set_runtime_resume(&mut self, snapshot: SessionSnapshot) {
        self.runtime_resume = Some(snapshot);
    }

    pub(crate) fn runtime_resume_snapshot(&self) -> Option<SessionSnapshot> {
        self.runtime_resume.clone()
    }
    /// Mount the full toolset. The first call creates one updatable catalog shared
    /// by every reassembly of these parts; later calls republish the complete current
    /// set so model-dependent tools (notably `read_file`) stay fresh.
    fn mount(&mut self) -> MountedTools {
        let names = self.selected_tool_names();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();

        if let (Some(mounted), Some(publisher)) =
            (&self.mounted_tools, &self.mounted_tools_publisher)
        {
            publisher.publish(&self.registry, &refs);
            return mounted.clone();
        }

        let (mounted, publisher) = self.registry.mount_updatable(&refs);
        if let (Some(mcp_registry), Some(mut connect_rx)) =
            (self.mcp_registry.clone(), self.mcp_connect_rx.take())
        {
            let tool_registry = self.registry.clone();
            let base_names = self.tool_names.clone();
            let mcp_tool_names = Arc::clone(&self.mcp_tool_names);
            let catalog_publisher = publisher.clone();
            let publish_lock = Arc::clone(&self.mcp_publish_lock);
            let publication_enabled = Arc::clone(&self.mcp_publication_enabled);
            let catalog_ready = self.mcp_catalog_ready.clone();
            tokio::spawn(async move {
                let readiness_registry = Arc::clone(&mcp_registry);
                let initial_readiness = async move {
                    readiness_registry
                        .wait_until_initial_connections_done()
                        .await;
                };
                tokio::pin!(initial_readiness);
                let cancellation_registry = Arc::clone(&mcp_registry);
                let cancellation = async move {
                    cancellation_registry.wait_for_cancellation().await;
                };
                tokio::pin!(cancellation);

                loop {
                    tokio::select! {
                        _ = &mut cancellation => break,
                        _ = &mut initial_readiness => {
                            publish_ready_mcp_tools(
                                Arc::clone(&mcp_registry),
                                tool_registry.clone(),
                                base_names.clone(),
                                Arc::clone(&mcp_tool_names),
                                catalog_publisher.clone(),
                                Arc::clone(&publish_lock),
                                Arc::clone(&publication_enabled),
                            )
                            .await;
                            catalog_ready.send_replace(true);
                            break;
                        }
                        event = connect_rx.recv() => {
                            match event {
                                Some(McpConnectEvent::Connected { name }) => {
                                    publish_connected_mcp_server(
                                        Arc::clone(&mcp_registry),
                                        name,
                                        tool_registry.clone(),
                                        base_names.clone(),
                                        Arc::clone(&mcp_tool_names),
                                        catalog_publisher.clone(),
                                        Arc::clone(&publish_lock),
                                        Arc::clone(&publication_enabled),
                                    )
                                    .await;
                                }
                                Some(_) => {}
                                None => break,
                            }
                        }
                    }
                }
            });
        }
        self.mounted_tools = Some(mounted.clone());
        self.mounted_tools_publisher = Some(publisher);
        mounted
    }

    fn selected_tool_names(&self) -> Vec<String> {
        let mut names = self.tool_names.clone();
        let dynamic = match self.mcp_tool_names.read() {
            Ok(names) => names,
            Err(poisoned) => poisoned.into_inner(),
        };
        names.extend(dynamic.iter().cloned());
        names
    }

    /// Readiness receiver for non-interactive surfaces whose first turn should
    /// include the catalog reconciled before their caller-owned timeout.
    pub(crate) fn mcp_readiness_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.mcp_catalog_ready.subscribe()
    }

    /// Fail-closed cutover used before a capability reload reads mutable MCP
    /// config/trust/auth state. Once disabled, this scope's late connection events
    /// cannot republish tools even if the replacement candidate fails.
    pub(crate) async fn withdraw_mcp_tools(&mut self) {
        self.mcp_publication_enabled
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(registry) = &self.mcp_registry {
            registry.cancel_pending_work();
        }
        let _publish_guard = self.mcp_publish_lock.lock().await;
        match self.mcp_tool_names.write() {
            Ok(mut names) => names.clear(),
            Err(poisoned) => poisoned.into_inner().clear(),
        }
        if let Some(publisher) = &self.mounted_tools_publisher {
            let refs: Vec<&str> = self.tool_names.iter().map(String::as_str).collect();
            publisher.publish(&self.registry, &refs);
        }
    }

    pub(crate) async fn mcp_statuses(
        &self,
    ) -> Vec<(String, atomcode_capabilities::mcp::ServerStatus)> {
        match &self.mcp_registry {
            Some(registry) => registry.server_statuses().await,
            None => Vec::new(),
        }
    }

    pub(crate) fn mcp_tools_for_server(&self, server: &str) -> Vec<String> {
        let names = match self.mcp_tool_names.read() {
            Ok(names) => names,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(registry) = self.mcp_registry.as_ref() else {
            return Vec::new();
        };
        let published: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
        registry
            .tool_aliases_for_server(server)
            .into_iter()
            .filter(|alias| published.contains(alias.as_str()))
            .collect()
    }

    /// Register an EXTRA driver-contributed tool into the kernel toolset, so it is
    /// both resolvable during a turn AND exposed to the model (added to `tool_names`,
    /// which [`mount`](Self::mount) reads). The `registry` / `tool_names` fields are
    /// crate-private — this is the supported seam for the runtime to inject
    /// a tool the always-on capability set doesn't include.
    ///
    /// Idempotent on name: re-registering the same name (e.g. on a respawn that
    /// re-injects `schedule_wakeup`) replaces the tool in the registry
    /// and does NOT duplicate the name, keeping the mounted tool list (a cache prefix)
    /// byte-stable across respawns.
    ///
    /// Call BEFORE [`assemble`] (it snapshots the toolset via `mount`). The runtime
    /// uses this for its kernel-side `schedule_wakeup` (`/loop`).
    pub fn register_extra_tool(&mut self, tool: Arc<dyn atomcode_kernel::tool::Tool>) {
        let name = tool.name().to_string();
        if !self.tool_names.iter().any(|n| n == &name) {
            self.tool_names.push(name);
        }
        self.registry.register(tool);
    }
}

async fn publish_ready_mcp_tools(
    mcp_registry: Arc<McpRegistry>,
    mut tool_registry: ToolRegistry,
    base_names: Vec<String>,
    mcp_tool_names: Arc<std::sync::RwLock<Vec<String>>>,
    catalog_publisher: MountedToolsPublisher,
    publish_lock: Arc<tokio::sync::Mutex<()>>,
    publication_enabled: Arc<std::sync::atomic::AtomicBool>,
) {
    if !publication_enabled.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    // Discovery is network I/O. Keep it outside the publication lock so a
    // fail-closed withdrawal is never delayed by an MCP server timeout.
    let tool_infos = tokio::select! {
        tools = mcp_registry.list_all_tools() => tools,
        _ = mcp_registry.wait_for_cancellation() => return,
    };
    let adapters: Vec<Arc<dyn atomcode_kernel::tool::Tool>> = tool_infos
        .into_iter()
        .filter_map(|info| {
            match atomcode_capabilities::mcp::McpToolAdapter::new(mcp_registry.clone(), info) {
                Ok(adapter) => Some(Arc::new(adapter) as Arc<dyn atomcode_kernel::tool::Tool>),
                Err(error) => {
                    eprintln!("[mcp] tool publication skipped: {error}");
                    None
                }
            }
        })
        .collect();
    // Serialize only the in-memory commit. Re-check after locking because a
    // capability reload may have revoked this publication while discovery ran.
    let _publish_guard = publish_lock.lock().await;
    if !publication_enabled.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let discovered = mcp::register_mcp_tools(&mut tool_registry, adapters);
    match mcp_tool_names.write() {
        Ok(mut names) => *names = discovered.clone(),
        Err(poisoned) => *poisoned.into_inner() = discovered.clone(),
    }
    let mut selected = base_names;
    selected.extend(discovered);
    let refs: Vec<&str> = selected.iter().map(String::as_str).collect();
    catalog_publisher.publish(&tool_registry, &refs);
}

async fn publish_connected_mcp_server(
    mcp_registry: Arc<McpRegistry>,
    server: String,
    mut tool_registry: ToolRegistry,
    base_names: Vec<String>,
    mcp_tool_names: Arc<std::sync::RwLock<Vec<String>>>,
    catalog_publisher: MountedToolsPublisher,
    publish_lock: Arc<tokio::sync::Mutex<()>>,
    publication_enabled: Arc<std::sync::atomic::AtomicBool>,
) {
    // A newly connected server should not make every existing server repeat
    // tools/list. The final readiness publication below remains the reconciliation
    // pass for transient discovery failures.
    if !publication_enabled.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let tool_infos = tokio::select! {
        tools = mcp_registry.list_tools_for_server(&server) => tools,
        _ = mcp_registry.wait_for_cancellation() => return,
    };
    let adapters: Vec<Arc<dyn atomcode_kernel::tool::Tool>> = tool_infos
        .into_iter()
        .filter_map(|info| {
            match atomcode_capabilities::mcp::McpToolAdapter::new(Arc::clone(&mcp_registry), info) {
                Ok(adapter) => Some(Arc::new(adapter) as Arc<dyn atomcode_kernel::tool::Tool>),
                Err(error) => {
                    eprintln!("[mcp] tool publication skipped: {error}");
                    None
                }
            }
        })
        .collect();
    let _publish_guard = publish_lock.lock().await;
    if !publication_enabled.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let discovered = mcp::register_mcp_tools(&mut tool_registry, adapters);
    let mut selected = base_names;
    {
        let mut names = match mcp_tool_names.write() {
            Ok(names) => names,
            Err(poisoned) => poisoned.into_inner(),
        };
        names.extend(discovered);
        names.sort_unstable();
        names.dedup();
        selected.extend(names.iter().cloned());
    }
    let refs: Vec<&str> = selected.iter().map(String::as_str).collect();
    catalog_publisher.publish(&tool_registry, &refs);
}

/// Phase 2 — composition: parts + provider → a runnable [`Agent`].
///
/// A session-bound assemble ALWAYS picks up the session's latest on-disk snapshot
/// (the SnapshotHook persisted one every turn), so calling it again on the SAME
/// parts with a new provider IS the respawn (B2 model swap / reload): approval
/// grants, hook state, session identity, AND the conversation all carry over. A
/// plain re-`assemble` can never rewind a live session — the one respawn footgun
/// the design review flagged. Errors:
/// - a snapshot that exists but can't be read or has an unsupported version
///   (continuing would silently fresh-start the SAME session id and corrupt its
///   transcript/snapshot — the exact "silent fresh start" the Resume contract
///   forbids);
/// - only an explicitly staged fresh session may have no aggregate yet; every
///   resume/reassemble requires metadata, snapshot, and presentation together.
///
/// CONCURRENCY CONTRACT: at most ONE live agent per `CodingParts` — await the old
/// `AgentHandle.task` (after `Shutdown`) before re-assembling. The session hooks
/// hold per-turn state and write per-session files; two live agents on the same
/// parts would interleave both.
pub fn assemble(
    parts: &mut CodingParts,
    cfg: &CodingAgentConfig,
    provider: Arc<dyn LlmProvider>,
) -> io::Result<Agent> {
    // Model swap (e.g. `/model`) routes here via the runtime WITHOUT re-running `prepare`,
    // so re-register `read_file` with the CURRENT model's vision capability — otherwise the
    // PREPARE-time flag goes stale and a text-only model could receive a base64 image (or a
    // VL model none). `register` overwrites by name, so this idempotently refreshes the one
    // tool whose behavior depends on the model. Same model-swap-refresh pattern as the
    // `review_provider` slot below.
    parts
        .registry
        .register(Arc::new(ReadFileTool::new(cfg.supports_vision)));

    // Session-bound: reload the complete canonical aggregate. Only a fresh
    // runtime intentionally staged in memory is allowed to assemble before its
    // first aggregate publication.
    if let Some(b) = &mut parts.session {
        match b.manager.load_native_session(&b.id) {
            Ok(loaded) => {
                let mut snap = loaded.snapshot;
                check_snapshot_version(&snap)?;
                reconcile_coding_persona(
                    &mut snap,
                    cfg,
                    parts.todo_enabled,
                    parts.request_user_input_enabled,
                    parts.review_provider.is_some(),
                    parts.subagent_provider.is_some(),
                    parts.has_external_subagents,
                );
                b.resume = Some(snap);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && b.staged_fresh.is_some() => {}
            Err(e) => return Err(e.into()),
        }
    }

    // A telemetry-metering decorator over the host provider. Calls made OUTSIDE the host
    // agent loop — the tier-2 overflow summary AND the `code_review` sub-agent's rounds —
    // never reach the turn-level TelemetryHook (which fires on this loop's on_request /
    // on_model_response), so without this their token spend is invisible. The host loop's
    // PRIMARY provider stays bare below: the TelemetryHook already meters it, and wrapping
    // it too would double-count. `None` ⇒ telemetry off ⇒ the bare provider (zero overhead).
    let out_of_loop_provider: Arc<dyn LlmProvider> = match &parts.session {
        Some(session) => {
            let mut recorder = atomcode_capabilities::session::DetachedUsageRecorder::new(
                session.manager.clone(),
                &session.id,
                &cfg.provider_name,
                &cfg.model,
            );
            if let Some(status) = parts.snapshot_persistence_status() {
                recorder = recorder.with_persistence_status(status);
            }
            Arc::new(atomcode_capabilities::session::UsageRecordingProvider::new(
                provider.clone(),
                recorder,
            ))
        }
        None => provider.clone(),
    };
    let metered_provider: Arc<dyn LlmProvider> = match &cfg.telemetry {
        Some(tel) => Arc::new(crate::telemetry::MeteredProvider::new(
            out_of_loop_provider.clone(),
            tel.clone(),
            cfg.provider_type.as_str(),
            &cfg.base_url,
            &cfg.model,
            parts.session.as_ref().map(|b| b.id.as_str()),
        )),
        None => out_of_loop_provider.clone(),
    };

    // Fill the `code_review` tool's provider slot (the tool was built in `prepare` before
    // the provider existed). Hand it the METERED provider so the reviewer's LLM rounds emit
    // LlmChat token telemetry — the review sub-agent runs its own kernel loop with no
    // TelemetryHook of its own. A model-swap respawn re-runs assemble and updates it, so the
    // reviewer always uses the current (metered) provider.
    if let Some(slot) = &parts.review_provider {
        // Tag the reviewer's rounds with surface="code_review". The sub-agent shares the
        // HOST session_id, so without this its LlmChat events are indistinguishable from
        // the primary loop's — the tag lets telemetry attribute review token spend.
        let review_provider: Arc<dyn LlmProvider> = match &cfg.telemetry {
            Some(tel) => Arc::new(
                crate::telemetry::MeteredProvider::new(
                    out_of_loop_provider.clone(),
                    tel.clone(),
                    cfg.provider_type.as_str(),
                    &cfg.base_url,
                    &cfg.model,
                    parts.session.as_ref().map(|b| b.id.as_str()),
                )
                .with_surface("code_review"),
            ),
            None => out_of_loop_provider.clone(),
        };
        if let Ok(mut g) = slot.write() {
            *g = Some(review_provider);
        }
    }

    if let Some(slot) = &parts.subagent_provider {
        let sub_provider: Arc<dyn LlmProvider> = match &cfg.telemetry {
            Some(tel) => Arc::new(
                crate::telemetry::MeteredProvider::new(
                    out_of_loop_provider.clone(),
                    tel.clone(),
                    cfg.provider_type.as_str(),
                    &cfg.base_url,
                    &cfg.model,
                    parts.session.as_ref().map(|b| b.id.as_str()),
                )
                .with_surface("subagent"),
            ),
            None => out_of_loop_provider.clone(),
        };
        if let Ok(mut g) = slot.write() {
            *g = Some(sub_provider);
        }
    }

    // Tier-2 overflow summary uses the same metered provider so its summary LLM call is
    // likewise counted.
    let summary_provider = metered_provider;

    // When a session is present, wire the artifact store: register the fetch_output tool
    // so the model can retrieve large outputs, and prepare the middleware that intercepts
    // oversized tool results and spills them to disk.  No session → no artifact I/O.
    let artifact_store: Option<Arc<ArtifactStore>> = parts.session.as_ref().and_then(|b| {
        b.manager
            .artifacts_dir(&b.id)
            .ok()
            .map(|dir| Arc::new(ArtifactStore::new(dir)))
    });
    if let Some(store) = &artifact_store {
        parts
            .registry
            .register(Arc::new(FetchOutputTool::new(store.clone())));
    }

    let mut builder = Agent::builder()
        .provider(provider)
        .tools(parts.mount())
        .persona(coding_persona_with_capabilities(
            &cfg.model,
            cfg.preferred_language,
            parts.todo_enabled,
            parts.request_user_input_enabled,
            parts.review_provider.is_some(),
            parts.subagent_provider.is_some(),
            parts.has_external_subagents,
        ))
        // Repair model-produced arguments before any observer or policy gate reads them.
        // Approval must inspect the same bytes that the tool executes.
        .middleware(Arc::new(RepairToolArgsMiddleware));
    // Tool telemetry is the first observation middleware. It never rewrites or blocks.
    // Its `before` always stamps the call, including a call that approval later denies.
    if let Some(tel) = &cfg.telemetry {
        builder = builder.middleware(Arc::new(crate::telemetry::ToolTelemetryMiddleware::new(
            tel.clone(),
            cfg.provider_type.as_str(),
            &cfg.base_url,
            &cfg.model,
            parts.session.as_ref().map(|b| b.id.as_str()),
        )));
        // Turn-level TelemetryHook (observation-only: on_request + on_model_response →
        // one LlmChat per round). Built HERE, not in `prepare`, so a /login or /model
        // swap (which re-runs assemble only) re-attributes every subsequent round to the
        // CURRENT model + provider_host instead of the value frozen at prepare. Vendor =
        // the configured `provider_type` (the exact vocabulary telemetry's
        // `resolve_provider_host` keys on). It mutates nothing, so chain order is moot.
        builder = builder.hook(Arc::new(crate::telemetry::TelemetryHook::new(
            tel.clone(),
            cfg.provider_type.as_str(),
            &cfg.base_url,
            &cfg.model,
            parts.session.as_ref().map(|b| b.id.as_str()),
        )));
    }
    let datalog = DatalogHook::new(
        &cfg.working_dir,
        &cfg.datalog,
        &cfg.model,
        cfg.context_window,
    )
    .map(Arc::new);
    let rate_limit_hook = match &parts.rate_limit_source {
        Some(source) => crate::rate_limit::RateLimitHook::with_source(
            cfg.base_url.clone(),
            source.clone(),
        ),
        None => crate::rate_limit::RateLimitHook::new(cfg.base_url.clone()),
    }
    .with_max_attempts(cfg.retry_max_attempts);
    let rate_limit_hook: Arc<dyn LifecycleHooks> = Arc::new(rate_limit_hook);
    let mut builder = builder
        // Hard per-turn user boundary. Register before every middleware that can `Allow`
        // and bypass downstream approval gates.
        .middleware(parts.turn_execution_policy.clone())
        // Plan-mode gate BEFORE approval: while active it blocks mutating (Risky)
        // tools outright, so there's no point prompting the user to approve a write
        // plan mode forbids. Read-only when inactive — zero cost off the plan path.
        .middleware(Arc::new(crate::plan_mode::PlanModeGate::new(
            parts.plan_mode.clone(),
            parts.mcp_plan_grants.clone(),
        )))
        // Plan-mode reminder (ephemeral request tail) — pairs with the gate: the gate
        // blocks mutating TOOLS, this keeps the model PLANNING instead of writing the
        // implementation inline. Shares the same plan_mode flag; cache-safe (tail only).
        .hook(Arc::new(crate::plan_mode::PlanModeReminderHook::new(
            parts.plan_mode.clone(),
        )))
        // Rate-limit hook: on a 429 it decides wait-vs-pause from CodingPlan usage windows.
        // Built HERE (not in `prepare`) — like TelemetryHook — so a /model swap (which re-runs
        // assemble only) re-captures the CURRENT provider's base_url. That base_url is the gate
        // that keeps a user's external-model 429 from being mislabelled as a CodingPlan quota;
        // a prepare-frozen base_url would defeat it after a model switch.
        .hook(rate_limit_hook)
        // Credentials are a product-wide security boundary, independent of whether
        // the optional AtomGit integration is compiled in. It must run before the
        // approval-oriented SensitivePathGate so an explicit extraction cannot be
        // downgraded from terminal denial into a retryable approval denial.
        .middleware(Arc::new(
            atomcode_capabilities::tools::CredentialBashGate::with_store(
                cfg.credential_shell_policy,
                parts.credential_shell_grants.clone(),
            ),
        ))
        // Sensitive-path read gate: read tools are Safe (skip approval), so without this an
        // agent could silently read ~/.ssh / .env / creds and leak them to the provider.
        // Acts ONLY on Safe tools touching a sensitive path → one approval round-trip.
        .middleware(Arc::new(SensitivePathGate::with_store(
            parts.sensitive_path_grants.clone(),
        )));
    #[cfg(feature = "atomgit")]
    {
        // Typed AtomGit tools are the only supported API path: they keep credentials
        // outside model-visible arguments and retain action-aware approval semantics.
        builder = builder.middleware(Arc::new(
            atomcode_capabilities::tools::AtomgitBashGate::new(),
        ));
    }
    // CC external hooks (PreToolUse gate). Runs AFTER the hard PlanMode/SensitivePath gates
    // (which must stay un-bypassable by a hook `allow`) but BEFORE every auto-approve
    // convenience gate — OpenFileWorkspaceGate and especially WriteApprovalGate, which
    // auto-`Allow`s in-workspace writes and would short-circuit the chain before a hook ever
    // sees the call (so a PreToolUse hook on edit/write would silently never run). This
    // matches Claude Code, where a PreToolUse hook IS the permission entry point: its
    // `updatedInput` rewrite lands before WriteApprovalGate inspects the path and before the
    // user sees approval (so the approved bytes are what run), `allow` short-circuits the
    // whole approval chain, and `deny` blocks. Registered only when hooks exist (else zero
    // middleware overhead).
    if let Some(cc) = &parts.cc_external_hooks {
        builder = builder.middleware(cc.clone());
    }
    let mut builder = builder
        // open_file is Risky (launches a GUI), so approval would prompt on EVERY preview.
        // Restore the legacy engine's behavior: auto-approve when the target is inside the
        // workspace (benign side effect on the user's own files). BEFORE approval so its
        // `Allow` short-circuits the prompt; out-of-workspace paths fall through and still
        // prompt. Reads the SAME live cwd handle below, so a /cd moves the boundary.
        .middleware(Arc::new(OpenFileWorkspaceGate::new(
            parts.shared_cwd.clone(),
        )))
        // Workspace-aware, per-path approval for the file-mutation tools (v1 granularity):
        // in-workspace non-sensitive writes auto-approve; sensitive writes always re-prompt
        // (never remembered); out-of-workspace writes prompt with a PER-PATH "Always". Owns
        // write-tool approval, so it must sit BEFORE the generic approval gate (its `Allow`
        // short-circuits the prompt). Reads the SAME live cwd handle, so /cd moves the boundary.
        .middleware(Arc::new(
            WriteApprovalGate::with_store(
                parts.shared_cwd.clone(),
                parts.write_approval_grants.clone(),
            )
            .with_accept_edits(parts.accept_edits.clone()),
        ))
        // Workspace-aware approval for DESTRUCTIVE bash (rm/mv/cp/dd/redirect…) whose target
        // lands OUTSIDE the workspace: prompt with a per-directory "Always", mirroring
        // WriteApprovalGate for the write tools. In-workspace destructive bash is unchanged
        // (single-file rm stays Safe→runs; recursive rm still reaches ApprovalMiddleware).
        // BEFORE the generic approval gate so its `Allow` short-circuits the prompt; reads the
        // SAME live cwd handle, so /cd moves the boundary. Mode-independent (accept-edits is for
        // edits only); full Auto bypasses it via the driver auto-answering.
        .middleware(Arc::new(BashWorkspaceGate::with_store(
            parts.shared_cwd.clone(),
            parts.bash_workspace_grants.clone(),
        )))
        // Approval AFTER the CC PreToolUse gate + the write/open auto-approve gates — every
        // arg-rewrite (CC `updatedInput`) has already applied, so the user approves the exact
        // bytes that run.
        .middleware(parts.approval.clone())
        // LIVE cwd handle (not the immutable pin): /cd mutates parts.shared_cwd.
        .working_dir_shared(parts.shared_cwd.clone())
        .chat_options(cfg.chat_options.clone())
        // Cache-friendly task-boundary stub + hard-overflow recovery ladder (stub→truncate
        // →drain+LLM-summary). Stubs old tool results once utilization crosses the threshold
        // (kept full below it); the overflow tiers fire only on a typed overflow error.
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
    // An explicit retry_max_attempts value is the TOTAL adapter OPEN budget.
    // Do not multiply it by the kernel's historical outer same-round retries;
    // the neutral kernel default remains unchanged when the setting is absent.
    if cfg.retry_max_attempts.is_some() {
        builder = builder.max_provider_retries(0);
    }
    builder = builder.round_cap_checkpoint(cfg.round_cap_checkpoint);
    // Approval liveness: `Some(d)` ⇒ fail-closed after `d` (headless); `None` ⇒ PARK until
    // answered (interactive — a present human must not be auto-denied). The kernel defaults
    // to unbounded when `.request_timeout` is never set, so None = park.
    if let Some(d) = cfg.request_timeout {
        builder = builder.request_timeout(d);
    }
    for h in &parts.hooks {
        builder = builder.hook(h.clone());
    }
    // Eagerness is generation-scoped: `/model` reuses CodingParts and re-runs only
    // `assemble`, so deriving this hook in `prepare` would freeze Auto/eagerness against the
    // session's original model generation. TodoHook itself is model-neutral and remains
    // in the reusable parts chain above.
    if parts.todo_enabled {
        builder = builder.hook(Arc::new(crate::todo::TodoEagerHook::new(
            &cfg.model,
            &cfg.provider_type,
            cfg.todo.eager,
        )));
    }
    if let Some(datalog) = datalog {
        // Register last in both chains: the lifecycle observer sees the final prompt/request
        // after product hooks, and the tool observer sees the final middleware-transformed
        // result. The same writer owns correlation for both without owning runtime state.
        builder = builder
            .hook(datalog.clone())
            .middleware(datalog as Arc<dyn ToolMiddleware>);
    }
    if let Some(checkpoint) = &parts.compaction_checkpoint {
        builder = builder.compaction_checkpoint(checkpoint.clone());
    }
    if let Some(b) = &parts.session {
        builder = builder.session_id(&b.id);
        // Share the parent's `x-atomcode-session-id` with the subagent tier providers so a
        // `task` fan-out's children run within the SAME gateway window as the main
        // conversation — otherwise each session-less child is a distinct window and GLM-5.2's
        // multi-window guard serializes the strong-tier subtasks. (Single-model users already
        // reuse the host provider, which the kernel binds with this id, so they're unaffected.)
        if let Some(cell) = &cfg.subagent_fast_provider {
            cell.set_session_id(&b.id);
        }
        if let Some(cell) = &cfg.subagent_capable_provider {
            cell.set_session_id(&b.id);
        }
        if let Some(models) = &cfg.subagent_model_providers {
            models.set_session_id(&b.id);
            let manager = b.manager.clone();
            let session_id = b.id.clone();
            let persistence_status = parts.snapshot_persistence_status();
            models.set_usage_recorder_factory(Arc::new(move |selection, model| {
                let mut recorder =
                    atomcode_capabilities::session::DetachedUsageRecorder::new(
                        manager.clone(),
                        &session_id,
                        selection,
                        model,
                    );
                if let Some(status) = persistence_status.clone() {
                    recorder = recorder.with_persistence_status(status);
                }
                recorder
            }));
        }
        if let Some(registry) = cfg.subagent_config.as_deref() {
            if let Some((fast_key, capable_key)) =
                crate::subagent_tiers::resolve_tier_keys(registry, &cfg.model)
            {
                let install_recorder = |cell: &Arc<crate::config::TierProvider>, key: &str| {
                    // `key` is a model-selection id (design §14.2); resolve it the
                    // same way the tier provider was built so usage attribution
                    // uses the same model identity.
                    if let Ok(resolved) = registry.resolve_model(Some(key)) {
                        let mut recorder =
                            atomcode_capabilities::session::DetachedUsageRecorder::new(
                                b.manager.clone(),
                                &b.id,
                                key,
                                &resolved.model,
                            );
                        if let Some(status) = parts.snapshot_persistence_status() {
                            recorder = recorder.with_persistence_status(status);
                        }
                        cell.set_usage_recorder(recorder);
                    }
                };
                if let Some(cell) = &cfg.subagent_fast_provider {
                    install_recorder(cell, &fast_key);
                }
                if let Some(cell) = &cfg.subagent_capable_provider {
                    install_recorder(cell, &capable_key);
                }
            }
        }
        if let Some(snap) = &b.resume {
            builder = builder.resume(snap.clone());
        }
    } else if let Some(mut snapshot) = parts.runtime_resume.as_ref().cloned() {
        reconcile_coding_persona(
            &mut snapshot,
            cfg,
            parts.todo_enabled,
            parts.request_user_input_enabled,
            parts.review_provider.is_some(),
            parts.subagent_provider.is_some(),
                    parts.has_external_subagents,
        );
        builder = builder.resume(snapshot);
    }
    if let Some(models) = cfg.subagent_model_providers.as_ref() {
        let telemetry_factory = match (cfg.telemetry.as_ref(), cfg.subagent_config.as_ref()) {
            (Some(telemetry), Some(model_config)) => {
                let telemetry = telemetry.clone();
                let model_config = model_config.clone();
                let mut base_config = cfg.clone();
                // The decorator is stored inside `subagent_model_providers`; do not
                // capture that same Arc through the cloned runtime config.
                base_config.subagent_fast_provider = None;
                base_config.subagent_capable_provider = None;
                base_config.subagent_model_providers = None;
                base_config.subagent_config = None;
                let session_id = parts.session.as_ref().map(|binding| binding.id.clone());
                Some(Arc::new(move |selection: &str, provider| {
                    let resolved = model_config
                        .resolve_model(Some(selection))
                        .map_err(|error| error.to_string())?;
                    let tier = crate::provider_factory::derive_tier_config_from_resolved(
                        &base_config,
                        &resolved,
                    );
                    Ok(Arc::new(
                        crate::telemetry::MeteredProvider::new(
                            provider,
                            telemetry.clone(),
                            tier.provider_type.as_str(),
                            &tier.base_url,
                            &tier.model,
                            session_id.as_deref(),
                        )
                        .with_surface("subagent"),
                    ) as Arc<dyn LlmProvider>)
                })
                    as crate::config::SubagentTelemetryProviderFactory)
            }
            _ => None,
        };
        models.set_telemetry_provider_factory(telemetry_factory);
    }
    // Ensure the repo's `atomcode` project label after a successful `git push` to a
    // gitcode/atomgit remote. THIS is the production mount: the terminal TUI, daemon, and
    // webui all build their agent here via `parts::assemble`. (`assemble.rs::build_coding_agent`
    // also mounts it, but that path is reachable only from tests/examples — so before this the
    // middleware never ran for a real session.) Best-effort: every failure is a `tracing::warn`
    // and the turn proceeds. Gated on `atomgit` (its sole consumer).
    #[cfg(feature = "atomgit")]
    {
        builder = builder.middleware(Arc::new(
            atomcode_capabilities::tools::GitPushLabelMiddleware::new(cfg.working_dir.clone()),
        ));
    }
    // Artifact spill middleware: intercepts oversized tool results and saves them to disk so
    // the conversation only carries a preview + handle. Only wired when a session is present
    // (no session = no on-disk store; the fetch_output tool is not registered either).
    if let Some(store) = artifact_store {
        builder = builder.middleware(Arc::new(ArtifactMiddleware::new(store)));
    }
    let agent = builder.build();
    // Commit model attribution only after every fallible assembly step has
    // succeeded. ReassembleProvider stops the old agent before entering here,
    // so no accepted turn can observe a half-switched attribution.
    if let Some(snapshot_hook) = &parts.snapshot_hook {
        snapshot_hook.set_model_attribution(&cfg.provider_name, &cfg.model);
    }
    Ok(agent)
}

const ATOMCODE_PERSONA_PREFIX: &str =
    "You are AtomCode, an AI coding agent by AtomGit running the ";
const MODEL_CHANGE_CONTEXT_PREFIX: &str = "=== MODEL CHANGE ===";

/// Legacy drivers persist conversation history without the separately supplied
/// system prompt. A v2 resume must restore that prompt, while a model switch must
/// replace the old model identity instead of retaining or duplicating it.
fn reconcile_coding_persona(
    snapshot: &mut SessionSnapshot,
    cfg: &CodingAgentConfig,
    todo_enabled: bool,
    request_user_input_enabled: bool,
    review_enabled: bool,
    subagents_enabled: bool,
    external_subagents_enabled: bool,
) {
    let persona = coding_persona_with_capabilities(
        &cfg.model,
        cfg.preferred_language,
        todo_enabled,
        request_user_input_enabled,
        review_enabled,
        subagents_enabled,
        external_subagents_enabled,
    );
    let is_persona = |message: &Message| {
        message.role == Role::System && message.text.starts_with(ATOMCODE_PERSONA_PREFIX)
    };
    let is_model_change = |message: &Message| {
        message.role == Role::System && message.text.starts_with(MODEL_CHANGE_CONTEXT_PREFIX)
    };
    let already_current = snapshot
        .messages
        .first()
        .is_some_and(|message| message.role == Role::System && message.text == persona)
        && snapshot
            .messages
            .iter()
            .skip(1)
            .all(|message| !is_persona(message))
        && snapshot
            .messages
            .iter()
            .filter(|message| is_model_change(message))
            .count()
            == 0;
    if already_current {
        return;
    }

    snapshot
        .messages
        .retain(|message| !is_persona(message) && !is_model_change(message));
    snapshot.messages.insert(0, Message::system(persona));
    snapshot.cache_epoch = snapshot.cache_epoch.saturating_add(1);
}

/// A snapshot from another kernel version must NOT be silently re-bound to its
/// session id: the kernel's forward-compat seam would start EMPTY, and the session
/// hooks would then overwrite the (newer-format) snapshot and append duplicate
/// turn_ids into the existing transcript.
fn check_snapshot_version(snap: &SessionSnapshot) -> io::Result<()> {
    if snap.version != atomcode_kernel::message::SNAPSHOT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "session snapshot version {} unsupported (this kernel supports {}); \
                 refusing to rebind the session id to an empty conversation",
                snap.version,
                atomcode_kernel::message::SNAPSHOT_VERSION
            ),
        ));
    }
    Ok(())
}

/// Legacy environment-only resolver retained for callers that have not yet adopted
/// [`SubagentPolicy`]. New runtime assembly resolves the explicit driver policy first.
pub fn subagent_enabled_from_env(var: Option<&str>) -> bool {
    match var {
        None => true,
        Some(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        ),
    }
}

/// Resolve the live `task` subagent knobs. The legacy `timeout_secs` field is deliberately
/// ignored: child liveness is owned by provider idle timeouts, the round cap, and cancellation.
pub fn subagent_runtime_knobs(
    cfg: &atomcode_config::config::SubAgentConfig,
    max_rounds_env: Option<&str>,
) -> (usize, u32) {
    let max_rounds = max_rounds_env
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(cfg.max_rounds);
    (cfg.max_concurrent.max(1), max_rounds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CodingAgentConfig;

    #[test]
    fn external_profiles_convert_and_guard_bypass() {
        use atomcode_capabilities::subagent::{PermissionMode, SubagentKind};
        use atomcode_config::config::ExternalSubagentConfig;
        let cfg = |name: &str, kind: &str, perm: Option<&str>, allow: bool, enabled: bool| {
            ExternalSubagentConfig {
                name: name.into(),
                kind: kind.into(),
                model: None,
                permission: perm.map(Into::into),
                allow_dangerous: allow,
                timeout_secs: None,
                enabled,
            }
        };
        let configs = vec![
            cfg("codex-ro", "codex", None, false, true), // default read-only
            cfg("cc-edit", "claude-code", Some("accept-edits"), false, true),
            cfg("codex-bypass", "codex", Some("bypass"), true, true), // wants bypass
            cfg("bad-kind", "gemini", None, false, true),             // skipped
            cfg("disabled", "codex", None, false, false),             // skipped
        ];

        // Interactive context: bypass allowed (profile opted in).
        let ctx_on = external_subagent_profiles(&configs, true);
        assert_eq!(ctx_on.len(), 3, "unknown kind + disabled are dropped");
        assert_eq!(ctx_on[0].permission, PermissionMode::ReadOnly);
        assert_eq!(ctx_on[0].kind, SubagentKind::Codex);
        assert_eq!(ctx_on[1].permission, PermissionMode::AcceptEdits);
        assert_eq!(ctx_on[2].permission, PermissionMode::Bypass);
        assert!(ctx_on[2].allow_dangerous);

        // Non-interactive context: bypass downgraded to read-only (fail-closed).
        let ctx_off = external_subagent_profiles(&configs, false);
        assert_eq!(ctx_off[2].permission, PermissionMode::ReadOnly);
        assert!(!ctx_off[2].allow_dangerous);
    }

    #[test]
    fn resolve_external_synthesizes_switches_and_explicit_wins() {
        use atomcode_capabilities::subagent::{PermissionMode, SubagentKind};
        use atomcode_config::config::{ExternalSubagentConfig, SubAgentConfig};

        // codex switch on (read-only), claude off, no explicit entries.
        let mut sub = SubAgentConfig::default();
        sub.codex = "read-only".into();
        sub.claude = "off".into();
        let profiles = resolve_external_subagents(&sub, true);
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "codex");
        assert_eq!(profiles[0].kind, SubagentKind::Codex);
        assert_eq!(profiles[0].permission, PermissionMode::ReadOnly);
        assert!(!profiles[0].allow_dangerous, "built-ins are never dangerous");

        // An explicit [[subagent.external]] named "codex" overrides the switch.
        sub.external = vec![ExternalSubagentConfig {
            name: "codex".into(),
            kind: "codex".into(),
            model: Some("gpt-5-codex".into()),
            permission: Some("accept-edits".into()),
            allow_dangerous: false,
            timeout_secs: None,
            enabled: true,
        }];
        let profiles = resolve_external_subagents(&sub, true);
        assert_eq!(profiles.len(), 1, "the built-in codex is not added on top");
        assert_eq!(profiles[0].permission, PermissionMode::AcceptEdits);
        assert_eq!(profiles[0].model.as_deref(), Some("gpt-5-codex"));

        // off + no explicit → nothing.
        let empty = resolve_external_subagents(&SubAgentConfig::default(), true);
        assert!(empty.is_empty());

        // Off-spelling level still parses (reuses from_config_str normalization).
        let mut sub = SubAgentConfig::default();
        sub.codex = "Read_Only".into();
        let p = resolve_external_subagents(&sub, true);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].permission, PermissionMode::ReadOnly);

        // An explicitly DISABLED codex entry suppresses the switch (the name is
        // reserved even though the disabled entry itself doesn't mount).
        let mut sub = SubAgentConfig::default();
        sub.codex = "read-only".into();
        sub.external = vec![ExternalSubagentConfig {
            name: "codex".into(),
            kind: "codex".into(),
            model: None,
            permission: Some("auto".into()),
            allow_dangerous: false,
            timeout_secs: None,
            enabled: false,
        }];
        let p = resolve_external_subagents(&sub, true);
        assert!(p.is_empty(), "disabled explicit codex blocks the built-in switch");
    }

    fn agent_config(model: &str) -> CodingAgentConfig {
        CodingAgentConfig::new("", "", model, ".")
    }

    struct TestMcpTool;

    #[async_trait::async_trait]
    impl atomcode_kernel::tool::Tool for TestMcpTool {
        fn name(&self) -> &str {
            "mcp__test__echo"
        }

        fn description(&self) -> &str {
            "test MCP tool"
        }

        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }

        async fn execute(
            &self,
            _args: &str,
            _ctx: &atomcode_kernel::tool::ToolContext,
        ) -> atomcode_kernel::tool::ToolResult {
            atomcode_kernel::tool::ToolResult {
                call_id: String::new(),
                content: "ok".into(),
                is_error: false,
                images: Vec::new(),
            }
        }
    }
    #[test]
    fn subagent_env_gate() {
        use super::subagent_enabled_from_env as g;
        // Default ON: unset or any non-opt-out value enables.
        assert!(g(None));
        assert!(g(Some("")));
        assert!(g(Some("1")));
        assert!(g(Some("true")));
        assert!(g(Some("yes")));
        // Only the explicit opt-out values disable.
        assert!(!g(Some("0")));
        assert!(!g(Some("false")));
        assert!(!g(Some("off")));
    }

    #[test]
    fn subagent_policy_is_driver_explicit_and_env_can_disable() {
        assert!(!SubagentPolicy::Disabled.resolve(None));
        assert!(SubagentPolicy::Enabled.resolve(None));
        assert!(!SubagentPolicy::Enabled.resolve(Some("0")));
        assert!(!SubagentPolicy::Disabled.resolve(Some("1")));
        assert!(SubagentPolicy::Enabled.resolve(Some("1")));
    }

    #[test]
    fn subagent_runtime_knobs_ignore_legacy_timeout_and_floor_concurrency() {
        use super::subagent_runtime_knobs;
        use atomcode_config::config::SubAgentConfig;
        let cfg = SubAgentConfig {
            max_concurrent: 0,
            timeout_secs: 5,
            ..SubAgentConfig::default()
        };
        let (mc, rounds) = subagent_runtime_knobs(&cfg, None);
        assert_eq!(mc, 1, "max_concurrent is still floored to one");
        assert_eq!(rounds, 200);
    }

    #[test]
    fn subagent_runtime_knobs_default_config_preserves_live_defaults() {
        use super::subagent_runtime_knobs;
        use atomcode_config::config::SubAgentConfig;
        let (mc, rounds) = subagent_runtime_knobs(&SubAgentConfig::default(), None);
        assert_eq!(mc, 3, "default max_concurrent unchanged");
        assert_eq!(rounds, 200, "default child round high-water unchanged");
    }

    #[test]
    fn subagent_round_limit_supports_override_and_explicit_unbounded() {
        use super::subagent_runtime_knobs;
        use atomcode_config::config::SubAgentConfig;
        let cfg = SubAgentConfig {
            max_rounds: 350,
            ..SubAgentConfig::default()
        };
        assert_eq!(subagent_runtime_knobs(&cfg, None).1, 350);
        assert_eq!(subagent_runtime_knobs(&cfg, Some(" 500 ")).1, 500);
        assert_eq!(
            subagent_runtime_knobs(&cfg, Some("0")).1,
            0,
            "zero is an intentional unbounded override"
        );
        assert_eq!(subagent_runtime_knobs(&cfg, Some("bad")).1, 350);
    }

    #[test]
    fn resume_adds_persona_before_legacy_session_context() {
        let mut snapshot = SessionSnapshot::new(vec![Message::system("SESSION CONTEXT")]);

        reconcile_coding_persona(
            &mut snapshot,
            &agent_config("deepseek-v4-flash"),
            true,
            true,
            true,
            true,
            false,
        );

        assert!(snapshot.messages[0]
            .text
            .contains("running the deepseek-v4-flash model"));
        assert_eq!(snapshot.messages[1].text, "SESSION CONTEXT");
        assert_eq!(snapshot.cache_epoch, 1);
    }

    #[test]
    fn provider_reassemble_keeps_the_prepared_todo_capability_gate() {
        let mut snapshot = SessionSnapshot::new(vec![Message::system("SESSION CONTEXT")]);
        let cfg = agent_config("deepseek-v4-flash");

        reconcile_coding_persona(&mut snapshot, &cfg, false, true, true, true, false);

        assert!(!snapshot.messages[0].text.contains("## TASK TRACKING"));
        assert!(snapshot.messages[0]
            .text
            .contains("running the deepseek-v4-flash model"));
    }

    #[test]
    #[serial_test::serial(offline_verdict)]
    fn model_switch_replaces_persona_without_duplication() {
        atomcode_config::config::offline::reset_offline_verdict_for_test();
        // Remove ATOMCODE_REQUEST_USER_INPUT so the persona is deterministic regardless
        // of what other tests may have set concurrently (we hold the serial lock, so this
        // is safe — no other test in this serial group can observe the removal).
        let _rui_guard = std::env::remove_var("ATOMCODE_REQUEST_USER_INPUT");
        let mut snapshot = SessionSnapshot::new(vec![
            Message::system(coding_persona(
                "old-model",
                crate::persona::todo_switch_enabled(),
                crate::persona::request_user_input_switch_enabled(),
            )),
            Message::system("SESSION CONTEXT"),
        ]);

        reconcile_coding_persona(
            &mut snapshot,
            &agent_config("deepseek-v4-flash"),
            true,
            true,
            true,
            true,
            false,
        );

        let personas = snapshot
            .messages
            .iter()
            .filter(|message| message.text.starts_with(ATOMCODE_PERSONA_PREFIX))
            .count();
        assert_eq!(personas, 1);
        assert!(snapshot.messages[0]
            .text
            .contains("running the deepseek-v4-flash model"));
        assert!(!snapshot.messages[0].text.contains("old-model"));
        assert!(!snapshot
            .messages
            .iter()
            .any(|message| message.text.starts_with(MODEL_CHANGE_CONTEXT_PREFIX)));
        assert_eq!(snapshot.cache_epoch, 1);
    }

    #[test]
    #[serial_test::serial(offline_verdict)]
    fn repeated_model_switch_keeps_system_context_leading() {
        atomcode_config::config::offline::reset_offline_verdict_for_test();
        let _rui_guard = std::env::remove_var("ATOMCODE_REQUEST_USER_INPUT");
        let mut snapshot = SessionSnapshot::new(vec![
            Message::system(coding_persona(
                "model-a",
                crate::persona::todo_switch_enabled(),
                crate::persona::request_user_input_switch_enabled(),
            )),
            Message::system("=== SESSION CONTEXT ===\nworkspace state"),
            Message::user("what model are you?"),
            Message::assistant("I am model-a", vec![]),
        ]);

        reconcile_coding_persona(
            &mut snapshot,
            &agent_config("model-b"),
            true,
            true,
            true,
            true,
            false,
        );
        reconcile_coding_persona(
            &mut snapshot,
            &agent_config("model-c"),
            true,
            true,
            true,
            true,
            false,
        );

        assert!(snapshot.messages[0]
            .text
            .contains("running the model-c model"));
        assert_eq!(
            snapshot.messages[1].text,
            "=== SESSION CONTEXT ===\nworkspace state"
        );
        let first_non_system = snapshot
            .messages
            .iter()
            .position(|message| message.role != Role::System)
            .expect("user history remains after the leading system block");
        assert_eq!(first_non_system, 2);
        assert!(snapshot.messages[first_non_system..]
            .iter()
            .all(|message| message.role != Role::System));
        assert_eq!(snapshot.messages[first_non_system].text, "what model are you?");
        assert_eq!(snapshot.messages[first_non_system + 1].text, "I am model-a");
        assert!(!snapshot
            .messages
            .iter()
            .any(|message| message.text.starts_with(MODEL_CHANGE_CONTEXT_PREFIX)));
    }

    #[test]
    #[serial_test::serial(offline_verdict)]
    fn current_persona_keeps_snapshot_byte_stable() {
        atomcode_config::config::offline::reset_offline_verdict_for_test();
        // Remove ATOMCODE_REQUEST_USER_INPUT so the persona is stable for both builds of
        // the persona string (captured and reconciled).  We hold the serial lock, so this
        // is safe.
        let _rui_guard = std::env::remove_var("ATOMCODE_REQUEST_USER_INPUT");
        let persona = coding_persona(
            "deepseek-v4-flash",
            crate::persona::todo_switch_enabled(),
            crate::persona::request_user_input_switch_enabled(),
        );
        let mut snapshot = SessionSnapshot::new(vec![
            Message::system(persona.clone()),
            Message::system("SESSION CONTEXT"),
        ]);

        reconcile_coding_persona(
            &mut snapshot,
            &agent_config("deepseek-v4-flash"),
            true,
            true,
            true,
            true,
            false,
        );

        assert_eq!(snapshot.messages[0].text, persona);
        assert_eq!(snapshot.cache_epoch, 0);
    }

    #[test]
    #[serial_test::serial(offline_verdict)]
    fn language_switch_refreshes_persona_without_model_change_boundary() {
        use atomcode_config::locale::Locale;

        atomcode_config::config::offline::reset_offline_verdict_for_test();
        let _rui_guard = std::env::remove_var("ATOMCODE_REQUEST_USER_INPUT");
        let mut snapshot = SessionSnapshot::new(vec![Message::system(
            crate::persona::coding_persona_with_language(
                "model-a",
                Some(Locale::En),
                crate::persona::todo_switch_enabled(),
                crate::persona::request_user_input_switch_enabled(),
            ),
        )]);
        let mut cfg = agent_config("model-a");
        cfg.preferred_language = Some(Locale::ZhCn);

        reconcile_coding_persona(&mut snapshot, &cfg, true, true, true, true, false);

        assert!(snapshot.messages[0]
            .text
            .contains("subject and body in Simplified Chinese"));
        assert!(!snapshot
            .messages
            .iter()
            .any(|message| message.text.starts_with(MODEL_CHANGE_CONTEXT_PREFIX)));
        assert_eq!(snapshot.cache_epoch, 1);
    }

    #[test]
    #[serial_test::serial(offline_verdict)]
    fn language_switch_removes_legacy_model_change_boundary() {
        use atomcode_config::locale::Locale;

        atomcode_config::config::offline::reset_offline_verdict_for_test();
        let _rui_guard = std::env::remove_var("ATOMCODE_REQUEST_USER_INPUT");
        let mut snapshot = SessionSnapshot::new(vec![
            Message::system(coding_persona(
                "model-a",
                crate::persona::todo_switch_enabled(),
                crate::persona::request_user_input_switch_enabled(),
            )),
            Message::assistant("I am model-a", vec![]),
        ]);
        reconcile_coding_persona(&mut snapshot, &agent_config("model-b"), true, true, true, true, false);
        snapshot.messages.push(Message::system(format!(
            "{MODEL_CHANGE_CONTEXT_PREFIX}\nlegacy transition"
        )));
        let mut cfg = agent_config("model-b");
        cfg.preferred_language = Some(Locale::ZhCn);

        reconcile_coding_persona(&mut snapshot, &cfg, true, true, true, true, false);

        assert!(!snapshot
            .messages
            .iter()
            .any(|message| message.text.starts_with(MODEL_CHANGE_CONTEXT_PREFIX)));
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn prepare_does_not_wait_for_mcp_network_readiness() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        #[cfg(unix)]
        let (command, args) = ("sh", vec!["-c", "sleep 5"]);
        #[cfg(windows)]
        let (command, args) = ("cmd", vec!["/C", "ping -n 6 127.0.0.1 >NUL"]);
        std::fs::write(
            home.path().join("mcp.json"),
            serde_json::to_vec(&serde_json::json!({
                "mcpServers": {
                    "never-ready": {
                        "command": command,
                        "args": args,
                        "timeout_ms": 5000
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let opts = PrepareOptions {
            session: SessionMode::Disabled,
            skill_dirs: Some(vec![]),
            plugin_skill_dirs: Vec::new(),
            mcp: true,
            extra_mcp_servers: Vec::new(),
            external_subagents: Vec::new(),
            memory: false,
            web: false,
            review: false,
            subagents: SubagentPolicy::Disabled,
            request_user_input: true,
            rate_limit_source: None,
        };

        let prepared =
            tokio::time::timeout(std::time::Duration::from_millis(250), prepare(&cfg, opts)).await;

        assert!(
            prepared.is_ok(),
            "MCP readiness must not block the session candidate prepare path"
        );
        assert!(prepared.unwrap().is_ok());
    }

    #[tokio::test]
    async fn capability_reload_withdraws_old_mcp_tools_fail_closed() {
        let project = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let mut parts = prepare(&cfg, io_free_opts()).await.unwrap();
        parts.registry.register(Arc::new(TestMcpTool));
        parts
            .mcp_tool_names
            .write()
            .unwrap()
            .push("mcp__test__echo".into());
        let mounted = parts.mount();
        assert!(mounted.get("mcp__test__echo").is_some());

        parts.withdraw_mcp_tools().await;

        assert!(mounted.get("mcp__test__echo").is_none());
        assert!(parts.mcp_tool_names.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn mcp_tools_for_server_uses_exact_alias_ownership() {
        let project = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let mut parts = prepare(&cfg, io_free_opts()).await.unwrap();
        let registry = Arc::new(McpRegistry::new());
        let info = atomcode_capabilities::mcp::McpToolInfo {
            server_name: "docs space".into(),
            tool_name: "read.file".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
            read_only: false,
        };
        let adapter =
            atomcode_capabilities::mcp::McpToolAdapter::new(registry.clone(), info).unwrap();
        let alias = atomcode_kernel::tool::Tool::name(&adapter).to_string();
        parts.mcp_registry = Some(registry);
        parts.mcp_tool_names.write().unwrap().push(alias.clone());

        assert_eq!(parts.mcp_tools_for_server("docs space"), vec![alias]);
        assert!(parts.mcp_tools_for_server("docs-space").is_empty());
    }

    /// `prepare` with all optional capabilities OFF — keeps the call I/O-free (no MCP
    /// connect, no session/skill/home scans) so the test only exercises CC-hook wiring.
    fn io_free_opts() -> PrepareOptions {
        PrepareOptions {
            session: SessionMode::Disabled,
            skill_dirs: Some(vec![]),
            plugin_skill_dirs: Vec::new(),
            mcp: false,
            extra_mcp_servers: Vec::new(),
            external_subagents: Vec::new(),
            memory: false,
            web: false,
            review: false,
            subagents: SubagentPolicy::Disabled,
            request_user_input: true,
            rate_limit_source: None,
        }
    }

    #[tokio::test]
    async fn driver_can_disable_request_user_input_at_mount_boundary() {
        let project = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let mut opts = io_free_opts();
        opts.request_user_input = false;
        let parts = prepare(&cfg, opts).await.unwrap();

        assert!(!parts
            .selected_tool_names()
            .iter()
            .any(|name| name == "request_user_input"));
        assert!(!parts.request_user_input_enabled);
    }

    #[cfg(feature = "atomgit")]
    #[tokio::test]
    async fn production_prepare_exposes_atomgit_tools() {
        let project = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let parts = prepare(&cfg, io_free_opts()).await.unwrap();
        let names = parts.selected_tool_names();

        for expected in ["atomgit_repo", "atomgit_pr", "atomgit_issue"] {
            assert!(
                names.iter().any(|name| name == expected),
                "production tool catalog must expose {expected}: {names:?}"
            );
        }
    }

    async fn resume_prepare_error(cfg: &CodingAgentConfig, id: &str) -> io::Error {
        let mut opts = io_free_opts();
        opts.session = SessionMode::Resume(id.to_string());
        match prepare(cfg, opts).await {
            Ok(_) => panic!("resume must reject invalid native aggregate for {id}"),
            Err(error) => error,
        }
    }

    fn session_store_error(
        error: &io::Error,
    ) -> &atomcode_capabilities::session::SessionStoreError {
        error
            .get_ref()
            .and_then(|source| {
                source.downcast_ref::<atomcode_capabilities::session::SessionStoreError>()
            })
            .expect("prepare error must preserve the session store cause")
    }

    fn persist_native_session(
        manager: &SessionManager,
        id: &str,
        working_dir: &std::path::Path,
        snapshot: &SessionSnapshot,
    ) {
        let lease = manager.acquire_lease(id).unwrap();
        let mut meta = SessionMeta::new(id, working_dir.to_string_lossy(), 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = u32::try_from(snapshot.messages.len()).unwrap();
        manager
            .commit_native_import(
                &lease,
                Some(snapshot),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();
    }

    async fn external_snapshot_prepare_error(
        cfg: &CodingAgentConfig,
        id: &str,
        snapshot: SessionSnapshot,
    ) -> io::Error {
        let mut opts = io_free_opts();
        opts.session = SessionMode::ExternalSnapshot {
            id: id.to_string(),
            snapshot,
        };
        match prepare(cfg, opts).await {
            Ok(_) => panic!("external snapshot must reject invalid native aggregate for {id}"),
            Err(error) => error,
        }
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn resume_requires_a_complete_native_session_aggregate() {
        use atomcode_capabilities::session::SessionStoreError;

        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let manager = SessionManager::for_project(project.path());
        let snapshot = SessionSnapshot::new(vec![Message::user("persisted")]);
        let presentation = PresentationFile::default();

        manager.save_snapshot("missing-meta", &snapshot).unwrap();
        manager
            .write_presentation("missing-meta", &presentation)
            .unwrap();
        let error = resume_prepare_error(&cfg, "missing-meta").await;
        assert!(matches!(
            session_store_error(&error),
            SessionStoreError::NotFound { path }
                if path == &manager.meta_path("missing-meta").unwrap()
        ));

        let mut missing_snapshot =
            SessionMeta::new("missing-snapshot", project.path().to_string_lossy(), 1);
        missing_snapshot.owner = StorageOwner::Native;
        manager.write_meta(&missing_snapshot).unwrap();
        manager
            .write_presentation("missing-snapshot", &presentation)
            .unwrap();
        let error = resume_prepare_error(&cfg, "missing-snapshot").await;
        assert!(matches!(
            session_store_error(&error),
            SessionStoreError::NotFound { path }
                if path == &manager.snapshot_path("missing-snapshot").unwrap()
        ));

        let mut missing_presentation =
            SessionMeta::new("missing-presentation", project.path().to_string_lossy(), 1);
        missing_presentation.owner = StorageOwner::Native;
        manager.write_meta(&missing_presentation).unwrap();
        manager
            .save_snapshot("missing-presentation", &snapshot)
            .unwrap();
        let error = resume_prepare_error(&cfg, "missing-presentation").await;
        assert!(matches!(
            session_store_error(&error),
            SessionStoreError::NotFound { path }
                if path == &manager.presentation_path("missing-presentation").unwrap()
        ));

        for (id, owner) in [
            ("unconfirmed-owner", StorageOwner::Unconfirmed),
            ("legacy-owner", StorageOwner::Legacy),
        ] {
            manager.save_snapshot(id, &snapshot).unwrap();
            manager.write_presentation(id, &presentation).unwrap();
            let mut meta = SessionMeta::new(id, project.path().to_string_lossy(), 1);
            meta.owner = owner.clone();
            manager.write_meta(&meta).unwrap();

            let error = resume_prepare_error(&cfg, id).await;
            assert!(matches!(
                session_store_error(&error),
                SessionStoreError::OwnershipConflict {
                    id: actual_id,
                    owner: actual_owner,
                    operation: "load native session",
                } if actual_id == id && actual_owner == &owner
            ));
        }
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn session_bound_reassemble_rejects_an_incomplete_native_aggregate() {
        use atomcode_capabilities::session::SessionStoreError;

        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let manager = SessionManager::for_project(project.path());
        let id = "incomplete-reassemble";
        let snapshot = SessionSnapshot::new(vec![Message::user("persisted")]);
        persist_native_session(&manager, id, project.path(), &snapshot);

        let mut opts = io_free_opts();
        opts.session = SessionMode::Resume(id.into());
        let mut parts = prepare(&cfg, opts).await.unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);
        drop(assemble(&mut parts, &cfg, provider.clone()).unwrap());
        std::fs::remove_file(manager.presentation_path(id).unwrap()).unwrap();

        let error = match assemble(&mut parts, &cfg, provider) {
            Ok(_) => panic!("reassemble must reject an incomplete native aggregate"),
            Err(error) => error,
        };
        assert!(matches!(
            session_store_error(&error),
            SessionStoreError::NotFound { path }
                if path == &manager.presentation_path(id).unwrap()
        ));
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn external_snapshot_requires_a_complete_native_session_aggregate() {
        use atomcode_capabilities::session::SessionStoreError;

        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let manager = SessionManager::for_project(project.path());

        let error = external_snapshot_prepare_error(
            &cfg,
            "missing-native",
            SessionSnapshot::new(vec![Message::user("external")]),
        )
        .await;
        assert!(matches!(
            session_store_error(&error),
            SessionStoreError::NotFound { path }
                if path == &manager.meta_path("missing-native").unwrap()
        ));
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn external_snapshot_must_match_the_canonical_native_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let manager = SessionManager::for_project(project.path());
        let id = "divergent-external";
        let canonical = SessionSnapshot::new(vec![Message::user("canonical")]);
        persist_native_session(&manager, id, project.path(), &canonical);

        let error = external_snapshot_prepare_error(
            &cfg,
            id,
            SessionSnapshot::new(vec![Message::user("stale external")]),
        )
        .await;
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error
            .to_string()
            .contains("does not match the canonical native snapshot"));
        assert_eq!(manager.load_snapshot(id).unwrap(), canonical);
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn external_snapshot_accepts_a_matching_complete_native_aggregate() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let manager = SessionManager::for_project(project.path());
        let id = "matching-external";
        let canonical = SessionSnapshot::new(vec![Message::user("canonical")]);
        persist_native_session(&manager, id, project.path(), &canonical);

        let mut opts = io_free_opts();
        opts.session = SessionMode::ExternalSnapshot {
            id: id.into(),
            snapshot: canonical.clone(),
        };
        let parts = prepare(&cfg, opts).await.unwrap();
        assert_eq!(parts.session.unwrap().resume.as_ref(), Some(&canonical));
    }

    #[tokio::test]
    async fn capability_reprepare_inherits_runtime_continuity_handles() {
        let project = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let previous = prepare(&cfg, io_free_opts()).await.unwrap();
        previous
            .plan_mode
            .store(true, std::sync::atomic::Ordering::Release);
        previous.write_approval_grants.grant("edit_file");

        let mut candidate = prepare(&cfg, io_free_opts()).await.unwrap();
        assert!(!Arc::ptr_eq(&candidate.approval, &previous.approval));
        assert!(!Arc::ptr_eq(
            &candidate.write_approval_grants,
            &previous.write_approval_grants,
        ));

        candidate.inherit_runtime_continuity(&previous);

        assert!(Arc::ptr_eq(&candidate.approval, &previous.approval));
        assert!(Arc::ptr_eq(
            &candidate.mcp_plan_grants,
            &previous.mcp_plan_grants,
        ));
        assert!(Arc::ptr_eq(
            &candidate.write_approval_grants,
            &previous.write_approval_grants,
        ));
        assert!(Arc::ptr_eq(
            &candidate.bash_workspace_grants,
            &previous.bash_workspace_grants,
        ));
        assert!(Arc::ptr_eq(
            &candidate.sensitive_path_grants,
            &previous.sensitive_path_grants,
        ));
        assert!(Arc::ptr_eq(
            &candidate.credential_shell_grants,
            &previous.credential_shell_grants,
        ));
        assert!(candidate
            .plan_mode
            .load(std::sync::atomic::Ordering::Acquire));
        assert!(candidate.write_approval_grants.is_granted("edit_file"));
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn prepare_injects_checkpoint_only_for_persistent_sessions() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());

        let mut persistent = io_free_opts();
        persistent.session = SessionMode::Fresh;
        let parts = prepare(&cfg, persistent).await.unwrap();
        assert!(parts.compaction_checkpoint.is_some());
        let binding = parts.session.as_ref().unwrap();
        assert_eq!(
            binding.manager.read_meta(&binding.id).unwrap().owner,
            StorageOwner::Native
        );
        assert!(binding.manager.load_snapshot(&binding.id).is_ok());

        let ephemeral = prepare(&cfg, io_free_opts()).await.unwrap();
        assert!(ephemeral.compaction_checkpoint.is_none());
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn runtime_prepare_keeps_fresh_session_staged_until_publish() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let mut opts = io_free_opts();
        opts.session = SessionMode::Fresh;

        let mut parts = prepare_with_plugin_hooks_reusing_lease(&cfg, opts, Vec::new(), None, true)
            .await
            .unwrap();
        let binding = parts.session.as_ref().unwrap();
        assert!(binding.manager.read_meta(&binding.id).is_err());

        parts.publish_staged_session().unwrap();
        let binding = parts.session.as_ref().unwrap();
        assert_eq!(
            binding.manager.read_meta(&binding.id).unwrap().owner,
            StorageOwner::Native
        );
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn prepare_rejects_a_second_binding_until_the_first_drops() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let snapshot = SessionSnapshot::new(vec![Message::user("persisted")]);
        let manager = SessionManager::for_project(project.path());
        persist_native_session(&manager, "same-session", project.path(), &snapshot);
        let opts = || {
            let mut opts = io_free_opts();
            opts.session = SessionMode::ExternalSnapshot {
                id: "same-session".into(),
                snapshot: snapshot.clone(),
            };
            opts
        };

        let first = prepare(&cfg, opts()).await.unwrap();
        let error = match prepare(&cfg, opts()).await {
            Ok(_) => panic!("a second binding must not own the same session"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(matches!(
            error
                .get_ref()
                .and_then(|source| source.downcast_ref::<
                    atomcode_capabilities::session::SessionStoreError,
                >()),
            Some(atomcode_capabilities::session::SessionStoreError::SessionInUse {
                id,
                ..
            }) if id == "same-session"
        ));

        drop(first);
        prepare(&cfg, opts()).await.unwrap();
    }

    /// `prepare` loads a project `.hooks.json` and exposes the runner via
    /// `cc_external_hooks` (the handle `assemble` registers as a ToolMiddleware) AND
    /// pushes it onto the lifecycle `hooks`. With no hooks file, neither is registered —
    /// the zero-overhead common path. ATOMCODE_HOME is pinned to an empty temp dir so the
    /// user-level lookup can't pick up a real `~/.atomcode/hooks.json` on the dev box.
    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn prepare_wires_cc_external_hooks_only_when_present() {
        let home = tempfile::tempdir().unwrap(); // empty → no user-level hooks
        std::env::set_var("ATOMCODE_HOME", home.path());

        // No project hooks → nothing wired.
        let bare = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", bare.path());
        let parts = prepare(&cfg, io_free_opts()).await.unwrap();
        assert!(
            parts.cc_external_hooks.is_none(),
            "no hooks.json ⇒ nothing registered"
        );
        let baseline_hooks = parts.hooks.len();

        // Project .hooks.json present → wired as the middleware handle AND a lifecycle hook.
        let proj = tempfile::tempdir().unwrap();
        std::fs::write(
            proj.path().join(".hooks.json"),
            r#"{"hooks":{"a":{"event":"PreToolUse","matcher":"bash","command":"echo hi"}}}"#,
        )
        .unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", proj.path());
        let parts = prepare(&cfg, io_free_opts()).await.unwrap();
        assert!(
            parts.cc_external_hooks.is_some(),
            "project .hooks.json ⇒ wired"
        );
        assert_eq!(
            parts.hooks.len(),
            baseline_hooks + 1,
            "the CC runner is also pushed onto the lifecycle hook chain"
        );
    }

    /// A canned provider that reports usage then ends — enough for a telemetry
    /// decorator to fold a `TokenUsage` and emit one `LlmChat`.
    struct CannedProvider;
    #[async_trait::async_trait]
    impl LlmProvider for CannedProvider {
        fn model_name(&self) -> &str {
            "m"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[atomcode_kernel::tool::ToolDef],
            _: &atomcode_kernel::provider::ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            use atomcode_kernel::stream::{StreamEvent, TokenUsage};
            let evs = vec![
                StreamEvent::TextDelta("looks good".into()),
                StreamEvent::Usage(TokenUsage {
                    prompt: 500,
                    completion: 30,
                    cached: 0,
                }),
                StreamEvent::Done { truncated: false },
            ];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
    }

    /// The `code_review` sub-agent runs its OWN kernel loop with no turn-level
    /// `TelemetryHook`, so its LLM rounds bypass the host's metering entirely. To keep
    /// review token spend visible, the provider placed in the review slot at `assemble`
    /// must be a metered decorator (when a telemetry sink is configured). This drives one
    /// round through that provider and asserts the `LlmChat` lands.
    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn review_subagent_provider_is_metered_for_token_telemetry() {
        use futures::stream::StreamExt;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());

        let (tel, captured) = atomcode_telemetry::Telemetry::in_memory("test".into());
        let proj = tempfile::tempdir().unwrap();
        let mut cfg = CodingAgentConfig::new("k", "http://localhost", "m", proj.path());
        cfg.telemetry = Some(tel);

        let mut opts = io_free_opts();
        opts.review = true;
        let mut parts = prepare(&cfg, opts).await.unwrap();

        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);
        let _agent = assemble(&mut parts, &cfg, provider).unwrap();

        let slot = parts
            .review_provider
            .clone()
            .expect("review enabled ⇒ slot present");
        let review_provider = slot
            .read()
            .unwrap()
            .clone()
            .expect("slot filled at assemble");
        let mut stream = review_provider
            .chat_stream(
                &[Message::user("review this")],
                &[],
                &atomcode_kernel::provider::ChatOptions::default(),
            )
            .await
            .unwrap();
        while stream.next().await.is_some() {}

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let llm_chats = captured
            .lock()
            .await
            .iter()
            .filter(|r| matches!(r.event, atomcode_telemetry::Event::LlmChat { .. }))
            .count();
        assert_eq!(
            llm_chats, 1,
            "review sub-agent LLM round must emit one LlmChat token event"
        );
    }

    /// A `/login` or `/model` swap updates `cfg.model` and re-runs `assemble` ONLY
    /// (never `prepare`). The primary turn-level `TelemetryHook` must therefore be
    /// (re)built at `assemble` so its `LlmChat` envelope reports the CURRENTLY active
    /// model — not the value frozen at `prepare`. Regression guard for the v2 bug where
    /// a session that launched with no resolvable provider (model="" + the openai host
    /// default `api.openai.com`) kept mis-attributing every real post-login round.
    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn primary_telemetry_hook_tracks_model_swapped_at_assemble() {
        use atomcode_kernel::agent::AutoRespond;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());

        let (tel, captured) = atomcode_telemetry::Telemetry::in_memory("test".into());
        let proj = tempfile::tempdir().unwrap();
        // Launched in onboarding mode: no resolvable provider ⇒ empty model at prepare.
        let mut cfg = CodingAgentConfig::new("k", "http://localhost", "", proj.path());
        cfg.telemetry = Some(tel);

        let mut parts = prepare(&cfg, io_free_opts()).await.unwrap();

        // /login resolves the real provider: cfg picks up the model, assemble re-runs.
        cfg.model = "swapped-model".to_string();
        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);
        let agent = assemble(&mut parts, &cfg, provider).unwrap();

        let _ = agent.run_to_completion("hi", AutoRespond::AllowAll).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        let chat = records
            .iter()
            .find(|r| matches!(r.event, atomcode_telemetry::Event::LlmChat { .. }))
            .expect("the primary turn must emit one LlmChat");
        assert_eq!(
            chat.envelope.model.as_deref(),
            Some("swapped-model"),
            "primary TelemetryHook must report the model active at assemble, not the prepare-time one"
        );
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn primary_runtime_mounts_configured_datalog_at_assemble() {
        use atomcode_kernel::agent::AutoRespond;

        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let project = tempfile::tempdir().unwrap();
        let datalog_root = home.path().join("custom-datalog");
        let mut cfg =
            CodingAgentConfig::new("k", "http://localhost", "logged-model", project.path());
        cfg.datalog.enabled = true;
        cfg.datalog.dir = Some(datalog_root.display().to_string());

        let mut parts = prepare(&cfg, io_free_opts()).await.unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);
        let agent = assemble(&mut parts, &cfg, provider).unwrap();
        let _ = agent
            .run_to_completion("record this turn", AutoRespond::AllowAll)
            .await;

        let project_dir = std::fs::read_dir(&datalog_root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let files: Vec<_> = std::fs::read_dir(project_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        let markdown = files
            .iter()
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("md"))
            .expect("turn markdown");
        let jsonl = files
            .iter()
            .find(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
            .expect("per-round request jsonl");
        assert!(std::fs::read_to_string(markdown)
            .unwrap()
            .contains("**Response:**\nlooks good"));
        let request = std::fs::read_to_string(jsonl).unwrap();
        assert!(request.contains("\"model\":\"logged-model\""));
        assert!(request.contains("record this turn"));
    }

    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn snapshot_cost_attribution_tracks_model_swapped_at_assemble() {
        use atomcode_kernel::agent::AutoRespond;

        let home = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let project = tempfile::tempdir().unwrap();
        let mut cfg = CodingAgentConfig::new("k", "http://localhost", "model-a", project.path());
        cfg.provider_name = "provider-a".into();

        let mut opts = io_free_opts();
        opts.session = SessionMode::Fresh;
        let mut parts = prepare(&cfg, opts).await.unwrap();
        let binding = parts.session.as_ref().unwrap();
        let manager = binding.manager.clone();
        let session_id = binding.id.clone();

        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);
        let first = assemble(&mut parts, &cfg, provider.clone()).unwrap();
        parts.publish_staged_session().unwrap();
        let _ = first
            .run_to_completion("first", AutoRespond::AllowAll)
            .await;

        cfg.provider_name = "provider-b".into();
        cfg.model = "model-b".into();
        let second = assemble(&mut parts, &cfg, provider).unwrap();
        let _ = second
            .run_to_completion("second", AutoRespond::AllowAll)
            .await;

        let report = atomcode_capabilities::session::aggregate_session_cost(
            &manager.read_meta(&session_id).unwrap(),
        );
        assert_eq!(report.models.len(), 2);
        assert_eq!(report.models[0].provider_id, "provider-a");
        assert_eq!(report.models[0].model_id, "model-a");
        assert_eq!(report.models[1].provider_id, "provider-b");
        assert_eq!(report.models[1].model_id, "model-b");
    }

    /// Helper: run `prepare` with `opts.web = web_enabled` (all other optional capabilities
    /// OFF so the call is I/O-free) and return the registered tool names.
    async fn tool_names_for_test(web_enabled: bool) -> Vec<String> {
        let project = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let mut opts = io_free_opts();
        opts.web = web_enabled;
        let parts = prepare(&cfg, opts).await.unwrap();
        parts.tool_names.clone()
    }

    /// Offline mode must drop `web_fetch` and `web_search` from the coding-path tool
    /// registry even when `opts.web` is `true` (the normal production setting).
    #[tokio::test]
    #[serial_test::serial(offline_verdict)]
    async fn offline_removes_web_tools_from_coding_registry() {
        use atomcode_config::config::offline::{
            reset_offline_verdict_for_test, seed_offline_verdict, OfflineMode,
        };
        reset_offline_verdict_for_test();
        seed_offline_verdict(OfflineMode::On, None);

        let names = tool_names_for_test(true).await;
        assert!(
            !names.contains(&"web_fetch".to_string()),
            "web_fetch must be absent when offline; got: {names:?}"
        );
        assert!(
            !names.contains(&"web_search".to_string()),
            "web_search must be absent when offline; got: {names:?}"
        );

        reset_offline_verdict_for_test();
    }

    /// When online (the default), `opts.web = true` must still register both web tools
    /// (0-intrusion: behaviour is byte-identical to before this feature).
    #[tokio::test]
    #[serial_test::serial(offline_verdict)]
    async fn online_keeps_web_tools() {
        use atomcode_config::config::offline::{
            reset_offline_verdict_for_test, seed_offline_verdict, OfflineMode,
        };
        reset_offline_verdict_for_test();
        seed_offline_verdict(OfflineMode::Off, None);

        let names = tool_names_for_test(true).await;
        assert!(
            names.contains(&"web_fetch".to_string()),
            "web_fetch must be present when online; got: {names:?}"
        );
        assert!(
            names.contains(&"web_search".to_string()),
            "web_search must be present when online; got: {names:?}"
        );

        reset_offline_verdict_for_test();
    }

    /// Unit-test the artifact wiring as `assemble` would build it:
    /// big tool result → preview+handle stored → fetch_output retrieves full bytes.
    #[tokio::test]
    async fn artifact_wiring_store_middleware_and_fetch_roundtrip() {
        use atomcode_capabilities::tools::{
            ArtifactMiddleware, ArtifactStore, FetchOutputTool, THRESHOLD_BYTES,
        };
        use atomcode_kernel::middleware::ToolMiddleware;
        use atomcode_kernel::tool::Tool as _;
        use atomcode_kernel::tool::{ToolContext, ToolResult};
        use tokio_util::sync::CancellationToken;

        let artifacts_tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(ArtifactStore::new(artifacts_tmp.path()));
        let mw = ArtifactMiddleware::new(store.clone());
        let fetch = FetchOutputTool::new(store.clone());

        // (a) a big result gets spilled and the content is replaced with a preview+handle
        let big = "Z".repeat(THRESHOLD_BYTES + 1);
        let mut result = ToolResult {
            call_id: "t1".into(),
            content: big.clone(),
            is_error: false,
            images: vec![],
        };
        mw.after(&mut result).await;
        assert!(
            result.content.len() < big.len(),
            "middleware must shorten the content"
        );
        assert!(
            result.content.contains("fetch_output"),
            "middleware must embed fetch_output hint"
        );

        // (b) artifact file exists on disk
        let id = atomcode_capabilities::tools::artifact_id(big.as_bytes());
        let artifact_path = artifacts_tmp.path().join(&id);
        assert!(
            artifact_path.exists(),
            "artifact file must be present at {artifact_path:?}"
        );

        // (c) fetch_output retrieves the full bytes
        let ctx = ToolContext {
            working_dir: artifacts_tmp.path().to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        };
        let fetch_result = fetch
            .execute(
                &format!(
                    "{{\"artifact_id\":\"{id}\",\"offset\":0,\"limit\":{}}}",
                    big.len()
                ),
                &ctx,
            )
            .await;
        assert!(
            !fetch_result.is_error,
            "fetch_output must succeed: {}",
            fetch_result.content
        );
        assert!(
            fetch_result.content.starts_with('Z'),
            "fetched bytes must start with the original content"
        );
    }

    /// When no session is present, fetch_output must NOT appear in the assembled tools.
    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn no_session_means_no_fetch_output_tool() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        // Disabled session: no artifact store, so fetch_output must not be mounted.
        let mut parts = prepare(&cfg, io_free_opts()).await.unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);
        let _agent = assemble(&mut parts, &cfg, provider).unwrap();
        // After assemble(), parts.mounted_tools is populated by the mount() call inside.
        let mounted = parts
            .mounted_tools
            .as_ref()
            .expect("mounted_tools must be set after assemble");
        assert!(
            mounted.get("fetch_output").is_none(),
            "fetch_output must not be mounted when no session is present"
        );
    }

    /// When a session IS present, fetch_output must appear in the assembled tools.
    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn session_presence_mounts_fetch_output_tool() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let mut opts = io_free_opts();
        opts.session = SessionMode::Fresh;
        let mut parts = prepare(&cfg, opts).await.unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);
        let _agent = assemble(&mut parts, &cfg, provider).unwrap();
        let mounted = parts
            .mounted_tools
            .as_ref()
            .expect("mounted_tools must be set after assemble");
        assert!(
            mounted.get("fetch_output").is_some(),
            "fetch_output must be mounted when a session is present"
        );
    }
}
