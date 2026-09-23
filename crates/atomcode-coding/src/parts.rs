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
use atomcode_capabilities::mcp::{McpConnectEvent, McpRegistry, McpServerConfig};
use atomcode_capabilities::session::snapshot::SnapshotPersistenceStatus;
use atomcode_capabilities::session::{
    ListSessionsTool, RecallTool, SessionContextHook, SessionLease, SessionManager, SessionMeta,
    SnapshotHook, StorageOwner,
};
use atomcode_capabilities::skills::{register_skill_tools, runtime_skill_dirs, SkillRegistry};
use atomcode_capabilities::tools::{
    register_coding_tools_with_vision, ApprovalMiddleware, WebFetchTool, WebSearchTool,
};
use atomcode_kernel::message::{Message, SessionSnapshot};
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::session::SessionHeader;
use atomcode_kernel::tool::ToolRegistry;
use atomcode_review::{ReviewTool, ReviewToolConfig, SharedReviewProvider};

use crate::config::CodingAgentConfig;
use crate::execution_policy::TurnExecutionPolicy;
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
    /// Whether this driver exposes any tools to the model. `false` keeps the
    /// normal coding runtime/provider lifecycle but publishes an empty tool
    /// catalog. Headless eval drivers use this to measure model-only behavior.
    pub tools: bool,
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
    /// A front end outside the App, fed from every App the runtime builds
    /// (`crate::front_end`). `None` when the driver reads the runtime's own
    /// events instead.
    pub front_end: Option<Arc<crate::front_end::FrontEnd>>,
}

impl Default for PrepareOptions {
    fn default() -> Self {
        Self {
            session: SessionMode::Fresh,
            tools: true,
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
            front_end: None,
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
    let permission = PermissionMode::from_config_str(level).filter(|p| !p.is_dangerous())?;
    Some(ExternalSubagentProfile {
        name: name.to_string(),
        kind,
        model: None,
        permission,
        allow_dangerous: false,
        timeout: None,
    })
}

/// Derive the DRIVER-NEUTRAL half of a production [`PrepareOptions`] from a
/// [`CodingRuntimeConfig`]: the full-capability defaults (`tools`/`memory`/
/// `web`/`review`/`request_user_input` on), MCP per the config, in-process
/// delegation enabled, and external-agent subagents resolved from
/// `[subagent]` — including the `claude`/`codex` convenience switches and
/// `[[subagent.external]]` entries. Dangerous (`bypass`) profiles follow
/// `cfg.interactive` (the same rule the CLI headless path applies).
///
/// Every spawn site (CLI startup, daemon, the TUI's deferred respawns) MUST
/// build its `PrepareOptions` from this and then override only what makes its
/// driver different (session mode, `front_end`, `no_tools`, plugin skill dirs,
/// rate-limit source). That is how a config field can never again be wired at
/// one spawn site and silently dropped at another — which is exactly what
/// happened to `external_subagents` when the daemon path hardcoded
/// `Vec::new()` and in-TUI respawns lost `subagent_claude-code`.
pub fn prepare_from_config(cfg: &crate::config::CodingRuntimeConfig) -> PrepareOptions {
    let external_subagents = cfg
        .subagent_config
        .as_ref()
        .map(|c| resolve_external_subagents(&c.subagent, cfg.interactive))
        .unwrap_or_default();
    PrepareOptions {
        mcp: cfg.mcp,
        external_subagents,
        ..PrepareOptions::default()
    }
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
    /// A fresh session prepared in memory but not yet catalog-visible. CodingRuntime
    /// publishes it only after the complete candidate graph has assembled.
    staged_fresh: Option<(SessionMeta, SessionHeader)>,
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

/// Register the shipped AtomGit REST capabilities into a coding tool catalog.
///
#[cfg(feature = "atomgit")]
fn register_atomgit_capabilities(
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

/// Everything `assemble` composes — and everything a respawn must REUSE so state
/// survives (approval grants, hook state, session identity).
pub struct CodingParts {
    registry: ToolRegistry,
    tool_names: Vec<String>,
    /// Capability-graph decision made at prepare time. Like request-user-input,
    /// changing the master switch requires a capability reprepare; provider-only
    /// reassembly must not advertise a tool absent from the mounted catalog.
    todo_enabled: bool,
    mcp_tool_names: Arc<std::sync::RwLock<Vec<String>>>,
    /// Connection events for whoever publishes this registry's tools: the chain's
    /// catalog task, or a tree's `mcp-host` row. Taken once.
    mcp_connect_rx: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<McpConnectEvent>>>,
    /// The same events again, for the `mcp-telemetry` row. `None` when nothing
    /// is metering, which is also when no fan-out was started. Taken once.
    mcp_telemetry_rx:
        std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<McpConnectEvent>>>,
    mcp_publish_lock: Arc<tokio::sync::Mutex<()>>,
    mcp_publication_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// The mounted catalog the `mcp-host` row publishes into, once it is up.
    ///
    /// Withdrawal has to reach the SAME catalog the model is offered, not just
    /// this struct's bookkeeping: `/mcp reload` and friends withdraw first and
    /// may then fail without rebuilding, leaving the mounted tree serving
    /// whatever was published. Filled by the row; `None` before it mounts or
    /// when no MCP is configured, and that is the "nothing published yet" case.
    mcp_toolbox: Arc<std::sync::RwLock<Option<Arc<atomcode_harness::seams::ToolBox>>>>,
    /// True only after the publisher has reconciled every initial connection into
    /// the mounted kernel catalog. This is distinct from transport readiness.
    mcp_catalog_ready: tokio::sync::watch::Sender<bool>,
    _mcp_work_guard: McpWorkGuard,
    /// The approval gate, handle EXPOSED: respawning on the same parts keeps every
    /// allow-always grant (the in_memory-buried-in-the-assembly bug from the review).
    pub approval: Arc<ApprovalMiddleware>,
    /// The person's on/off over individual tools, for the same reason `approval`
    /// is here: the tree is rebuilt on undo, restore, `/model` and a logout, and
    /// none of those are "put the tools back". A reprepare hands the old one
    /// over (`adopt_tool_switches`); a new session starts clean, because a new
    /// session is where the config speaks again.
    pub(crate) tool_switches: Arc<atomcode_harness::seams::ToolSwitches>,
    /// The mounted catalog the `tools-host` row built, so the runtime can
    /// answer "what can the model call right now" without reaching into the
    /// App. Filled by the row, cleared when it unloads — the same shape
    /// `mcp_toolbox` uses, and for the same reason.
    tool_catalog: Arc<std::sync::RwLock<Option<Arc<atomcode_harness::seams::ToolBox>>>>,
    /// What the `capability-commands` row asks the runtime to do — goal, loop,
    /// the local-context queue, the policy intervention (`docs/adr/0021` §3).
    /// Filled by the runtime before it mounts, and kept across a rebuild:
    /// nothing in `prepare` can build it, because it talks back to the loop that
    /// owns these parts.
    pub(crate) runtime_commands: Option<Arc<dyn crate::runtime::RuntimeCommands>>,
    /// Concrete handle retained so provider-only reassembly can update the
    /// per-turn cost attribution without rebuilding session-owned hooks.
    snapshot_hook: Option<Arc<SnapshotHook>>,
    extra_tools: Vec<Arc<dyn atomcode_kernel::tool::Tool>>,
    host_only_tools: Vec<String>,
    /// The skill catalog prepare loaded, and its prompt rendering (prioritizing
    /// skills the project's instructions name), and where it looked. `None`
    /// without tools.
    skill_registry: Option<crate::host_rows::LoadedSkills>,
    snapshot_persistence_status: Option<SnapshotPersistenceStatus>,
    pub session: Option<SessionBinding>,
    /// Runtime-owned resume for sessionless drivers during an in-process reassembly.
    /// Persistent sessions reload their canonical snapshot through `SessionBinding` instead.
    runtime_resume: Option<SessionSnapshot>,
    /// Connected MCP servers (None when `opts.tools` or `opts.mcp` was false; an empty
    /// registry, not None, when MCP is on but nothing is configured).
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
    /// The provider for model calls made outside a round — summaries — recorded
    /// and metered; filled with the others by [`wire_side_providers`].
    side_provider: SharedReviewProvider,
    /// Child-agent assembly prepared from the same live tier-provider cells and
    /// execution policy as `task`. The `team` tool is mounted in the next phase.
    /// Runtime-owned Team Agent orchestration. The manager is shared with the
    /// mounted tool, while lifecycle termination is driven only by CodingRuntime.
    pub team_manager: crate::team::TeamRunManager,
    /// `[subagent]` `(max_concurrent, max_rounds)`, when delegation is on.
    pub(crate) subagent_knobs: Option<(usize, u32)>,
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
    // Tools a tree takes from here as they are (see `CodingParts::host_only_tools`):
    // no harness row provides them, or the product's own version is the one that
    // ships and the row's is a different contract under the same capability.
    let mut host_only_tools: Vec<String> = Vec::new();
    let turn_execution_policy = Arc::new(TurnExecutionPolicy::new());

    // Always-on core: neutral fs/bash toolset + codeintel. Vision gating: a VL model
    // (e.g. Qwen3-VL) makes read_file hand image files to the model as pictures. Uses the
    // SAME canonical detector as the user-paste path (`model_suggests_vision`) so one
    // model can't accept a pasted image yet refuse a read_file image. NOTE: this is the
    // PREPARE-time flag; `assemble` re-registers read_file on every model swap (see there)
    // so a `/model` change to/from a VL model can't leave it stale.
    if opts.tools {
        register_coding_tools_with_vision(&mut registry, cfg.supports_vision);
        names.extend(
            atomcode_capabilities::tools::coding_tool_names()
                .iter()
                .map(|s| s.to_string()),
        );
    }
    let request_user_input_enabled = opts.tools
        && opts.request_user_input
        && crate::persona::request_user_input_switch_enabled();
    let todo_enabled = opts.tools && crate::persona::todo_switch_enabled_for(cfg.todo.enabled);
    let subagents_enabled = opts.tools
        && opts
            .subagents
            .resolve(std::env::var("ATOMCODE_SUBAGENT").ok().as_deref());
    if !todo_enabled {
        names.retain(|name| name != "todowrite");
    }
    if !request_user_input_enabled {
        names.retain(|name| name != "request_user_input");
    } else {
        // The product's structured question — single, multiple, free text, a batch
        // of up to four — in place of the tree's `ask_user`, which offers a choice
        // and nothing else.
        host_only_tools.push("request_user_input".into());
    }
    if opts.tools {
        register_codeintel_tools(&mut registry);
        names.extend(
            atomcode_capabilities::codeintel::codeintel_tool_names()
                .iter()
                .map(|s| s.to_string()),
        );
        if atomcode_capabilities::codeintel::register_lsp_tool(&mut registry, &cfg.lsp) {
            names.push("lsp".into());
            host_only_tools.push("lsp".into());
        }
    }

    // External-agent subagents (Claude Code / Codex as named subagent tools).
    // Each enabled profile whose binary is present on PATH mounts one tool.
    // External-agent subagents: each enabled profile whose binary is on PATH
    // mounts one tool, which the tree takes as a host tool.
    if !opts.tools || opts.external_subagents.is_empty() {
        false
    } else {
        let mounted = atomcode_capabilities::subagent::tool::register_external_subagent_tools(
            &mut registry,
            &opts.external_subagents,
        );
        let any = !mounted.is_empty();
        host_only_tools.extend(mounted.iter().cloned());
        names.extend(mounted);
        any
    };

    // `[tools.atomgit]` switch (`ATOMCODE_ATOMGIT` env wins over the config value):
    // off ⇒ the four typed REST tools stay out of the catalog AND the persona guidance
    // block (delivered by `host_tool_guidance`, below, so it travels with whatever is
    // mounted) — instructing the model to call unmounted tools provokes phantom calls.
    #[cfg(feature = "atomgit")]
    let atomgit_enabled = opts.tools
        && atomcode_config::config::atomgit_enabled_from_env(
            std::env::var("ATOMCODE_ATOMGIT").ok().as_deref(),
            cfg.atomgit_enabled,
        );
    #[cfg(feature = "atomgit")]
    if atomgit_enabled {
        let before = names.len();
        register_atomgit_capabilities(&mut registry, &mut names)
            .map_err(|error| io::Error::other(format!("AtomGit tool setup failed: {error}")))?;
        host_only_tools.extend(names[before..].iter().cloned());
    }

    if opts.tools && opts.web && !atomcode_config::config::offline::is_offline_active() {
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
    let review_provider: Option<SharedReviewProvider> = if opts.tools && opts.review {
        let slot: SharedReviewProvider = Arc::new(std::sync::RwLock::new(None));
        registry.register(Arc::new(
            ReviewTool::new(
                slot.clone(),
                ReviewToolConfig {
                    model: cfg.model.clone(),
                    context_window: cfg.context_window,
                    stream_timeout: cfg.stream_timeout,
                    first_token_timeout: cfg.first_token_timeout,
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
        host_only_tools.push("code_review".into());
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
    // Delegation is the tree's: the `subagent-in-process` and `team-in-process`
    // rows (`docs/adr/0023` §2), configured from `[subagent]` by the runtime.
    // What stays here is the provider a delegated agent runs on when it inherits
    // the conversation's model — billed to the session apart from it — filled at
    // assemble like the reviewer's.
    let subagent_provider: Option<SharedReviewProvider> =
        subagents_enabled.then(|| Arc::new(std::sync::RwLock::new(None)) as SharedReviewProvider);
    let subagent_knobs =
        subagents_enabled.then_some((subagent_max_concurrent, subagent_max_rounds));

    // Build the context hook once so skill-catalog ranking and later context
    // injection observe the exact same instruction-file precedence and bytes.
    let session_context_hook = Arc::new(SessionContextHook::new(&cfg.working_dir));
    let instruction_text = session_context_hook.instruction_text();

    // Skills: standard home+project precedence unless the caller supplied dirs.
    let skill_dirs = if opts.tools {
        opts.skill_dirs.clone().unwrap_or_else(|| {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            runtime_skill_dirs(&home, &cfg.working_dir)
        })
    } else {
        Vec::new()
    };
    // Plugin-contributed skills: each (dir, namespace) pair registered as
    // `<namespace>:<skill-name>`, matching the slash-menu's core registry
    // convention. Empty when the driver saw no installed plugins (the L1
    // capabilities crate cannot reach the core plugin loader by design).
    let skills = SkillRegistry::load(&skill_dirs);
    if opts.tools {
        for (dir, ns) in &opts.plugin_skill_dirs {
            skills.load_dir(dir, Some(ns));
        }
    }
    let skills = Arc::new(skills);
    // Render the catalog BEFORE the registry is moved into the tools; injected as a
    // leading system message by SkillCatalogHook below (without it the model never
    // learns which skills exist — only the use_skill/list_skills tools were mounted).
    let skill_catalog = skills.render_catalog_prioritizing(&instruction_text);
    let skill_registry = opts.tools.then(|| crate::host_rows::LoadedSkills {
        registry: Arc::clone(&skills),
        catalog: skill_catalog.clone(),
        dirs: skill_dirs.clone(),
        // Only for the standard list: a driver that named its own directories
        // decided where skills come from, and this runtime cannot say where a
        // new one belongs in that.
        install: opts.skill_dirs.is_none().then(|| {
            let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
            atomcode_capabilities::skills::runtime_skill_install_dirs(&home, &cfg.working_dir)
        }),
        plugins: opts
            .plugin_skill_dirs
            .iter()
            .map(|(_, plugin)| plugin.clone())
            .collect(),
    });
    if opts.tools {
        register_skill_tools(&mut registry, skills);
        names.extend(
            atomcode_capabilities::skills::skill_tool_names()
                .iter()
                .map(|s| s.to_string()),
        );
    }

    // MCP readiness is supplemental: start connections now, but never await them on
    // the session candidate path. `mount()` publishes each connected server's tools
    // atomically for the next turn, then publishes once more when the initial pass
    // reaches its bounded terminal state.
    let (mcp_registry, mcp_connect_rx, mcp_telemetry_rx) = if opts.tools && opts.mcp {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<McpConnectEvent>();
        // A second listener, only when something is metering. The registry
        // publishes to one sender, so the fan-out is here rather than in
        // `atomcode-capabilities` — one consumer is still the common case, and
        // a telemetry-free embedder pays nothing for this.
        //
        // Chaining instead of teeing would make the meter load-bearing for the
        // tool catalog: turn telemetry off and the tools stop being published.
        let (event_rx, telemetry_rx) = match cfg.telemetry.is_some() {
            false => (event_rx, None),
            true => {
                let (rows_tx, rows_rx) = tokio::sync::mpsc::unbounded_channel();
                let (meter_tx, meter_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut source = event_rx;
                // Ends when the registry drops its sender, which happens when
                // these parts do — no handle to abort, and nothing to leak.
                tokio::spawn(async move {
                    while let Some(event) = source.recv().await {
                        let to_rows = rows_tx.send(event.clone()).is_ok();
                        let to_meter = meter_tx.send(event).is_ok();
                        if !to_rows && !to_meter {
                            break;
                        }
                    }
                });
                (rows_rx, Some(meter_rx))
            }
        };
        (
            Some(Arc::new(McpRegistry::from_config_background_with_extra(
                &cfg.working_dir,
                Some(event_tx),
                opts.extra_mcp_servers.clone(),
            ))),
            Some(event_rx),
            telemetry_rx,
        )
    } else {
        (None, None, None)
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
            let working_dir = cfg.working_dir.to_string_lossy().into_owned();
            let mut meta = SessionMeta::new(&id, working_dir.as_str(), now);
            meta.owner = StorageOwner::Native;
            // The session's log starts with what stays true of it, the context
            // block its prompt opens with among them: a resume sends that
            // prefix again rather than one rendered from a repository that has
            // moved on.
            let mut header = SessionHeader::new(&id);
            header.created_at = u64::try_from(now).unwrap_or(0);
            header.cwd = Some(working_dir);
            header.context = Some(SessionContextHook::new(&cfg.working_dir).block(None));
            if !stage_fresh {
                manager
                    .create_event_session(&lease, &header, &meta)
                    .map_err(io::Error::from)?;
            }
            Some(SessionBinding {
                id,
                manager,
                lease,
                resume: None,
                pending_resume_prompt: None,
                staged_fresh: stage_fresh.then_some((meta, header)),
            })
        }
        SessionMode::Resume(id) => {
            let manager = Arc::new(SessionManager::for_project(&cfg.working_dir));
            let lease = session_lease(&manager, id, reuse_lease.as_ref())?;
            // Resume is a native-only boundary. Legacy/unconfirmed data must first
            // converge through a driver importer; accepting a lone snapshot here
            // would bypass ownership and manufacture an incomplete native session.
            // A session a released build stored as a snapshot becomes a log here,
            // once (`docs/adr/0024` §10).
            let (loaded, pending_resume_prompt) =
                manager.open_for_resume(&lease).map_err(io::Error::from)?;
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
            manager.open_as_events(&lease).map_err(io::Error::from)?;
            let loaded = manager.load_native_session(id).map_err(io::Error::from)?;
            check_snapshot_version(&loaded.snapshot)?;
            // What the log projects carries no system prompt; that is assembled
            // for each request.
            if !atomcode_capabilities::session::events::same_conversation(
                &loaded.snapshot.messages,
                &snapshot.messages,
            ) {
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
        host_only_tools.push("recall".into());
        registry.register(Arc::new(
            ListSessionsTool::new().with_sessions_dir(b.manager.root()),
        ));
        names.push("list_sessions".into());
        host_only_tools.push("list_sessions".into());
    }

    // What the session keeps beside its log — per-turn statistics, the rewind
    // ledger, the name — which a `kernel-hooks` row runs at the tree's own
    // moments (see `host_rows`). The log itself is `session-store`'s. Everything
    // else the chain registered here — memory, the skill catalog, the MCP
    // instructions tail, the verify cadence, the todo reminder, the skill-first
    // nudge — is a row now, and the row builds its own.
    let mut snapshot_hook_handle = None;
    let mut snapshot_persistence_status = None;
    if let Some(b) = &session {
        let wd = cfg.working_dir.to_string_lossy().into_owned();
        let snapshot_hook = Arc::new(
            SnapshotHook::new(b.manager.clone(), &b.id, &wd)
                .with_lease(b.lease.clone())
                .with_model_attribution(&cfg.provider_name, &cfg.model),
        );
        snapshot_persistence_status = Some(snapshot_hook.persistence_status());
        snapshot_hook_handle = Some(snapshot_hook);
    }
    // CC external hooks: user/project `hooks.json` + plugin-contributed inline hooks
    // (`plugin_cc_hooks`, resolved by the host), mounted by the `cc-hooks-host` row on
    // both the lifecycle and the tool-middleware seam. Only when hooks actually exist.
    let cc_external = {
        let mut cc = CCExternalHooks::load_with_extra(&cfg.working_dir, plugin_cc_hooks);
        // Stamp the persistent session id into every CC payload (CC `session_id`), so a
        // hook can correlate its events with the session. Empty for non-persistent runs.
        if let Some(b) = &session {
            cc = cc.with_session_id(b.id.as_str());
            // CC `transcript_path` = the session's log, which replaced the transcript
            // (`docs/adr/0024` §14), so a Stop/StopFailure hook can open the finished
            // turn's full record: one JSON fact per line after a header line. The
            // path resolves before the file is written (a session not published yet);
            // unresolvable → left `None` → the payload carries `null`, never a wedge.
            if let Ok(p) = b.manager.events_path(&b.id) {
                cc = cc.with_transcript_path(p.to_string_lossy().into_owned());
            }
        }
        if cc.is_empty() {
            None
        } else {
            Some(Arc::new(cc))
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

    if !opts.tools {
        names.clear();
    }

    Ok(CodingParts {
        runtime_commands: None,
        tool_switches: atomcode_harness::seams::ToolSwitches::new(),
        tool_catalog: Arc::new(std::sync::RwLock::new(None)),
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
        mcp_tool_names,
        mcp_connect_rx: std::sync::Mutex::new(mcp_connect_rx),
        mcp_telemetry_rx: std::sync::Mutex::new(mcp_telemetry_rx),
        mcp_publish_lock: Arc::new(tokio::sync::Mutex::new(())),
        mcp_publication_enabled,
        mcp_toolbox: Arc::new(std::sync::RwLock::new(None)),
        mcp_catalog_ready: tokio::sync::watch::channel(mcp_registry.is_none()).0,
        _mcp_work_guard: mcp_work_guard,
        approval: Arc::new(ApprovalMiddleware::in_memory()),
        snapshot_hook: snapshot_hook_handle,
        extra_tools: Vec::new(),
        host_only_tools,
        skill_registry,
        snapshot_persistence_status,
        session,
        runtime_resume: None,
        mcp_registry,
        review_provider,
        subagent_provider,
        side_provider: Arc::new(std::sync::RwLock::new(None)),
        team_manager,
        subagent_knobs,
        cc_external_hooks: cc_external,
        rate_limit_source: opts.rate_limit_source,
    })
}

/// A provider that is whatever a slot holds when it is called.
struct SlotProvider {
    slot: SharedReviewProvider,
    model: String,
}

impl SlotProvider {
    fn current(&self) -> Option<Arc<dyn LlmProvider>> {
        self.slot.read().ok().and_then(|provider| provider.clone())
    }
}

#[async_trait::async_trait]
impl LlmProvider for SlotProvider {
    fn model_name(&self) -> &str {
        &self.model
    }
    fn context_window(&self) -> u32 {
        self.current().map(|p| p.context_window()).unwrap_or(0)
    }
    fn supports_vision(&self) -> bool {
        self.current().is_some_and(|p| p.supports_vision())
    }
    fn effort_levels(&self) -> Vec<String> {
        self.current()
            .map(|p| p.effort_levels())
            .unwrap_or_default()
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[atomcode_kernel::tool::ToolDef],
        options: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        match self.current() {
            Some(provider) => provider.chat_stream(messages, tools, options).await,
            None => Err(atomcode_kernel::stream::ProviderError {
                retryable: false,
                message: "no model is signed in for delegated work".into(),
                http_status: None,
                code: None,
                retry_after_secs: None,
            }),
        }
    }
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

impl SessionBinding {
    /// The header a session not published yet will be created with.
    pub(crate) fn staged_header(&self) -> Option<&SessionHeader> {
        self.staged_fresh.as_ref().map(|(_, header)| header)
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

    /// `[subagent]` `(max_concurrent, max_rounds)`, when delegation is on.
    pub(crate) fn subagent_knobs(&self) -> Option<(usize, u32)> {
        self.subagent_knobs
    }

    /// The provider a delegated agent inheriting the conversation's model runs
    /// on: whatever the subagent slot holds at the moment of each call. Read
    /// through, never copied — a logout empties the slot, and a copy taken at
    /// mount would keep the signed-in provider alive in the tree.
    pub(crate) fn delegated_provider(&self) -> Option<Arc<dyn LlmProvider>> {
        let slot = self.subagent_provider.clone()?;
        let model = slot
            .read()
            .ok()
            .and_then(|provider| provider.as_ref().map(|p| p.model_name().to_string()))
            .unwrap_or_default();
        Some(Arc::new(SlotProvider { slot, model }))
    }

    pub(crate) fn todo_enabled(&self) -> bool {
        self.todo_enabled
    }

    /// Everything a tree needs to publish this registry's MCP tools the way the
    /// chain's catalog task does, sharing its locks, switches and readiness.
    pub(crate) fn mcp_publication(&self) -> Option<crate::host_rows::McpPublication> {
        let registry = self.mcp_registry.clone()?;
        Some(crate::host_rows::McpPublication {
            registry,
            connect_rx: self
                .mcp_connect_rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
            tool_names: Arc::clone(&self.mcp_tool_names),
            publish_lock: Arc::clone(&self.mcp_publish_lock),
            publication_enabled: Arc::clone(&self.mcp_publication_enabled),
            catalog_ready: self.mcp_catalog_ready.clone(),
            toolbox_slot: Arc::clone(&self.mcp_toolbox),
        })
    }

    /// The connection events again, for the row that meters them.
    ///
    /// `None` when this assembly has no MCP or nothing to meter with, which is
    /// what keeps the `mcp-telemetry` row out of a tree that would have nothing
    /// for it to do.
    pub(crate) fn mcp_connect_meter(
        &self,
    ) -> Option<tokio::sync::mpsc::UnboundedReceiver<McpConnectEvent>> {
        self.mcp_telemetry_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Where the provider for out-of-round model calls lives, refilled on every
    /// model change.
    pub(crate) fn side_provider_slot(&self) -> SharedReviewProvider {
        Arc::clone(&self.side_provider)
    }

    /// Tools a tree takes from this capability graph as the objects prepare built:
    /// the ones no harness row provides — `list_sessions` over the native catalog,
    /// `lsp`, external-agent subagents, the AtomGit tools — and the ones whose
    /// product contract a row does not match: `request_user_input`, `task` and
    /// `team` (tiers, worker scopes, the team panel), `code_review` (the product's
    /// limits and provider), `recall` (over the native store, which is the master).
    pub(crate) fn host_only_tools(&self) -> Vec<Arc<dyn atomcode_kernel::tool::Tool>> {
        self.host_only_tools
            .iter()
            .filter_map(|name| self.registry.mount(&[name.as_str()]).get(name))
            .collect()
    }

    pub(crate) fn skill_registry(&self) -> Option<crate::host_rows::LoadedSkills> {
        self.skill_registry.clone()
    }

    #[cfg(test)]
    pub(crate) fn report_snapshot_persistence_uncertain(&mut self, message: impl Into<String>) {
        self.snapshot_persistence_status
            .as_ref()
            .expect("persistent test parts must have a snapshot status")
            .report_uncertain_commit(message);
    }

    /// Make a prepared fresh session durable and catalog-visible. This is the
    /// session transition's persistence commit point; preparing and mounting
    /// deliberately leave the catalog untouched.
    pub fn publish_staged_session(&mut self) -> io::Result<()> {
        let Some(binding) = self.session.as_mut() else {
            return Ok(());
        };
        let Some((meta, header)) = binding.staged_fresh.as_ref() else {
            return Ok(());
        };
        binding
            .manager
            .create_event_session(&binding.lease, header, meta)
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
    /// The runtime's own capabilities, for the row that offers them as commands.
    /// The switches this session's catalog answers to.
    pub(crate) fn tool_switches(&self) -> Arc<atomcode_harness::seams::ToolSwitches> {
        self.tool_switches.clone()
    }

    /// Where the `tools-host` row publishes the catalog it built.
    pub(crate) fn tool_catalog_slot(
        &self,
    ) -> Arc<std::sync::RwLock<Option<Arc<atomcode_harness::seams::ToolBox>>>> {
        Arc::clone(&self.tool_catalog)
    }

    /// The live catalog, once a tree carrying `tools-host` has mounted.
    pub(crate) fn tool_catalog(&self) -> Option<Arc<atomcode_harness::seams::ToolBox>> {
        self.tool_catalog
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Take over the switches a previous parts held, so a reprepare does not
    /// quietly put back the tools the person turned off.
    pub(crate) fn adopt_tool_switches(
        &mut self,
        switches: Arc<atomcode_harness::seams::ToolSwitches>,
    ) {
        self.tool_switches = switches;
    }

    pub(crate) fn set_runtime_commands(
        &mut self,
        commands: Arc<dyn crate::runtime::RuntimeCommands>,
    ) {
        self.runtime_commands = Some(commands);
    }

    pub(crate) fn set_runtime_resume(&mut self, snapshot: SessionSnapshot) {
        self.runtime_resume = Some(snapshot);
    }

    pub(crate) fn runtime_resume_snapshot(&self) -> Option<SessionSnapshot> {
        self.runtime_resume.clone()
    }

    /// Readiness receiver for non-interactive surfaces whose first turn should
    /// include the catalog reconciled before their caller-owned timeout.
    pub(crate) fn mcp_readiness_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.mcp_catalog_ready.subscribe()
    }

    /// Fail-closed cutover used before a capability reload reads mutable MCP
    /// config/trust/auth state. Once disabled, this scope's late connection events
    /// cannot republish tools even if the replacement candidate fails.
    ///
    /// Takes the tools OFF the mounted catalog, not just off the books. Every
    /// caller (`/mcp reload`, `/mcp untrust`, `/mcp logout`) withdraws before it
    /// reads state that may turn out to be unusable, and returns without
    /// rebuilding when it does — so the tree that is still mounted must already
    /// have stopped offering `mcp__*` by the time this returns. Criterion:
    /// `withdrawing_mcp_takes_the_tools_off_the_model`.
    pub(crate) async fn withdraw_mcp_tools(&mut self) {
        self.mcp_publication_enabled
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(registry) = &self.mcp_registry {
            registry.cancel_pending_work();
        }
        // Held across the unregister so a publish that is already inside the
        // lock finishes first and its names are in the list we drain — rather
        // than being added right after we cleared it.
        let _publish_guard = self.mcp_publish_lock.lock().await;
        let names: Vec<String> = match self.mcp_tool_names.write() {
            Ok(mut names) => names.drain(..).collect(),
            Err(poisoned) => poisoned.into_inner().drain(..).collect(),
        };
        let toolbox = self
            .mcp_toolbox
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(toolbox) = toolbox {
            for name in &names {
                toolbox.unregister(name);
            }
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
            self.tool_names.push(name.clone());
        }
        self.extra_tools.retain(|existing| existing.name() != name);
        self.extra_tools.push(tool.clone());
        self.registry.register(tool);
    }

    /// Tools the runtime added on top of the capability graph (`schedule_wakeup`),
    /// latest registration per name.
    pub(crate) fn extra_tools(&self) -> Vec<Arc<dyn atomcode_kernel::tool::Tool>> {
        self.extra_tools.clone()
    }
}

/// One configured MCP server, as a management list needs it: the file's static
/// config joined with what the running session actually has.
#[derive(Clone, Debug, PartialEq)]
pub struct McpRowFacts {
    pub name: String,
    pub disabled: bool,
    pub source: atomcode_capabilities::mcp::McpConfigSource,
    pub config_path: Option<std::path::PathBuf>,
    pub transport: atomcode_capabilities::mcp::McpTransportKind,
    /// The stdio program and its args, when this is a stdio server. Never
    /// carries env: that is where a stdio server's own secrets live.
    pub command: Option<(String, Vec<String>)>,
    /// The endpoint, when this is an HTTP server. Never carries headers: those
    /// may hold `Authorization: Bearer …`.
    pub url: Option<String>,
    /// The server authenticates by OAuth (so `authenticated` means something).
    pub oauth: bool,
    /// A usable token is stored for it.
    pub authenticated: bool,
    pub status: atomcode_capabilities::mcp::ServerStatus,
    pub tool_count: usize,
}

/// Join the configured servers (disabled ones included) with the live session.
///
/// The registry and the tool counts are arguments rather than ambient reads, so
/// the join can be tested against a registry that never connected to anything.
pub async fn mcp_row_facts(
    working_dir: &std::path::Path,
    registry: &atomcode_capabilities::mcp::McpRegistry,
    tool_counts: &[(String, usize)],
) -> Vec<McpRowFacts> {
    use atomcode_capabilities::mcp::{
        config_path_for_source, load_mcp_config_including_disabled, token_is_expired,
        McpHttpAuthConfig, McpTokenStore, McpTransportConfig, ServerStatus,
    };
    use std::collections::HashMap;

    // A malformed file is the connection path's to report; a management list
    // must not turn it into "no servers configured".
    let configs = load_mcp_config_including_disabled(working_dir).unwrap_or_default();
    let live: HashMap<String, ServerStatus> =
        registry.server_statuses().await.into_iter().collect();
    let counts: HashMap<&str, usize> = tool_counts.iter().map(|(n, c)| (n.as_str(), *c)).collect();
    let tokens = McpTokenStore::default();

    configs
        .into_iter()
        .map(|config| {
            let (command, url) = match &config.config {
                McpTransportConfig::Stdio { command, args, .. } => {
                    (Some((command.clone(), args.clone())), None)
                }
                McpTransportConfig::Http { url, .. } => (None, Some(url.clone())),
            };
            let oauth = matches!(
                &config.config,
                McpTransportConfig::Http {
                    auth: Some(McpHttpAuthConfig::OAuth(_)),
                    ..
                }
            );
            let authenticated = oauth
                && matches!(tokens.load_token(&config.name), Ok(Some(t)) if !token_is_expired(&t));
            let disabled = config.disabled;
            McpRowFacts {
                config_path: config_path_for_source(working_dir, config.source),
                status: if disabled {
                    // Not in the tree: the session has no status for it, and
                    // must not be asked for one.
                    ServerStatus::Disconnected
                } else {
                    live.get(&config.name)
                        .cloned()
                        .unwrap_or(ServerStatus::Disconnected)
                },
                tool_count: if disabled {
                    0
                } else {
                    counts.get(config.name.as_str()).copied().unwrap_or(0)
                },
                transport: config.config.kind(),
                command,
                url,
                disabled,
                oauth,
                authenticated,
                name: config.name,
                source: config.source,
            }
        })
        .collect()
}

/// Fill the providers this capability graph's own sub-agents run on, for `cfg`'s
/// model: the reviewer's and the subagent host tier's slots, the fast/capable tier
/// cells and the named-model resolver — each billed to this session's detached
/// usage and metered under its own telemetry surface.
///
/// Returns the provider for model calls the primary loop makes outside a round
/// (the overflow summary), recorded and metered the same way.
///
/// Its own function because both assemblies need it: the chain's `assemble`, and a
/// tree that mounts this graph's `task`, `team` and `code_review` as they are. A
/// slot left empty is a tool that panics on first use; a slot filled with the bare
/// provider is spend nobody is billed for.
pub(crate) fn wire_side_providers(
    parts: &CodingParts,
    cfg: &CodingAgentConfig,
    provider: &Arc<dyn LlmProvider>,
) -> Arc<dyn LlmProvider> {
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

    if let Some(b) = &parts.session {
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
                let mut recorder = atomcode_capabilities::session::DetachedUsageRecorder::new(
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

    if let Ok(mut slot) = parts.side_provider.write() {
        *slot = Some(metered_provider.clone());
    }
    metered_provider
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
    use atomcode_capabilities::session::PresentationFile;

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
        assert!(
            !profiles[0].allow_dangerous,
            "built-ins are never dangerous"
        );

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
        assert!(
            p.is_empty(),
            "disabled explicit codex blocks the built-in switch"
        );
    }

    /// Every production spawn site (CLI startup, daemon, the TUI's in-session
    /// deferred respawns) builds its `PrepareOptions` through
    /// [`prepare_from_config`]. This pins the seam the daemon path once broke:
    /// its hand-written `PrepareOptions` hardcoded
    /// `external_subagents: Vec::new()`, so a `[subagent] claude = "auto"`
    /// config silently lost `subagent_claude-code` on every in-TUI respawn
    /// while CLI startup mounted it. One derivation, one place — no second
    /// hand-copied field list to drift.
    #[test]
    fn prepare_from_config_resolves_external_subagents_for_every_spawn_site() {
        use crate::config::CodingRuntimeConfig;

        let toml = r#"
            default_provider = "p"
            [providers.p]
            type = "openai"
            model = "m"
            api_key = "k"
            base_url = "https://example.test/v1"
            [subagent]
            claude = "auto"
        "#;
        let config: atomcode_config::config::Config = toml::from_str(toml).unwrap();

        // The daemon / in-TUI-respawn shape (non-interactive): the convenience
        // switch still resolves — a bypass entry would be downgraded, `auto`
        // mounts as-is.
        let cfg = CodingRuntimeConfig::from_config(
            &config,
            std::path::Path::new("/tmp/proj"),
            None,
            None,
            false,
            false,
        );
        let prepare = prepare_from_config(&cfg);
        let names: Vec<&str> = prepare
            .external_subagents
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert!(
            names.contains(&"claude-code"),
            "a `[subagent] claude = \"auto\"` config must resolve through \
             prepare_from_config on EVERY spawn site; got {names:?}"
        );
        assert!(!prepare.external_subagents[0].allow_dangerous);

        // Defaults stay the full-capability production shape the daemon
        // expects; drivers overlay only their differences on top.
        assert!(prepare.tools);
        assert!(prepare.memory);
        assert!(prepare.web);
        assert!(prepare.review);
        assert!(prepare.request_user_input);
        assert!(prepare.mcp, "mcp follows the config flag");
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
    fn subagent_runtime_knobs_floor_concurrency() {
        use super::subagent_runtime_knobs;
        use atomcode_config::config::SubAgentConfig;
        // The legacy `timeout_secs` this test once also ignored is gone from the
        // schema; `legacy_dead_keys_still_parse` pins that a file carrying it loads.
        let cfg = SubAgentConfig {
            max_concurrent: 0,
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
            tools: true,
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
            front_end: None,
        };

        let prepared =
            tokio::time::timeout(std::time::Duration::from_millis(250), prepare(&cfg, opts)).await;

        assert!(
            prepared.is_ok(),
            "MCP readiness must not block the session candidate prepare path"
        );
        assert!(prepared.unwrap().is_ok());
    }

    // Serialized like every other `ATOMCODE_HOME` mutator in this file: it sets
    // the process-global var, so running beside another home-sensitive test (e.g.
    // `a_mount_rejects_an_incomplete_native_aggregate`, which resolves a persisted
    // session under its OWN home) races — its home leaks in and the neighbor's
    // `prepare` fails to find the session. The missing guard was a latent flake.
    #[tokio::test]
    #[serial_test::serial(atomcode_home)]
    async fn capability_reload_withdraws_old_mcp_tools_fail_closed() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let mut parts = prepare(&cfg, io_free_opts()).await.unwrap();
        parts.registry.register(Arc::new(TestMcpTool));
        parts
            .mcp_tool_names
            .write()
            .unwrap()
            .push("mcp__test__echo".into());
        parts.withdraw_mcp_tools().await;

        // What the `mcp-host` row reads: nothing left to publish, and publishing
        // switched off so a connection still in flight cannot re-add one.
        assert!(parts.mcp_tool_names.read().unwrap().is_empty());
        assert!(!parts
            .mcp_publication_enabled
            .load(std::sync::atomic::Ordering::Acquire));
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

    #[test]
    fn a_disabled_server_is_listed_but_has_no_tools() {
        // A disabled entry is in the file but not in the tree. The management
        // list has to show it — otherwise the switch back on is unreachable —
        // while its tool count stays zero, because the session never got any.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".mcp.json"),
            r#"{"mcpServers":{
                "off": {"command":"npx","args":["-y","x"],"disabled":true}
            }}"#,
        )
        .unwrap();

        let registry = std::sync::Arc::new(McpRegistry::new());
        let facts = futures::executor::block_on(mcp_row_facts(dir.path(), &registry, &[]));
        let row = facts.iter().find(|f| f.name == "off").expect("listed");
        assert!(row.disabled, "the flag reaches the row");
        assert_eq!(
            row.tool_count, 0,
            "a disabled server put nothing on the model"
        );
    }

    /// `prepare` with all optional capabilities OFF — keeps the call I/O-free (no MCP
    /// connect, no session/skill/home scans) so the test only exercises CC-hook wiring.
    fn io_free_opts() -> PrepareOptions {
        PrepareOptions {
            session: SessionMode::Disabled,
            tools: true,
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
            front_end: None,
        }
    }

    #[cfg(feature = "atomgit")]
    #[tokio::test]
    #[serial_test::serial(atomgit_env)]
    async fn production_prepare_exposes_atomgit_tools() {
        // Serialized with the test that mutates `ATOMCODE_ATOMGIT`: that one sets the
        // process-global var to disable the tools, and this one asserts they ARE present,
        // so they must not overlap — a leaked env var is a false negative here, not a flake
        // the harness will retry away.
        let project = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        let parts = prepare(&cfg, io_free_opts()).await.unwrap();
        // The field, not an accessor: `selected_tool_names` went with the chain
        // in `f3a7f048` and this test — behind the `atomgit` feature, so never
        // compiled by a plain `cargo nextest run -p atomcode-coding` — went on
        // calling it. A feature nobody builds is a feature nobody tests.
        let names = parts.tool_names.clone();

        for expected in ["atomgit_repo", "atomgit_pr", "atomgit_issue"] {
            assert!(
                names.iter().any(|name| name == expected),
                "production tool catalog must expose {expected}: {names:?}"
            );
        }
    }

    #[cfg(feature = "atomgit")]
    #[tokio::test]
    async fn atomgit_tools_absent_when_switch_disabled() {
        let project = tempfile::tempdir().unwrap();
        let mut cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        cfg.atomgit_enabled = false;
        let parts = prepare(&cfg, io_free_opts()).await.unwrap();
        let names = parts.tool_names.clone();

        for expected in ["atomgit_repo", "atomgit_pr", "atomgit_issue", "atomgit_api"] {
            assert!(
                !names.iter().any(|name| name == expected),
                "atomgit tools must be absent when switch is off: {expected} in {names:?}"
            );
        }
    }

    #[cfg(feature = "atomgit")]
    #[tokio::test]
    async fn other_tools_present_when_atomgit_disabled() {
        // The switch is scoped: turning it off must not take the rest of the
        // catalog with it.
        let project = tempfile::tempdir().unwrap();
        let mut cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        cfg.atomgit_enabled = false;
        let parts = prepare(&cfg, io_free_opts()).await.unwrap();
        let names = parts.tool_names.clone();

        for expected in [
            "read_file",
            "write_file",
            "edit_file",
            "list_directory",
            "open_file",
            "bash",
            "grep",
            "glob",
            "search_replace",
            "todowrite",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "non-atomgit tool must be present when atomgit is disabled: {expected} in {names:?}"
            );
        }
    }

    #[cfg(feature = "atomgit")]
    #[tokio::test]
    #[serial_test::serial(atomgit_env)]
    async fn atomgit_guidance_is_carried_by_the_mounted_tools_when_enabled() {
        // The link the other tests leave open: the persona body no longer teaches
        // `## ATOMGIT TOOLS:` — the block is contributed by `HostBuiltTools::apply`,
        // which iterates `host_only_tools` and asks `host_tool_guidance` for each.
        // So the real end-to-end claim is "when the switch mounts the tools, those
        // SAME tools carry the guidance". Reproduce that iteration here: on ⇒ a
        // mounted host tool yields the AtomGit block; off ⇒ none does, so the
        // prompt and the catalog cannot desync. Serialized with the env-mutating
        // switch tests so a leaked `ATOMCODE_ATOMGIT=0` cannot make the on-case a
        // false negative.
        let project = tempfile::tempdir().unwrap();

        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        assert!(cfg.atomgit_enabled, "default on");
        let parts = prepare(&cfg, io_free_opts()).await.unwrap();
        let carried_on = parts
            .host_only_tools()
            .iter()
            .filter_map(|tool| crate::persona::host_tool_guidance(tool.name()))
            .any(|(key, text)| key == "atomgit" && text.contains("## ATOMGIT TOOLS:"));
        assert!(
            carried_on,
            "enabled ⇒ a mounted host tool must carry the AtomGit guidance"
        );

        let mut off_cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        off_cfg.atomgit_enabled = false;
        let off_parts = prepare(&off_cfg, io_free_opts()).await.unwrap();
        let carried_off = off_parts.host_only_tools().iter().any(|tool| {
            crate::persona::host_tool_guidance(tool.name()).is_some_and(|(key, _)| key == "atomgit")
        });
        assert!(
            !carried_off,
            "disabled ⇒ no mounted tool may carry the AtomGit guidance"
        );
    }

    #[cfg(feature = "atomgit")]
    #[tokio::test]
    #[serial_test::serial(atomgit_env)]
    async fn atomgit_env_overrides_config_when_disabled() {
        // `ATOMCODE_ATOMGIT=0` disables the tools even when the config says on,
        // which is the escape hatch for a shared config file. The serial guard
        // keeps the process-global env from leaking into another test.
        std::env::set_var("ATOMCODE_ATOMGIT", "0");
        let project = tempfile::tempdir().unwrap();
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
        assert!(
            cfg.atomgit_enabled,
            "default must be true before the env override"
        );
        let parts = prepare(&cfg, io_free_opts()).await.unwrap();
        let names = parts.tool_names.clone();
        std::env::remove_var("ATOMCODE_ATOMGIT");

        for expected in ["atomgit_repo", "atomgit_pr", "atomgit_issue", "atomgit_api"] {
            assert!(
                !names.iter().any(|name| name == expected),
                "ATOMCODE_ATOMGIT=0 must disable atomgit tools: {expected} in {names:?}"
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
    async fn a_mount_rejects_an_incomplete_native_aggregate() {
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
        let parts = prepare(&cfg, opts.clone()).await.unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);
        crate::runtime::mount(&parts, &cfg, &opts, provider.clone())
            .await
            .expect("a complete aggregate mounts")
            .app
            .stop();
        std::fs::remove_file(manager.events_path(id).unwrap()).unwrap();

        // Half a session is not a session: rebuilding on one would hand the model
        // a conversation the store cannot explain.
        let error = match crate::runtime::mount(&parts, &cfg, &opts, provider).await {
            Ok(_) => panic!("a mount must reject an incomplete native aggregate"),
            Err(error) => error,
        };
        assert!(
            error.contains(&manager.events_path(id).unwrap().display().to_string()),
            "the failure must name the missing file: {error}"
        );
        let _ = std::mem::size_of::<SessionStoreError>();
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
        assert_eq!(
            manager.load_snapshot(id).unwrap().messages,
            canonical.messages
        );
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
        assert_eq!(
            parts
                .session
                .unwrap()
                .resume
                .map(|resumed| resumed.messages),
            Some(canonical.messages)
        );
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
    async fn prepare_gives_a_persistent_session_a_snapshot_writer() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", home.path());
        let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());

        let mut persistent = io_free_opts();
        persistent.session = SessionMode::Fresh;
        let parts = prepare(&cfg, persistent).await.unwrap();
        // The writer the `session-native` and `native-compaction-checkpoint`
        // rows call. Without it there is nothing to store a session with.
        assert!(parts.snapshot_hook().is_some());
        let binding = parts.session.as_ref().unwrap();
        assert_eq!(
            binding.manager.read_meta(&binding.id).unwrap().owner,
            StorageOwner::Native
        );
        assert!(binding.manager.load_snapshot(&binding.id).is_ok());

        let ephemeral = prepare(&cfg, io_free_opts()).await.unwrap();
        assert!(ephemeral.snapshot_hook().is_none());
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
        // One instance serves both seams: the `cc-hooks-host` row mounts it as a
        // lifecycle hook and as tool middleware.
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
        let parts = prepare(&cfg, opts).await.unwrap();

        let provider: Arc<dyn LlmProvider> = Arc::new(CannedProvider);
        wire_side_providers(&parts, &cfg, &provider);

        let slot = parts
            .review_provider
            .clone()
            .expect("review enabled ⇒ slot present");
        let review_provider = slot
            .read()
            .unwrap()
            .clone()
            .expect("the slot is filled with the providers");
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
        mw.after(&mut result, None).await;
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
}
