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
    register_coding_tools_with_vision, ApprovalMiddleware, OpenFileWorkspaceGate, ReadFileTool,
    SensitivePathGate, WebFetchTool, WebSearchTool, WriteApprovalGate,
};
use atomcode_core::provider::model_name_suggests_vision;
use atomcode_kernel::agent::Agent;
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
    /// Provider slot for the `code_review` sub-agent tool, FILLED by [`assemble`] (the tool
    /// is built in `prepare` before the provider exists). Shared so a respawn/model-swap
    /// updates the reviewer's provider too. `None` when `opts.review` was false.
    pub review_provider: Option<SharedReviewProvider>,
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

    if opts.web {
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
                // The review sub-agent isn't the interactive approval surface; keep a
                // concrete fail-closed bound even when the main agent parks (None).
                request_timeout: cfg.request_timeout.unwrap_or_else(|| std::time::Duration::from_secs(300)),
                rules_dir: None,
            },
        )));
        names.push("code_review".into());
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
    // Env / project-instructions / git context — unconditional (v1 parity: always present).
    hooks.push(Arc::new(SessionContextHook::new(&cfg.working_dir)));
    if opts.memory {
        hooks.push(Arc::new(MemoryHook::for_project(&cfg.working_dir)));
    }
    if let Some(b) = &session {
        let wd = cfg.working_dir.to_string_lossy().into_owned();
        hooks.push(Arc::new(SnapshotHook::new(b.manager.clone(), &b.id, &wd)));
        hooks.push(Arc::new(TranscriptHook::new(b.manager.clone(), &b.id)));
    }
    // Status awareness is UNCONDITIONAL (production parity): a per-turn <system-reminder>
    // with date + context-window usage + round budget. Serves recall's relative-date
    // resolution and lets the model pace itself. Injected from round 2 of each turn (round 1
    // is skipped — see StatusReminderHook — to avoid a user-after-user wire pair).
    hooks.push(Arc::new(StatusReminderHook::new()));
    hooks.push(Arc::new(VerifyCadenceHook::new()));
    // Rate-limit hook: on a 429 it fetches CodingPlan usage windows and picks the
    // right wait-vs-pause decision (decide_from_windows). Returns None for
    // non-CodingPlan providers / fetch failures so the kernel falls back to its
    // hint-based default with zero behavior change.
    hooks.push(Arc::new(crate::rate_limit::RateLimitHook::new()) as Arc<dyn LifecycleHooks>);
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
        registry,
        tool_names: names,
        approval: Arc::new(ApprovalMiddleware::in_memory()),
        hooks,
        session,
        mcp_registry,
        mcp_events,
        review_provider,
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

    // Tier-2 overflow summary uses the same metered provider so its summary LLM call is
    // likewise counted.
    let summary_provider = metered_provider;
    let mut builder = Agent::builder()
        .provider(provider)
        .tools(parts.mount())
        .persona(coding_persona(&cfg.model));
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
        .middleware(Arc::new(crate::plan_mode::PlanModeGate::new(parts.plan_mode.clone())))
        // Plan-mode reminder (ephemeral request tail) — pairs with the gate: the gate
        // blocks mutating TOOLS, this keeps the model PLANNING instead of writing the
        // implementation inline. Shares the same plan_mode flag; cache-safe (tail only).
        .hook(Arc::new(crate::plan_mode::PlanModeReminderHook::new(parts.plan_mode.clone())))
        // Sensitive-path read gate: read tools are Safe (skip approval), so without this an
        // agent could silently read ~/.ssh / .env / creds and leak them to the provider.
        // Acts ONLY on Safe tools touching a sensitive path → one approval round-trip.
        .middleware(Arc::new(SensitivePathGate::new()));
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
        .middleware(Arc::new(WriteApprovalGate::new(parts.shared_cwd.clone())))
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
    if let Some(b) = &parts.session {
        builder = builder.session_id(&b.id);
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
    let persona = coding_persona(model);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CodingAgentConfig;

    #[test]
    fn resume_adds_persona_before_legacy_session_context() {
        let mut snapshot = SessionSnapshot::new(vec![Message::system("SESSION CONTEXT")]);

        reconcile_coding_persona(&mut snapshot, "deepseek-v4-flash");

        assert!(snapshot.messages[0].text.contains("running the deepseek-v4-flash model"));
        assert_eq!(snapshot.messages[1].text, "SESSION CONTEXT");
        assert_eq!(snapshot.cache_epoch, 1);
    }

    #[test]
    fn model_switch_replaces_persona_without_duplication() {
        let mut snapshot = SessionSnapshot::new(vec![
            Message::system(coding_persona("old-model")),
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
    fn current_persona_keeps_snapshot_byte_stable() {
        let persona = coding_persona("deepseek-v4-flash");
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

        std::env::remove_var("ATOMCODE_HOME");
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

        std::env::remove_var("ATOMCODE_HOME");
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

        std::env::remove_var("ATOMCODE_HOME");
    }
}
