//! The two-phase FULL assembly: `prepare` (async, does I/O: MCP eager-connect,
//! skill loading, session binding) → `assemble` (pure composition, no I/O).
//!
//! WHY two phases (pre-C1 design review, all four confirmed findings):
//! - **sync/async**: MCP must eager-connect BEFORE spawn (MountedTools are frozen;
//!   eager connect keeps the tool list a stable cache prefix from turn 1), but
//!   assembly itself should stay pure composition. `prepare` absorbs the await.
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
use atomcode_capabilities::mcp::{self, McpConnectEvent, McpRegistry};
use atomcode_capabilities::memory::MemoryHook;
use atomcode_capabilities::session::{
    RecallTool, SessionContextHook, SessionManager, SnapshotHook, StatusReminderHook,
    TranscriptHook,
};
use atomcode_capabilities::skills::{register_skill_tools, standard_skill_dirs, SkillRegistry};
use atomcode_capabilities::tools::{
    register_coding_tools_with_vision, ApprovalMiddleware, BashWorkspaceGate, OpenFileWorkspaceGate,
    ReadFileTool, SensitivePathGate, WebFetchTool, WebSearchTool, WriteApprovalGate,
};
use atomcode_core::provider::model_name_suggests_vision;
use atomcode_kernel::agent::Agent;
use atomcode_kernel::checkpoint::CompactionCheckpoint;
use atomcode_kernel::hook::LifecycleHooks;
use atomcode_kernel::message::{Message, Role, SessionSnapshot};
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{MountedTools, ToolRegistry};
use atomcode_review::{ReviewTool, ReviewToolConfig, SharedReviewProvider};

use crate::config::CodingAgentConfig;
use crate::discipline::VerifyCadenceHook;
use crate::persona::coding_persona;

/// How `prepare` binds the agent to on-disk session persistence.
#[derive(Clone, Debug, Default)]
pub enum SessionMode {
    /// Allocate a fresh session id (uuid v4) and persist from turn 1.
    #[default]
    Fresh,
    /// Resume the given session id: load its `.snapshot`, continue its `.jsonl`.
    /// `prepare` errors if the snapshot cannot be read (the caller listed it, so a
    /// missing/corrupt file is a real failure, not a silent fresh start).
    Resume(String),
    /// No persistence (CI / one-shot / review-style runs).
    Disabled,
}

/// Capability inputs for [`prepare`] — what to wire beyond the always-on core
/// (fs/bash tools + codeintel). Defaults = the full production-parity agent.
#[derive(Clone, Debug)]
pub struct PrepareOptions {
    pub session: SessionMode,
    /// Skill dirs in LOW→HIGH priority order; `None` = the standard home+project
    /// precedence ([`standard_skill_dirs`]).
    pub skill_dirs: Option<Vec<PathBuf>>,
    /// Connect MCP servers from `<working_dir>/.mcp.json` (+ global config).
    pub mcp: bool,
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
}

impl Default for PrepareOptions {
    fn default() -> Self {
        Self {
            session: SessionMode::Fresh,
            skill_dirs: None,
            mcp: true,
            memory: true,
            web: true,
            review: true,
        }
    }
}

/// The session identity + persistence wiring, allocated ONCE by [`prepare`] —
/// the single owner the design review asked for.
pub struct SessionBinding {
    pub id: String,
    pub manager: Arc<SessionManager>,
    /// Loaded snapshot on [`SessionMode::Resume`]; `None` on fresh.
    pub resume: Option<SessionSnapshot>,
}

/// Everything `assemble` composes — and everything a respawn must REUSE so state
/// survives (approval grants, hook state, session identity).
pub struct CodingParts {
    registry: ToolRegistry,
    tool_names: Vec<String>,
    /// The approval gate, handle EXPOSED: respawning on the same parts keeps every
    /// allow-always grant (the in_memory-buried-in-the-assembly bug from the review).
    pub approval: Arc<ApprovalMiddleware>,
    /// Lifecycle hooks in the CANONICAL ORDER (see [`assemble`]).
    hooks: Vec<Arc<dyn LifecycleHooks>>,
    /// The same session snapshot writer used by `SnapshotHook`, exposed through
    /// the kernel's manual-compaction checkpoint seam.
    compaction_checkpoint: Option<Arc<dyn CompactionCheckpoint>>,
    pub session: Option<SessionBinding>,
    /// Connected MCP servers (None when `opts.mcp` was false or no config exists).
    pub mcp_registry: Option<Arc<McpRegistry>>,
    /// What happened during MCP connect — the DRIVER observes/renders these
    /// (seam-first: telemetry belongs to the driver, not the capability).
    pub mcp_events: Vec<McpConnectEvent>,
    /// The agent's tool working dir as a LIVE handle (kernel Seam 1b): the driver
    /// mutates it to implement `/cd` — tools resolve against the new dir from the
    /// next call. Session/memory/recall stay anchored to the PREPARE-time project
    /// root by design (the per-project stores don't follow a mid-session cd).
    pub shared_cwd: std::sync::Arc<std::sync::RwLock<std::path::PathBuf>>,
    /// Plan-mode toggle (read-only exploration). The driver flips it (the bridge maps
    /// `SetPlanMode`); the [`PlanModeGate`](crate::PlanModeGate) middleware reads it to
    /// block mutating tools. Shared (not rebuilt) so a respawn preserves the mode.
    pub plan_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Runtime auto-approve (bypass) flag. `SetMode(Auto)` sets it; the bridge
    /// approval seam auto-Allows while set. Mirrors `plan_mode`.
    pub bypass_mode: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Auto-accept-edits flag. `SetMode(AcceptEdits)` sets it; the
    /// [`WriteApprovalGate`](crate::tools) reads it to auto-approve NON-sensitive
    /// file edits without a prompt (bash + sensitive paths still prompt). Unlike
    /// `bypass_mode` this is enforced in a middleware (bridge-independent), mirroring
    /// `plan_mode`. Shared (not rebuilt) so a respawn preserves the mode.
    pub accept_edits: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Session grant store for mutating MCP tools the user approved "always" while in
    /// PLAN mode. Owned here (not rebuilt in [`assemble`]) so a respawn / model-swap
    /// preserves the grants — the same reason the mode flags above are shared.
    pub mcp_plan_grants: std::sync::Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    pub write_approval_grants: std::sync::Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    pub bash_workspace_grants: std::sync::Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    pub sensitive_path_grants: std::sync::Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    /// Provider slot for the `code_review` sub-agent tool, FILLED by [`assemble`] (the tool
    /// is built in `prepare` before the provider exists). Shared so a respawn/model-swap
    /// updates the reviewer's provider too. `None` when `opts.review` was false.
    pub review_provider: Option<SharedReviewProvider>,
    /// Provider slot for the `task` subagent tool, FILLED by [`assemble`] like
    /// `review_provider`. Both tiers currently reuse the host provider (strong/weak
    /// routing is a follow-up that adds a second signed provider in the bridge).
    /// `None` when the `ATOMCODE_SUBAGENT` env gate is off.
    pub subagent_provider: Option<SharedReviewProvider>,
    /// User/project CC external hooks (`$ATOMCODE_HOME/hooks.json` + `<root>/.hooks.json`).
    /// ONE instance is registered as BOTH a [`LifecycleHooks`] (already pushed into `hooks`)
    /// and a [`ToolMiddleware`](atomcode_kernel::middleware::ToolMiddleware) (registered by
    /// [`assemble`], before approval). `None` when no hooks are configured — the common path
    /// adds zero overhead (no registration at all).
    pub cc_external_hooks: Option<Arc<CCExternalHooks>>,
}

/// Phase 1 — gather + connect everything the agent needs (async: MCP connect,
/// snapshot load, skill-dir scans). Errors only on a broken EXPLICIT request
/// (`SessionMode::Resume` whose snapshot can't be read); everything optional
/// degrades gracefully (no `.mcp.json` → no MCP tools; empty skill dirs → none).
pub async fn prepare(cfg: &CodingAgentConfig, opts: PrepareOptions) -> io::Result<CodingParts> {
    prepare_with_plugin_hooks(cfg, opts, Vec::new()).await
}

/// Like [`prepare`], plus `plugin_cc_hooks` — CC hooks contributed INLINE by installed
/// plugins, which the DRIVER resolves (the plugin loader lives in `atomcode-core`, which
/// neither this crate nor L1 may depend on) and threads in here. They are merged with the
/// user/project `hooks.json` into the one [`CCExternalHooks`] runner. Drivers without a
/// plugin system (or with none installed) pass an empty vec and get the same result as
/// [`prepare`].
pub async fn prepare_with_plugin_hooks(
    cfg: &CodingAgentConfig,
    opts: PrepareOptions,
    plugin_cc_hooks: Vec<HookConfig>,
) -> io::Result<CodingParts> {
    let mut registry = ToolRegistry::new();
    let mut names: Vec<String> = Vec::new();

    // Always-on core: neutral fs/bash toolset + codeintel. Vision gating: a VL model
    // (e.g. Qwen3-VL) makes read_file hand image files to the model as pictures. Uses the
    // SAME canonical detector as the user-paste path (`model_name_suggests_vision`) so one
    // model can't accept a pasted image yet refuse a read_file image. NOTE: this is the
    // PREPARE-time flag; `assemble` re-registers read_file on every model swap (see there)
    // so a `/model` change to/from a VL model can't leave it stale.
    register_coding_tools_with_vision(&mut registry, model_name_suggests_vision(&cfg.model));
    names.extend(atomcode_capabilities::tools::coding_tool_names().iter().map(|s| s.to_string()));
    register_codeintel_tools(&mut registry);
    names.extend(
        atomcode_capabilities::codeintel::codeintel_tool_names().iter().map(|s| s.to_string()),
    );

    if opts.web && !atomcode_config::config::offline::is_offline_active() {
        registry.register(Arc::new(WebFetchTool));
        // web_search backend: explicit config wins; else the `ATOMCODE_WEB_SEARCH_PROVIDER`
        // env knob (the zero-core path for legacy drivers, whose config types we can't
        // extend); else Exa. `with_provider` maps unknown values to Exa, the safe default.
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
        registry.register(Arc::new(ReviewTool::new(
            slot.clone(),
            ReviewToolConfig {
                model: cfg.model.clone(),
                context_window: cfg.context_window,
                stream_timeout: cfg.stream_timeout,
                request_timeout: cfg.request_timeout.unwrap_or_else(|| std::time::Duration::from_secs(300)),
                max_commits_without_confirmation: 20,
                max_files_without_confirmation: 40,
                max_changed_lines_without_confirmation: 4_000,
                max_diff_bytes_without_confirmation: 256 * 1024,
                rules_dir: None,
            },
        )));
        names.push("code_review".into());
        Some(slot)
    } else {
        None
    };

    // `task` subagent tool (env-gated, default off). Reuses the HOST provider for both
    // tiers via a slot filled at assemble (mirrors `review_provider`); strong/weak model
    // routing is a follow-up. Child tools: read-only `explore` vs edit-capable `worker`.
    let subagent_provider: Option<SharedReviewProvider> =
        if subagent_enabled_from_env(std::env::var("ATOMCODE_SUBAGENT").ok().as_deref()) {
            use atomcode_capabilities::tools::TaskTool;

            let slot: SharedReviewProvider = Arc::new(std::sync::RwLock::new(None));

            // Child subagent tool registry (mount a subset per type).
            let mut child_reg = atomcode_kernel::tool::ToolRegistry::new();
            atomcode_capabilities::tools::register_coding_tools_with_vision(&mut child_reg, false);
            let child_reg = Arc::new(child_reg);

            let explore_names: Vec<String> =
                ["read_file", "grep", "glob", "list_directory"].iter().map(|s| s.to_string()).collect();
            let worker_names: Vec<String> = [
                "read_file", "edit_file", "write_file", "bash", "grep", "glob", "search_replace",
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

            // Prefer a bridge-injected tier provider, else fall back to the host-provider
            // slot (filled at assemble — the single-model / same-as-host collapse path). The
            // tier provider is a SHARED, swap-aware cell ([`TierProvider`]): it builds lazily on
            // first `task` use (startup never pays the reqwest-client cost) and its cache is
            // reset by the bridge on a `/model` swap, so routing re-resolves without a respawn.
            let fast_cell = cfg.subagent_fast_provider.clone();
            let cap_cell = cfg.subagent_capable_provider.clone();
            let slot_fast = slot.clone();
            let slot_cap = slot.clone();
            let make_fast = move || {
                fast_cell.as_ref().and_then(|c| c.get()).unwrap_or_else(|| {
                    slot_fast.read().ok().and_then(|g| g.clone())
                        .expect("subagent provider slot filled at assemble before any turn")
                })
            };
            let make_capable = move || {
                cap_cell.as_ref().and_then(|c| c.get()).unwrap_or_else(|| {
                    slot_cap.read().ok().and_then(|g| g.clone())
                        .expect("subagent provider slot filled at assemble before any turn")
                })
            };

            let subtask_timeout = subagent_timeout_from_env(
                std::env::var("ATOMCODE_SUBAGENT_TIMEOUT").ok().as_deref(),
            );
            registry.register(Arc::new(
                TaskTool::new(make_fast, make_capable, make_explore_tools, make_worker_tools)
                    .with_subtask_timeout(subtask_timeout),
            ));
            names.push("task".to_string());
            Some(slot)
        } else {
            None
        };

    // Skills: standard home+project precedence unless the caller supplied dirs.
    let skill_dirs = opts.skill_dirs.clone().unwrap_or_else(|| {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        standard_skill_dirs(&home, &cfg.working_dir)
    });
    let skills = Arc::new(SkillRegistry::load(&skill_dirs));
    register_skill_tools(&mut registry, skills);
    names.extend(atomcode_capabilities::skills::skill_tool_names().iter().map(|s| s.to_string()));

    // MCP: eager connect PRE-spawn (frozen MountedTools / stable tool-list prefix).
    let (mcp_registry, mcp_events) = if opts.mcp {
        let (reg, adapters, events) = mcp::connect_and_adapt(&cfg.working_dir).await;
        names.extend(mcp::register_mcp_tools(&mut registry, adapters));
        (Some(reg), events)
    } else {
        (None, Vec::new())
    };

    // Session binding: the id's single owner.
    let session = match &opts.session {
        SessionMode::Disabled => None,
        SessionMode::Fresh => Some(SessionBinding {
            id: uuid::Uuid::new_v4().to_string(),
            manager: Arc::new(SessionManager::for_project(&cfg.working_dir)),
            resume: None,
        }),
        SessionMode::Resume(id) => {
            let manager = Arc::new(SessionManager::for_project(&cfg.working_dir));
            match manager.load_snapshot(id) {
                Ok(snap) => {
                    // A version-mismatched snapshot must FAIL here, not fall through
                    // to the kernel's empty-start seam — that would silently fresh-
                    // start under the SAME session id and corrupt on-disk state.
                    check_snapshot_version(&snap)?;
                    Some(SessionBinding { id: id.clone(), manager, resume: Some(snap) })
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    // Snapshot not found (possibly due to a previous save_snapshot
                    // IO failure that was logged and skipped). Downgrade to Fresh:
                    // start a new session under a new id rather than crashing.
                    let fresh_id = uuid::Uuid::new_v4().to_string();
                    Some(SessionBinding { id: fresh_id, manager, resume: None })
                }
                Err(e) => return Err(e),
            }
        }
    };

    if let Some(b) = &session {
        registry
            .register(Arc::new(RecallTool::new().with_sessions_dir(b.manager.root())));
        names.push("recall".into());
    }

    // Hooks in the CANONICAL ORDER (registration order = HookChain execution order):
    // 1. SessionContextHook — session_start: inject env + project-instructions + git
    //    snapshot after persona. Rewrites the leading-system run (like MemoryHook); runs
    //    FIRST so the order is persona → context → memory.
    // 2. MemoryHook    — session_start: inject memory.md after the leading-system run
    //    (fresh inject / resume reconcile). Both 1 and 2 reconcile by their own header
    //    prefix, so they compose (the insert position is computed live each time).
    // 3. SnapshotHook  — turn_complete: persist .snapshot + .meta.
    // 4. TranscriptHook— turn_complete: append the .jsonl record. (No coupling with
    //    3 — the order is fixed purely for determinism.)
    // 5. StatusReminderHook — pre_request tail-append (cache red-line: tail only, and
    //    SKIPPED on a turn's round 1 so it never pairs with the user message).
    // 6. VerifyCadenceHook — offer_continuation; FIRST `Some` wins in the chain, so
    //    keep it last: any earlier hook's continuation outranks the cadence nudge.
    let mut hooks: Vec<Arc<dyn LifecycleHooks>> = Vec::new();
    let mut compaction_checkpoint: Option<Arc<dyn CompactionCheckpoint>> = None;
    // Env / project-instructions / git context — unconditional (v1 parity: always present).
    hooks.push(Arc::new(SessionContextHook::new(&cfg.working_dir)));
    if opts.memory {
        hooks.push(Arc::new(MemoryHook::for_project(&cfg.working_dir)));
    }
    if let Some(b) = &session {
        let wd = cfg.working_dir.to_string_lossy().into_owned();
        let snapshot_hook = Arc::new(SnapshotHook::new(b.manager.clone(), &b.id, &wd));
        compaction_checkpoint = Some(snapshot_hook.clone());
        hooks.push(snapshot_hook);
        hooks.push(Arc::new(TranscriptHook::new(b.manager.clone(), &b.id)));
    }
    // Status awareness is UNCONDITIONAL (production parity): a per-turn <system-reminder>
    // with date + round budget (NO context-usage gauge — pressure is handled silently by
    // auto-compaction, never pushed to the model). Serves recall's relative-date resolution and
    // lets the model pace itself. Injected from round 2 of each turn (round 1 is skipped — see
    // StatusReminderHook — to avoid a user-after-user wire pair).
    hooks.push(Arc::new(StatusReminderHook::new()));
    // Pin the workspace root the cadence uses to gate out-of-workspace edits (e.g. a throwaway
    // /tmp write must not arm the "run cargo check" nudge). INVARIANT: this must equal the dir
    // the edit/write tools resolve relative `file_path` against — they stay in lockstep because
    // `/cd` respawns the agent (rebuilding this hook with the new dir), not by mutating cwd in
    // place. If `/cd` ever moves to an in-place cwd mutation, thread the live cwd in here too.
    hooks.push(Arc::new(VerifyCadenceHook::new(cfg.working_dir.clone())));
    // Todo hook (bridge/daemon path — the live TUI + webui): per-turn <system-reminder> of the
    // current list so the model keeps it accurate after compaction, PLUS an `offer_continuation`
    // that nudges once to close out open items when the model tries to stop. Gated on the SAME
    // ATOMCODE_TODO switch as the todowrite/todo tools + persona guidance (so the reminder never
    // references tools that aren't mounted). Pushed AFTER VerifyCadenceHook so verify's
    // "first Some wins" continuation outranks the todo-completion nudge. This is the ONLY
    // production registration of TodoHook — every real entrypoint (bridge, daemon, clix) goes
    // through prepare()/assemble() here; `assemble.rs::build_coding_agent` (which also registers
    // it) is reachable only from tests + examples, so there is no double-registration.
    if crate::persona::todo_switch_enabled() {
        hooks.push(Arc::new(crate::todo::TodoHook));
    }
    // NOTE: the `RateLimitHook` is NOT built here. It gates CodingPlan-specific 429
    // messaging on `cfg.base_url` being the gateway, so — like the turn-level
    // `TelemetryHook` — it must be built in `assemble` (which re-runs on a /model
    // swap), NOT here in `prepare` (which does not). A prepare-frozen base_url would
    // keep mislabelling an external-model 429 as a CodingPlan quota after a switch.
    // CC external hooks: user/project `hooks.json` + plugin-contributed inline hooks
    // (`plugin_cc_hooks`, resolved by the driver) on the kernel seams — the port of core's
    // CC-parity hook engine onto the v2 engine. ONE instance serves both seams: pushed here
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

    Ok(CodingParts {
        shared_cwd: std::sync::Arc::new(std::sync::RwLock::new(cfg.working_dir.clone())),
        plan_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        bypass_mode: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        accept_edits: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
        registry,
        tool_names: names,
        approval: Arc::new(ApprovalMiddleware::in_memory()),
        hooks,
        compaction_checkpoint,
        session,
        mcp_registry,
        mcp_events,
        review_provider,
        subagent_provider,
        cc_external_hooks: cc_external,
    })
}

impl CodingParts {
    /// Mount the full toolset (fresh `MountedTools` per call — it is not `Clone`;
    /// the underlying tools are shared `Arc`s).
    fn mount(&self) -> MountedTools {
        let names: Vec<&str> = self.tool_names.iter().map(String::as_str).collect();
        self.registry.mount(&names)
    }

    /// Register an EXTRA driver-contributed tool into the kernel toolset, so it is
    /// both resolvable during a turn AND exposed to the model (added to `tool_names`,
    /// which [`mount`](Self::mount) reads). The `registry` / `tool_names` fields are
    /// crate-private — this is the supported seam for a driver (the bridge) to inject
    /// a tool the always-on capability set doesn't include.
    ///
    /// Idempotent on name: re-registering the same name (e.g. on a respawn that
    /// re-injects the bridge's `schedule_wakeup`) replaces the tool in the registry
    /// and does NOT duplicate the name, keeping the mounted tool list (a cache prefix)
    /// byte-stable across respawns.
    ///
    /// Call BEFORE [`assemble`] (it snapshots the toolset via `mount`). The bridge
    /// uses this for its kernel-side `schedule_wakeup` (`/loop`).
    pub fn register_extra_tool(&mut self, tool: Arc<dyn atomcode_kernel::tool::Tool>) {
        let name = tool.name().to_string();
        if !self.tool_names.iter().any(|n| n == &name) {
            self.tool_names.push(name);
        }
        self.registry.register(tool);
    }
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
/// - nothing-persisted-yet (`NotFound`) is NOT an error: a fresh session's first
///   assemble starts empty by design.
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
    // Model swap (e.g. `/model`) routes here via the bridge WITHOUT re-running `prepare`,
    // so re-register `read_file` with the CURRENT model's vision capability — otherwise the
    // PREPARE-time flag goes stale and a text-only model could receive a base64 image (or a
    // VL model none). `register` overwrites by name, so this idempotently refreshes the one
    // tool whose behavior depends on the model. Same model-swap-refresh pattern as the
    // `review_provider` slot below.
    parts
        .registry
        .register(Arc::new(ReadFileTool::new(model_name_suggests_vision(&cfg.model))));

    // Session-bound: reload the LATEST snapshot (turn 1 of a fresh session: none
    // yet → NotFound → start empty). Anything else unreadable is a real failure.
    if let Some(b) = &mut parts.session {
        match b.manager.load_snapshot(&b.id) {
            Ok(mut snap) => {
                check_snapshot_version(&snap)?;
                reconcile_coding_persona(&mut snap, &cfg.model);
                b.resume = Some(snap);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }

    // A telemetry-metering decorator over the host provider. Calls made OUTSIDE the host
    // agent loop — the tier-2 overflow summary AND the `code_review` sub-agent's rounds —
    // never reach the turn-level TelemetryHook (which fires on this loop's on_request /
    // on_model_response), so without this their token spend is invisible. The host loop's
    // PRIMARY provider stays bare below: the TelemetryHook already meters it, and wrapping
    // it too would double-count. `None` ⇒ telemetry off ⇒ the bare provider (zero overhead).
    let metered_provider: Arc<dyn LlmProvider> = match &cfg.telemetry {
        Some(tel) => Arc::new(crate::telemetry::MeteredProvider::new(
            provider.clone(),
            tel.clone(),
            cfg.provider_type.as_str(),
            &cfg.base_url,
            &cfg.model,
            parts.session.as_ref().map(|b| b.id.as_str()),
        )),
        None => provider.clone(),
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
                    provider.clone(),
                    tel.clone(),
                    cfg.provider_type.as_str(),
                    &cfg.base_url,
                    &cfg.model,
                    parts.session.as_ref().map(|b| b.id.as_str()),
                )
                .with_surface("code_review"),
            ),
            None => provider.clone(),
        };
        if let Ok(mut g) = slot.write() {
            *g = Some(review_provider);
        }
    }

    if let Some(slot) = &parts.subagent_provider {
        let sub_provider: Arc<dyn LlmProvider> = match &cfg.telemetry {
            Some(tel) => Arc::new(
                crate::telemetry::MeteredProvider::new(
                    provider.clone(),
                    tel.clone(),
                    cfg.provider_type.as_str(),
                    &cfg.base_url,
                    &cfg.model,
                    parts.session.as_ref().map(|b| b.id.as_str()),
                )
                .with_surface("subagent"),
            ),
            None => provider.clone(),
        };
        if let Ok(mut g) = slot.write() {
            *g = Some(sub_provider);
        }
    }

    // Tier-2 overflow summary uses the same metered provider so its summary LLM call is
    // likewise counted.
    let summary_provider = metered_provider;
    let mut builder = Agent::builder()
        .provider(provider)
        .tools(parts.mount())
        .persona(coding_persona(&cfg.model, crate::persona::todo_switch_enabled()));
    // Tool telemetry registers FIRST. It is observation-only — it never rewrites args
    // or blocks — so its position does not affect the approve-what-runs contract (an
    // ARG-REWRITING gate, e.g. CC PreToolUse `updatedInput`, must instead sit BEFORE
    // approval so the user approves the POST-rewrite bytes — see the CC hooks block
    // below). Going first means its `before` always stamps the call, so a tool that
    // approval then DENIES is still recorded (the after-chain runs for every middleware).
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
    let mut builder = builder
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
        .hook(Arc::new(crate::plan_mode::PlanModeReminderHook::new(parts.plan_mode.clone())))
        // Rate-limit hook: on a 429 it decides wait-vs-pause from CodingPlan usage windows.
        // Built HERE (not in `prepare`) — like TelemetryHook — so a /model swap (which re-runs
        // assemble only) re-captures the CURRENT provider's base_url. That base_url is the gate
        // that keeps a user's external-model 429 from being mislabelled as a CodingPlan quota;
        // a prepare-frozen base_url would defeat it after a model switch.
        .hook(Arc::new(crate::rate_limit::RateLimitHook::new(cfg.base_url.clone())))
        // Sensitive-path read gate: read tools are Safe (skip approval), so without this an
        // agent could silently read ~/.ssh / .env / creds and leak them to the provider.
        // Acts ONLY on Safe tools touching a sensitive path → one approval round-trip.
        .middleware(Arc::new(SensitivePathGate::with_store(
            parts.sensitive_path_grants.clone(),
        )));
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
        .middleware(Arc::new(OpenFileWorkspaceGate::new(parts.shared_cwd.clone())))
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
        .compaction(Arc::new(atomcode_capabilities::compaction::OverflowCompaction::new(
            atomcode_capabilities::compaction::StubCompaction::default(),
            Some(summary_provider),
        )))
        .compact_threshold(cfg.compact_threshold)
        .stream_timeout(cfg.stream_timeout)
        .max_continuations(cfg.max_continuations)
        // Ctrl-C semantics: false = UNDO (default), true = PRESERVE the interrupted turn.
        .keep_interrupted_context(cfg.keep_interrupted_context);
    // Approval liveness: `Some(d)` ⇒ fail-closed after `d` (headless); `None` ⇒ PARK until
    // answered (interactive — a present human must not be auto-denied). The kernel defaults
    // to unbounded when `.request_timeout` is never set, so None = park.
    if let Some(d) = cfg.request_timeout {
        builder = builder.request_timeout(d);
    }
    for h in &parts.hooks {
        builder = builder.hook(h.clone());
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
        if let Some(snap) = &b.resume {
            builder = builder.resume(snap.clone());
        }
    }
    Ok(builder.build())
}

const ATOMCODE_PERSONA_PREFIX: &str =
    "You are AtomCode, an AI coding agent by AtomGit running the ";

/// Legacy drivers persist conversation history without the separately supplied
/// system prompt. A v2 resume must restore that prompt, while a model switch must
/// replace the old model identity instead of retaining or duplicating it.
fn reconcile_coding_persona(snapshot: &mut SessionSnapshot, model: &str) {
    let persona = coding_persona(model, crate::persona::todo_switch_enabled());
    let is_persona = |message: &Message| {
        message.role == Role::System && message.text.starts_with(ATOMCODE_PERSONA_PREFIX)
    };
    let already_current = snapshot
        .messages
        .first()
        .is_some_and(|message| message.role == Role::System && message.text == persona)
        && snapshot.messages.iter().skip(1).all(|message| !is_persona(message));
    if already_current {
        return;
    }

    snapshot.messages.retain(|message| !is_persona(message));
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

/// env `ATOMCODE_SUBAGENT` gate: default OFF; `0`/`false`/`off`/empty (case-insensitive)
/// = off, any other non-empty value = on. (Opposite default from `ATOMCODE_TODO`.)
pub fn subagent_enabled_from_env(var: Option<&str>) -> bool {
    match var {
        None => false,
        Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "off"),
    }
}

/// Per-subtask wall-clock timeout from env `ATOMCODE_SUBAGENT_TIMEOUT` (whole seconds).
/// Default 900s (15 min). Unset / empty / unparseable → default. A tiny value would kill
/// every subtask, so anything below 30s is floored to 30s (a deliberate footgun guard).
pub fn subagent_timeout_from_env(var: Option<&str>) -> std::time::Duration {
    const DEFAULT_SECS: u64 = 900;
    const MIN_SECS: u64 = 30;
    let secs = var
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&s| s > 0)
        .map(|s| s.max(MIN_SECS))
        .unwrap_or(DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CodingAgentConfig;

    #[test]
    fn subagent_env_gate() {
        use super::subagent_enabled_from_env as g;
        assert!(!g(None));
        assert!(!g(Some("0")));
        assert!(!g(Some("false")));
        assert!(!g(Some("off")));
        assert!(!g(Some("")));
        assert!(g(Some("1")));
        assert!(g(Some("true")));
        assert!(g(Some("yes")));
    }

    #[test]
    fn subagent_timeout_env_parsing() {
        use super::subagent_timeout_from_env as t;
        use std::time::Duration;
        // Default when unset / empty / non-numeric.
        assert_eq!(t(None), Duration::from_secs(900));
        assert_eq!(t(Some("")), Duration::from_secs(900));
        assert_eq!(t(Some("abc")), Duration::from_secs(900));
        assert_eq!(t(Some("0")), Duration::from_secs(900)); // 0 rejected → default
        // Valid values, with whitespace tolerated.
        assert_eq!(t(Some("600")), Duration::from_secs(600));
        assert_eq!(t(Some("  1200 ")), Duration::from_secs(1200));
        // Footgun guard: anything under 30s is floored to 30s.
        assert_eq!(t(Some("5")), Duration::from_secs(30));
    }

    #[test]
    fn resume_adds_persona_before_legacy_session_context() {
        let mut snapshot = SessionSnapshot::new(vec![Message::system("SESSION CONTEXT")]);

        reconcile_coding_persona(&mut snapshot, "deepseek-v4-flash");

        assert!(snapshot.messages[0].text.contains("running the deepseek-v4-flash model"));
        assert_eq!(snapshot.messages[1].text, "SESSION CONTEXT");
        assert_eq!(snapshot.cache_epoch, 1);
    }

    #[test]
    #[serial_test::serial(offline_verdict)]
    fn model_switch_replaces_persona_without_duplication() {
        atomcode_config::config::offline::reset_offline_verdict_for_test();
        let mut snapshot = SessionSnapshot::new(vec![
            Message::system(coding_persona("old-model", crate::persona::todo_switch_enabled())),
            Message::system("SESSION CONTEXT"),
        ]);

        reconcile_coding_persona(&mut snapshot, "deepseek-v4-flash");

        let personas = snapshot
            .messages
            .iter()
            .filter(|message| message.text.starts_with(ATOMCODE_PERSONA_PREFIX))
            .count();
        assert_eq!(personas, 1);
        assert!(snapshot.messages[0].text.contains("running the deepseek-v4-flash model"));
        assert!(!snapshot.messages[0].text.contains("old-model"));
        assert_eq!(snapshot.cache_epoch, 1);
    }

    #[test]
    #[serial_test::serial(offline_verdict)]
    fn current_persona_keeps_snapshot_byte_stable() {
        atomcode_config::config::offline::reset_offline_verdict_for_test();
        let persona = coding_persona("deepseek-v4-flash", crate::persona::todo_switch_enabled());
        let mut snapshot = SessionSnapshot::new(vec![
            Message::system(persona.clone()),
            Message::system("SESSION CONTEXT"),
        ]);

        reconcile_coding_persona(&mut snapshot, "deepseek-v4-flash");

        assert_eq!(snapshot.messages[0].text, persona);
        assert_eq!(snapshot.cache_epoch, 0);
    }

    /// `prepare` with all optional capabilities OFF — keeps the call I/O-free (no MCP
    /// connect, no session/skill/home scans) so the test only exercises CC-hook wiring.
    fn io_free_opts() -> PrepareOptions {
        PrepareOptions {
            session: SessionMode::Disabled,
            skill_dirs: Some(vec![]),
            mcp: false,
            memory: false,
            web: false,
            review: false,
        }
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

        let ephemeral = prepare(&cfg, io_free_opts()).await.unwrap();
        assert!(ephemeral.compaction_checkpoint.is_none());
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
        assert!(parts.cc_external_hooks.is_none(), "no hooks.json ⇒ nothing registered");
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
        assert!(parts.cc_external_hooks.is_some(), "project .hooks.json ⇒ wired");
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
                StreamEvent::Usage(TokenUsage { prompt: 500, completion: 30, cached: 0 }),
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

        let slot = parts.review_provider.clone().expect("review enabled ⇒ slot present");
        let review_provider =
            slot.read().unwrap().clone().expect("slot filled at assemble");
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
}
