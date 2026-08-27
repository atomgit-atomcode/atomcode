//! `atomcodex code` — the new stack's first INTERACTIVE coding-agent driver (D layer).
//!
//! Drives the FULL assembly (`atomcode_coding::prepare` → `assemble`): web + skills +
//! mcp + session persistence/recall + memory, zero `atomcode-core`. The driver owns
//! exactly what the engine deliberately left to it:
//! - the approval UX (the typed `ApprovalRequest`/`ApprovalResponse` round-trip),
//! - slash commands (`/remember` `/forget` `/memory` `/compact` `/mcp` `/sessions`),
//! - rendering the event stream (text deltas, tool trace, turn outcome),
//! - Ctrl-C → `Cancel` (one LONG-LIVED SIGINT listener feeds a channel, so it works
//!   mid-stream, at the approval prompt — where the kernel flushes the pending
//!   round-trip to deny — and at the idle prompt, where it exits gracefully).
//!
//! Everything else (persistence, resume, memory injection, compaction timing,
//! turn-id continuity) happens inside the engine — this loop is deliberately thin.

use anyhow::{bail, Context, Result};
use atomcode_capabilities::memory::MemoryStore;
use atomcode_capabilities::session::SessionManager;
use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
use atomcode_coding::{
    CodingAgentConfig, CodingRuntime, CodingRuntimeEvent, CodingRuntimeEvents, CodingRuntimeHandle,
    CodingRuntimeStart, DefaultCodingProviderFactory, PrepareOptions, SessionMode,
    StaticPluginHookSource, TurnCompletion, UserInput,
};
use atomcode_kernel::event::{AgentEvent, StopReason};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

#[derive(Parser)]
pub struct CodeArgs {
    /// One-shot prompt: run a single turn, print the answer, exit (the session is
    /// still persisted and resumable).
    #[arg(short = 'p', long)]
    pub prompt: Option<String>,
    /// Working directory the agent's tools are scoped to.
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
    /// Resume a session by id (see `atomcodex sessions`).
    #[arg(long, conflicts_with = "continue_latest")]
    pub resume: Option<String>,
    /// Resume the most recently updated session of this project.
    #[arg(long = "continue", conflicts_with = "resume")]
    pub continue_latest: bool,
    /// Auto-approve every risky tool call (CI / trusted runs).
    #[arg(long)]
    pub yolo: bool,
    /// Skip MCP server connection.
    #[arg(long)]
    pub no_mcp: bool,
    /// Skip memory.md injection.
    #[arg(long)]
    pub no_memory: bool,
    /// Skip web_fetch / web_search tools.
    #[arg(long)]
    pub no_web: bool,
    /// Model id (overrides $ATOMCODE_MODEL).
    #[arg(long)]
    pub model: Option<String>,
    /// Provider API key (overrides $ATOMCODE_API_KEY).
    #[arg(long)]
    pub api_key: Option<String>,
    /// Provider base URL (overrides $ATOMCODE_BASE_URL).
    #[arg(long)]
    pub base_url: Option<String>,
    /// Named `[providers.<name>]` config entry (overrides `default_provider`).
    #[arg(long)]
    pub provider: Option<String>,
    /// Config file path (default: ~/.atomcode/config.toml).
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Disable anonymous usage telemetry for this invocation.
    #[arg(long = "no-telemetry")]
    pub no_telemetry: bool,
    /// Max seconds to wait for each stream event (liveness guard).
    #[arg(long, default_value_t = 180)]
    pub stream_timeout: u64,
}

/// `atomcodex sessions` — list this project's resumable sessions, newest first.
#[derive(Parser)]
pub struct SessionsArgs {
    /// Project directory (default: current directory).
    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
}

pub fn sessions(args: SessionsArgs) -> Result<()> {
    let dir = args.dir.canonicalize().context("project dir not found")?;
    let mgr = SessionManager::for_project(&dir);
    let list = mgr.list();
    if list.is_empty() {
        println!(
            "No sessions for {} (bucket: {})",
            dir.display(),
            mgr.root().display()
        );
        return Ok(());
    }
    for m in list {
        println!(
            "{}  {:<28} turns {:<4} {}",
            m.id,
            m.name,
            m.turn_count,
            fmt_ts(m.updated_at)
        );
    }
    Ok(())
}

pub async fn code(args: CodeArgs) -> Result<()> {
    let dir = args.dir.canonicalize().context("working dir not found")?;

    // Provider creds: flag > env > config.toml (same resolution as `review`).
    let selected = crate::load_config_selection(args.config.as_deref(), args.provider.as_deref())?;
    let entry = selected.provider.as_ref();
    let base_url = crate::first_nonempty([
        args.base_url.clone(),
        crate::env("ATOMCODE_BASE_URL"),
        entry
            .and_then(|e| e.base_url.clone())
            .map(|v| crate::expand_env(&v)),
    ])
    .context(
        "missing base URL: pass --base-url, set $ATOMCODE_BASE_URL, or configure a provider",
    )?;
    let model = crate::first_nonempty([
        args.model.clone(),
        crate::env("ATOMCODE_MODEL"),
        entry
            .and_then(|e| e.model.clone())
            .map(|v| crate::expand_env(&v)),
    ])
    .context("missing model: pass --model, set $ATOMCODE_MODEL, or configure a provider")?;
    if crate::is_signing_gateway(&base_url) {
        bail!(
            "provider base_url '{base_url}' needs AtomCode's proprietary request signing, \
             which atomcodex cannot produce — use a plain OpenAI-compatible endpoint"
        );
    }
    let api_key = crate::first_nonempty([
        args.api_key.clone(),
        crate::env("ATOMCODE_API_KEY"),
        entry
            .and_then(|e| e.api_key.clone())
            .map(|k| crate::expand_env(&k)),
    ])
    .unwrap_or_default();

    let mut cfg = CodingAgentConfig::new(api_key, base_url, &model, &dir);
    cfg.preferred_language = selected.language;
    cfg.context_window = entry.and_then(|e| e.context_window).unwrap_or(128_000);
    cfg.stream_timeout = std::time::Duration::from_secs(args.stream_timeout);
    // Telemetry: the full host-loop instrumentation (turn-level TelemetryHook + tool
    // middleware + the review-slot MeteredProvider) lights up purely from setting this —
    // a disabled sink makes every emit a no-op. Flushed to disk on exit (below).
    let telemetry = crate::tel::build_sink(args.config.as_deref(), args.no_telemetry);
    crate::tel::maybe_show_notice(telemetry.is_enabled());
    cfg.telemetry = Some(telemetry.clone());

    // Session mode: --resume <id> / --continue (latest) / fresh.
    let session = if let Some(id) = &args.resume {
        SessionMode::Resume(id.clone())
    } else if args.continue_latest {
        let latest = SessionManager::for_project(&dir)
            .latest()
            .context("no session to continue in this project — start one without --continue")?;
        SessionMode::Resume(latest.id)
    } else {
        SessionMode::Fresh
    };

    // Mount configured `[[subagent.external]]` instances (Codex / Claude Code as
    // named subagent tools) so `atomcodex code` honors the same config the TUI
    // and `atomcode` CLI do. A missing config file means "no external agents";
    // a present-but-malformed one surfaces (same contract as provider loading).
    let loaded_config = match &args.config {
        Some(path) => atomcode_config::Config::load(path)
            .with_context(|| format!("failed to load config: {}", path.display()))?,
        None => match crate::default_config_path() {
            Some(path) if path.exists() => atomcode_config::Config::load(&path)
                .with_context(|| format!("failed to load config: {}", path.display()))?,
            _ => atomcode_config::Config::default(),
        },
    };
    let external_subagents =
        atomcode_coding::parts::resolve_external_subagents(&loaded_config.subagent, true);

    let opts = PrepareOptions {
        subagents: atomcode_coding::SubagentPolicy::Enabled,
        session,
        tools: true,
        skill_dirs: None,
        plugin_skill_dirs: Vec::new(),
        mcp: !args.no_mcp,
        extra_mcp_servers: Vec::new(),
        external_subagents,
        memory: !args.no_memory,
        web: !args.no_web,
        // The `code` agent can also review the current changes in-session (the dedicated
        // `atomcodex review` subcommand still exists for headless/CI one-shots).
        review: true,
        // atomcodex has no typed request/response protocol to carry a structured
        // prompt back to the user, so mounting `request_user_input` would advertise
        // a tool it could only ever answer with `Null`.
        request_user_input: false,
        // clix is intentionally core/bridge-free; external endpoints keep generic 429 handling.
        rate_limit_source: None,
    };

    eprintln!("preparing ({model}) …");
    let runtime = CodingRuntime::start(CodingRuntimeStart {
        agent: cfg,
        prepare: opts,
        provider_factory: Arc::new(DefaultCodingProviderFactory::new(concat!(
            "atomcode/",
            env!("CARGO_PKG_VERSION")
        ))),
        plugin_hooks: Arc::new(StaticPluginHookSource::default()),
        image_preprocessor: None,
    })
    .await
    .context("runtime start failed")?;
    let session_id = runtime.session.as_ref().map(|session| session.id.clone());
    let resumed = runtime
        .session
        .as_ref()
        .is_some_and(|session| session.resumed);
    let CodingRuntime {
        handle,
        mut events,
        task,
        ..
    } = runtime;

    if let Some(id) = &session_id {
        eprintln!(
            "session {} {}",
            id,
            if resumed { "(resumed)" } else { "(new)" }
        );
    }

    let mut input = BufReader::new(tokio::io::stdin()).lines();
    // ONE long-lived SIGINT listener for the whole process: a fresh
    // `tokio::signal::ctrl_c()` per select-iteration both drops signals landing in
    // the re-registration gap AND is simply absent outside drive_turn (tokio
    // permanently overrides the default disposition on first poll, so an unlistened
    // Ctrl-C would be a silent no-op).
    let mut sigint = spawn_sigint_listener();

    // One-shot: a single turn, then exit (still persisted). A failed turn must
    // exit NON-ZERO — `--yolo -p` is the CI mode, and CI needs the signal.
    if let Some(p) = args.prompt {
        handle
            .wait_mcp_ready(atomcode_capabilities::mcp::CONNECT_TIMEOUT)
            .await?;
        handle.submit(UserInput::from(p)).await?;
        let outcome = drive_turn(&handle, &mut events, &mut input, args.yolo, &mut sigint).await;
        finish(handle, task, session_id).await?;
        telemetry.shutdown(crate::tel::FLUSH_TIMEOUT).await;
        return match outcome {
            Some(TurnOutcome::Completed(StopReason::Stopped)) => Ok(()),
            Some(TurnOutcome::Completed(other)) => {
                bail!("turn did not complete normally: {other:?}")
            }
            Some(TurnOutcome::SnapshotUnavailable { reason, error }) => {
                bail!("turn snapshot unavailable after {reason:?}: {error}")
            }
            None => bail!("agent terminated unexpectedly"),
        };
    }

    eprintln!("interactive mode — /help for commands, /quit to exit");
    loop {
        eprint!("› ");
        let line = tokio::select! {
            // An stdin IO error must still take the graceful-exit path (Shutdown →
            // session_end), not `?` out past finish(). Rare (EOF is Ok(None)), and
            // the agent is idle here so nothing is mid-write — but exit cleanly.
            l = input.next_line() => match l {
                Ok(Some(l)) => l,
                Ok(None) => break,
                Err(e) => {
                    eprintln!("[stdin error: {e} — exiting]");
                    break;
                }
            },
            _ = sigint.recv() => {
                eprintln!("\n(exiting — session saved; /quit next time, or just Ctrl-C again)");
                break;
            }
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = line.strip_prefix('/') {
            match handle_slash(cmd, &dir, &handle) {
                SlashOutcome::Handled => continue,
                SlashOutcome::Quit => break,
            }
        }
        if handle.submit(UserInput::from(line)).await.is_err() {
            eprintln!("[agent terminated — exiting]");
            break;
        }
        if drive_turn(&handle, &mut events, &mut input, args.yolo, &mut sigint)
            .await
            .is_none()
        {
            eprintln!("[agent terminated — exiting]");
            break;
        }
    }
    let r = finish(handle, task, session_id).await;
    telemetry.shutdown(crate::tel::FLUSH_TIMEOUT).await;
    r
}

/// One process-wide SIGINT listener feeding a channel — every prompt/select in the
/// driver listens on the SAME receiver, so no signal lands in a re-registration gap
/// and no await-point is deaf to Ctrl-C.
fn spawn_sigint_listener() -> tokio::sync::mpsc::UnboundedReceiver<()> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            if tx.send(()).is_err() {
                return;
            }
        }
    });
    rx
}

async fn finish(
    handle: CodingRuntimeHandle,
    task: tokio::task::JoinHandle<atomcode_coding::RuntimeExit>,
    session_id: Option<String>,
) -> Result<()> {
    let _ = handle.shutdown().await;
    let _ = task.await;
    if let Some(id) = session_id {
        eprintln!("session saved — resume with: atomcodex code --resume {id}");
    }
    Ok(())
}

/// Drive ONE turn: render events until `TurnComplete`; answer approval requests
/// (interactive, or auto-allow under --yolo); Ctrl-C (via the shared SIGINT channel)
/// cancels the turn — the kernel also unparks a pending approval, fail-closed.
/// Returns the runtime-owned completion variant, or `None` when the agent task died.
#[derive(Debug, PartialEq, Eq)]
enum TurnOutcome {
    Completed(StopReason),
    SnapshotUnavailable { reason: StopReason, error: String },
}

fn turn_outcome(completion: TurnCompletion) -> TurnOutcome {
    match completion {
        TurnCompletion::Completed { reason, .. } => TurnOutcome::Completed(reason),
        TurnCompletion::SnapshotUnavailable { reason, error, .. } => {
            TurnOutcome::SnapshotUnavailable {
                reason,
                error: error.message,
            }
        }
    }
}

async fn drive_turn(
    handle: &CodingRuntimeHandle,
    events: &mut CodingRuntimeEvents,
    input: &mut Lines<BufReader<Stdin>>,
    yolo: bool,
    sigint: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
) -> Option<TurnOutcome> {
    use std::io::Write;
    let mut streamed = false;
    // Once a cancel is in flight, any approval Request still buffered in the event
    // channel is ALREADY flushed-to-deny inside the kernel — auto-answer Null
    // instead of parking the user on a prompt for a decision that no longer exists.
    let mut cancelled = false;
    loop {
        let ev = tokio::select! {
            ev = events.recv() => match ev { Some(ev) => ev.event, None => return None },
            _ = sigint.recv() => {
                eprintln!("\n[cancelling …]");
                cancelled = true;
                let _ = handle.cancel().await;
                continue;
            }
        };
        match ev {
            CodingRuntimeEvent::Agent(AgentEvent::TextDelta(t)) => {
                streamed = true;
                print!("{t}");
                let _ = std::io::stdout().flush();
            }
            CodingRuntimeEvent::Agent(AgentEvent::ToolStarted { call }) => {
                eprintln!(
                    "  → {} {}",
                    call.name,
                    crate::tool_hint(&call.name, &call.arguments)
                );
            }
            CodingRuntimeEvent::Agent(AgentEvent::ToolResult { result }) => {
                let mark = if result.is_error { "✗" } else { "✓" };
                eprintln!("    {mark} ({} chars)", result.content.chars().count());
            }
            CodingRuntimeEvent::Request(request) if request.kind == APPROVAL_KIND && !cancelled => {
                match approval_decision(&request.payload, input, yolo, sigint).await {
                    ApprovalAnswer::Respond(value) => {
                        let _ = handle.respond(request.id, value).await;
                    }
                    ApprovalAnswer::Cancelled => {
                        // Ctrl-C at the approval prompt: cancel the TURN — the kernel
                        // flushes this very round-trip to Null (deny) and unparks.
                        eprintln!("\n[cancelling …]");
                        cancelled = true;
                        let _ = handle.cancel().await;
                    }
                }
            }
            CodingRuntimeEvent::Request(request) => {
                // Unknown kind, or a stale approval after cancel: fail closed
                // (Null → deny semantics; a flushed id no-ops harmlessly).
                let _ = handle.respond(request.id, serde_json::Value::Null).await;
            }
            CodingRuntimeEvent::Agent(AgentEvent::Error { message, .. }) => {
                eprintln!("\n[error] {message}")
            }
            CodingRuntimeEvent::Agent(AgentEvent::Warning(w)) => eprintln!("[warn] {w}"),
            CodingRuntimeEvent::Agent(AgentEvent::ProviderRetry {
                attempt,
                max_attempts,
                backoff_secs,
                reason,
            }) => eprintln!(
                "[retry] API error {reason}; retrying in {backoff_secs}s ({attempt}/{max_attempts})"
            ),
            CodingRuntimeEvent::Agent(AgentEvent::StreamRecovery {
                attempt,
                max_attempts,
                recovered,
            }) => {
                if recovered {
                    eprintln!("[ok] recovered from the interrupted stream");
                } else {
                    eprintln!(
                        "[retry] safely continuing from saved progress ({attempt}/{max_attempts})"
                    );
                }
            }
            CodingRuntimeEvent::CompactionStarted { trigger } => {
                // The kernel fires this ONLY when a real drain/summary (a multi-second
                // LLM call) will run — manual `/compact`, overflow tier 2, OR an auto
                // compaction that escalated past the high-water mark. Show a progress
                // line for ALL of them so a headless run isn't silently blocked.
                let _ = &trigger;
                eprintln!("[compacting …]");
            }
            CodingRuntimeEvent::CompactionFinished {
                completion: atomcode_coding::runtime::CompactionCompletion::Completed(outcome),
            } => {
                eprintln!(
                    "[compacted{}]",
                    if outcome.committed {
                        ""
                    } else {
                        " — refused (no gain)"
                    }
                );
            }
            CodingRuntimeEvent::CompactionFinished {
                completion: atomcode_coding::runtime::CompactionCompletion::Failed { error, .. },
            } => {
                eprintln!("[compact failed] {error}");
            }
            CodingRuntimeEvent::TurnFinished(completion) => {
                if streamed {
                    println!();
                }
                let outcome = turn_outcome(completion);
                match &outcome {
                    TurnOutcome::Completed(StopReason::Stopped) => {}
                    TurnOutcome::Completed(other) => eprintln!("[turn ended: {other:?}]"),
                    TurnOutcome::SnapshotUnavailable { reason, error } => {
                        eprintln!("[turn snapshot unavailable after {reason:?}: {error}]")
                    }
                }
                return Some(outcome);
            }
            _ => {}
        }
    }
}

enum ApprovalAnswer {
    Respond(serde_json::Value),
    /// Ctrl-C at the prompt — the caller cancels the turn instead of responding.
    Cancelled,
}

/// Render the approval prompt and read the decision. The payload is the TYPED
/// `ApprovalRequest`; the answer is a serialized `ApprovalResponse`.
///
/// SAFETY OF THE READ (the must-fix from review): the prompt shares stdin with the
/// REPL, so a pasted block / typed-ahead text could be sitting in the buffer and
/// would otherwise be consumed AS the answer — a stray buffered "y" would silently
/// ALLOW a tool call the user never saw. Two layers of defense:
/// 1. DRAIN every already-buffered line before showing the prompt (discarded with
///    a notice — they were never going to be sent anyway);
/// 2. the escalating answer is a DELIBERATE WORD: `always` (not a single letter a
///    stray buffered character could spell). `y`/`yes` allow once; everything else
///    denies (fail-closed).
async fn approval_decision(
    payload: &serde_json::Value,
    input: &mut Lines<BufReader<Stdin>>,
    yolo: bool,
    sigint: &mut tokio::sync::mpsc::UnboundedReceiver<()>,
) -> ApprovalAnswer {
    let req: ApprovalRequest = match serde_json::from_value(payload.clone()) {
        Ok(r) => r,
        Err(_) => return ApprovalAnswer::Respond(serde_json::Value::Null), // malformed → fail closed
    };
    if yolo {
        // The audit line is the ONLY record of what was auto-allowed — UNABRIDGED.
        eprintln!("  [yolo] auto-allow {} {}", req.tool, req.args);
        return ApprovalAnswer::Respond(
            serde_json::to_value(ApprovalResponse::allow()).unwrap_or(serde_json::Value::Null),
        );
    }
    // Layer 1: discard buffered typed-ahead lines so they can't answer the prompt.
    let mut discarded = 0usize;
    while let Ok(Ok(Some(_))) =
        tokio::time::timeout(std::time::Duration::ZERO, input.next_line()).await
    {
        discarded += 1;
    }
    if discarded > 0 {
        eprintln!("  (discarded {discarded} typed-ahead line(s) — an approval prompt needs a fresh answer)");
    }
    eprintln!(
        "  approval needed: {} {}",
        req.tool,
        crate::truncate(&req.args, 200)
    );
    eprint!("  allow? [y = once / always = remember / N = deny] ");
    let answer = tokio::select! {
        l = input.next_line() => l.ok().flatten().unwrap_or_default(),
        _ = sigint.recv() => return ApprovalAnswer::Cancelled,
    };
    ApprovalAnswer::Respond(
        serde_json::to_value(map_decision(&answer)).unwrap_or(serde_json::Value::Null),
    )
}

/// Map a prompt answer to the typed response. Escalation (`allow_always`) requires
/// the deliberate word `always`; `y`/`yes` allow once; anything else denies.
fn map_decision(answer: &str) -> ApprovalResponse {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => ApprovalResponse::allow(),
        "always" => ApprovalResponse::allow_always(),
        _ => ApprovalResponse::deny(),
    }
}

enum SlashOutcome {
    Handled,
    Quit,
}

/// Handle a `/command`. Memory commands write through `MemoryStore` (the engine
/// injects/reconciles at the NEXT session start — production semantics); `/compact`
/// goes to the agent (queued to the turn boundary if one is running).
fn handle_slash(cmd: &str, dir: &std::path::Path, handle: &CodingRuntimeHandle) -> SlashOutcome {
    let (name, rest) = match cmd.split_once(char::is_whitespace) {
        Some((n, r)) => (n, r.trim()),
        None => (cmd, ""),
    };
    match name {
        "quit" | "exit" | "q" => SlashOutcome::Quit,
        "help" | "?" => {
            eprintln!(
                "  /remember [-g] <fact>   append to project (or -g: global) memory.md\n\
                 \x20 /forget [-g] <keyword>  remove matching entries\n\
                 \x20 /memory                 show the merged memory the model gets\n\
                 \x20 /compact [focus]        compact the conversation (turn boundary)\n\
                 \x20 /sessions               list this project's sessions\n\
                 \x20 /quit                   exit (session persists)"
            );
            SlashOutcome::Handled
        }
        "remember" => {
            let (global, text) = split_global_flag(rest);
            if text.is_empty() {
                eprintln!("  usage: /remember [-g] <fact>");
                return SlashOutcome::Handled;
            }
            let store = if global {
                MemoryStore::global()
            } else {
                MemoryStore::project(dir)
            };
            match store.append(text) {
                Ok(()) => eprintln!(
                    "  remembered ({}) — injected from the next session start",
                    if global { "global" } else { "project" }
                ),
                Err(e) => eprintln!("  failed to write memory: {e}"),
            }
            SlashOutcome::Handled
        }
        "forget" => {
            let (global, kw) = split_global_flag(rest);
            if kw.is_empty() {
                eprintln!("  usage: /forget [-g] <keyword>");
                return SlashOutcome::Handled;
            }
            let store = if global {
                MemoryStore::global()
            } else {
                MemoryStore::project(dir)
            };
            match store.remove_matching(kw) {
                Ok(removed) if removed.is_empty() => eprintln!("  nothing matched '{kw}'"),
                Ok(removed) => {
                    for r in &removed {
                        eprintln!("  forgot: {r}");
                    }
                }
                Err(e) => eprintln!("  failed to update memory: {e}"),
            }
            SlashOutcome::Handled
        }
        "memory" => {
            let merged = MemoryStore::merged_for_prompt(
                &MemoryStore::global(),
                &MemoryStore::project(dir),
                &dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "project".into()),
            );
            if merged.is_empty() {
                eprintln!("  (memory is empty — /remember <fact>)");
            } else {
                eprintln!("{merged}");
            }
            SlashOutcome::Handled
        }
        "compact" => {
            let focus = (!rest.is_empty()).then(|| rest.to_string());
            let _ = handle.compact(focus);
            eprintln!("  compaction requested");
            SlashOutcome::Handled
        }
        "sessions" => {
            let mgr = SessionManager::for_project(dir);
            for m in mgr.list() {
                eprintln!(
                    "  {}  {:<28} turns {:<4} {}",
                    m.id,
                    m.name,
                    m.turn_count,
                    fmt_ts(m.updated_at)
                );
            }
            SlashOutcome::Handled
        }
        _ => {
            // Warn-and-SKIP: a typo'd /quti must not silently burn a model turn
            // (and, under --yolo, execute whatever the model makes of it).
            eprintln!("  unknown command /{name} — /help (a leading '/' is command space)");
            SlashOutcome::Handled
        }
    }
}

/// Split a leading `-g`/`--global` flag off a slash-command argument.
fn split_global_flag(rest: &str) -> (bool, &str) {
    for flag in ["-g ", "--global "] {
        if let Some(r) = rest.strip_prefix(flag) {
            return (true, r.trim());
        }
    }
    if rest == "-g" || rest == "--global" {
        return (true, "");
    }
    (false, rest)
}

/// epoch ms → `YYYY-MM-DD HH:MM UTC` (dependency-free; UTC — labeled as such, while
/// the engine's StatusReminderHook shows the MODEL local date; negative ms clamp to epoch).
fn fmt_ts(ms: i64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let Some(t) = UNIX_EPOCH.checked_add(Duration::from_millis(ms.max(0) as u64)) else {
        return ms.to_string();
    };
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs();
            // days since epoch → Y-M-D (civil from days, Howard Hinnant's algorithm).
            let days = (secs / 86_400) as i64;
            let (y, m, dd) = civil_from_days(days);
            let rem = secs % 86_400;
            format!(
                "{y:04}-{m:02}-{dd:02} {:02}:{:02} UTC",
                rem / 3600,
                (rem % 3600) / 60
            )
        }
        Err(_) => ms.to_string(),
    }
}

/// Days-since-epoch → (year, month, day), Gregorian. Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_global_flag_variants() {
        assert_eq!(split_global_flag("-g prefer tabs"), (true, "prefer tabs"));
        assert_eq!(split_global_flag("--global x"), (true, "x"));
        assert_eq!(split_global_flag("plain fact"), (false, "plain fact"));
        assert_eq!(split_global_flag("-g"), (true, ""));
        assert_eq!(
            split_global_flag("-grand fact"),
            (false, "-grand fact"),
            "no false prefix match"
        );
    }

    #[test]
    fn map_decision_requires_deliberate_tokens() {
        assert_eq!(map_decision("y"), ApprovalResponse::allow());
        assert_eq!(map_decision(" Yes "), ApprovalResponse::allow());
        assert_eq!(map_decision("always"), ApprovalResponse::allow_always());
        assert_eq!(map_decision("ALWAYS"), ApprovalResponse::allow_always());
        // A stray single letter can no longer escalate.
        assert_eq!(map_decision("a"), ApprovalResponse::deny());
        assert_eq!(map_decision(""), ApprovalResponse::deny());
        assert_eq!(map_decision("n"), ApprovalResponse::deny());
        assert_eq!(map_decision("sure, do it"), ApprovalResponse::deny());
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert!(fmt_ts(0).starts_with("1970-01-01 00:00"));
    }

    #[test]
    fn snapshot_unavailable_is_not_a_successful_stopped_turn() {
        let completion = TurnCompletion::SnapshotUnavailable {
            turn_id: 1,
            reason: StopReason::Stopped,
            error: atomcode_coding::RuntimeSnapshotError {
                message: "snapshot failed".into(),
            },
            stats: Default::default(),
        };

        assert!(matches!(
            turn_outcome(completion),
            TurnOutcome::SnapshotUnavailable {
                reason: StopReason::Stopped,
                ..
            }
        ));
    }
}
