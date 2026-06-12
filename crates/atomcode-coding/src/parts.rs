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

use atomcode_capabilities::codeintel::register_codeintel_tools;
use atomcode_capabilities::mcp::{self, McpConnectEvent, McpRegistry};
use atomcode_capabilities::memory::MemoryHook;
use atomcode_capabilities::session::{
    CurrentDateHook, RecallTool, SessionManager, SnapshotHook, TranscriptHook,
};
use atomcode_capabilities::skills::{register_skill_tools, standard_skill_dirs, SkillRegistry};
use atomcode_capabilities::tools::{register_coding_tools, ApprovalMiddleware, WebFetchTool, WebSearchTool};
use atomcode_kernel::agent::Agent;
use atomcode_kernel::hook::LifecycleHooks;
use atomcode_kernel::message::SessionSnapshot;
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::tool::{MountedTools, ToolRegistry};

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
}

impl Default for PrepareOptions {
    fn default() -> Self {
        Self { session: SessionMode::Fresh, skill_dirs: None, mcp: true, memory: true, web: true }
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
}

/// Phase 1 — gather + connect everything the agent needs (async: MCP connect,
/// snapshot load, skill-dir scans). Errors only on a broken EXPLICIT request
/// (`SessionMode::Resume` whose snapshot can't be read); everything optional
/// degrades gracefully (no `.mcp.json` → no MCP tools; empty skill dirs → none).
pub async fn prepare(cfg: &CodingAgentConfig, opts: PrepareOptions) -> io::Result<CodingParts> {
    let mut registry = ToolRegistry::new();
    let mut names: Vec<String> = Vec::new();

    // Always-on core: neutral fs/bash toolset + codeintel.
    register_coding_tools(&mut registry);
    names.extend(atomcode_capabilities::tools::coding_tool_names().iter().map(|s| s.to_string()));
    register_codeintel_tools(&mut registry);
    names.extend(
        atomcode_capabilities::codeintel::codeintel_tool_names().iter().map(|s| s.to_string()),
    );

    if opts.web {
        registry.register(Arc::new(WebFetchTool));
        registry.register(Arc::new(WebSearchTool::new()));
        names.push("web_fetch".into());
        names.push("web_search".into());
    }

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
            let snap = manager.load_snapshot(id)?;
            // A version-mismatched snapshot must FAIL here, not fall through to the
            // kernel's empty-start seam — that would silently fresh-start under the
            // SAME session id and corrupt the session's on-disk state.
            check_snapshot_version(&snap)?;
            Some(SessionBinding { id: id.clone(), manager, resume: Some(snap) })
        }
    };

    if let Some(b) = &session {
        registry
            .register(Arc::new(RecallTool::new().with_sessions_dir(b.manager.root())));
        names.push("recall".into());
    }

    // Hooks in the CANONICAL ORDER (registration order = HookChain execution order):
    // 1. MemoryHook    — the ONLY hook that rewrites the leading-system run at
    //    session_start (fresh inject after persona / resume reconcile) → must run
    //    before anything else observes the conversation.
    // 2. SnapshotHook  — turn_complete: persist .snapshot + .meta.
    // 3. TranscriptHook— turn_complete: append the .jsonl record. (No coupling with
    //    2 — the order is fixed purely for determinism.)
    // 4. CurrentDateHook — pre_request tail-append (cache red-line: tail only).
    // 5. VerifyCadenceHook — offer_continuation; FIRST `Some` wins in the chain, so
    //    keep it last: any earlier hook's continuation outranks the cadence nudge.
    let mut hooks: Vec<Arc<dyn LifecycleHooks>> = Vec::new();
    if opts.memory {
        hooks.push(Arc::new(MemoryHook::for_project(&cfg.working_dir)));
    }
    if let Some(b) = &session {
        let wd = cfg.working_dir.to_string_lossy().into_owned();
        hooks.push(Arc::new(SnapshotHook::new(b.manager.clone(), &b.id, &wd)));
        hooks.push(Arc::new(TranscriptHook::new(b.manager.clone(), &b.id)));
    }
    // Date awareness is UNCONDITIONAL (production parity): it serves recall's
    // relative-date resolution when sessions are on, but the model should know the
    // date in a Disabled-session CI run too.
    hooks.push(Arc::new(CurrentDateHook::new()));
    hooks.push(Arc::new(VerifyCadenceHook::new()));
    // 6. TelemetryHook — observation-only (on_request + on_model_response): emits
    //    LlmChat per round. Last in the chain; it mutates nothing, so order is moot.
    //    Only when the driver supplied a telemetry sink (kernel stays zero-telemetry
    //    otherwise). The new stack always speaks OpenAI-compat → vendor "openai".
    if let Some(tel) = &cfg.telemetry {
        hooks.push(Arc::new(crate::telemetry::TelemetryHook::new(
            tel.clone(),
            "openai",
            &cfg.base_url,
            &cfg.model,
            session.as_ref().map(|b| b.id.as_str()),
        )));
    }

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
    // Session-bound: reload the LATEST snapshot (turn 1 of a fresh session: none
    // yet → NotFound → start empty). Anything else unreadable is a real failure.
    if let Some(b) = &mut parts.session {
        match b.manager.load_snapshot(&b.id) {
            Ok(snap) => {
                check_snapshot_version(&snap)?;
                b.resume = Some(snap);
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }

    let mut builder = Agent::builder()
        .provider(provider)
        .tools(parts.mount())
        .persona(coding_persona(&cfg.model));
    // Tool telemetry registers BEFORE approval. It is observation-only — it never
    // rewrites args or blocks — so it does NOT affect the approve-what-runs contract
    // (which only requires ARG-REWRITING middleware to sit after approval). Going
    // first means its `before` always stamps the call, so a tool that approval then
    // DENIES is still recorded (the after-chain runs for every middleware).
    if let Some(tel) = &cfg.telemetry {
        builder = builder.middleware(Arc::new(crate::telemetry::ToolTelemetryMiddleware::new(
            tel.clone(),
            "openai",
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
        // Approval BEFORE any arg-rewriting middleware — the user approves the exact
        // bytes that run.
        .middleware(parts.approval.clone())
        // LIVE cwd handle (not the immutable pin): /cd mutates parts.shared_cwd.
        .working_dir_shared(parts.shared_cwd.clone())
        .chat_options(cfg.chat_options.clone())
        .stream_timeout(cfg.stream_timeout)
        .request_timeout(cfg.request_timeout)
        .max_continuations(cfg.max_continuations);
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
