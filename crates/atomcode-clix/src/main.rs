//! `atomcodex` — a standalone, single-capability CLI: code review. It drives the
//! `atomcode-review` agent (kernel + capabilities, no atomcode-core/atomcode-cli coupling)
//! over a `git diff`, then prints the structured findings the agent reported.
//!
//! Usage:
//!   atomcodex review [--base <ref>] [--staged] [--repo <dir>] [--model <m>] [--json]
//!
//! Provider creds resolve in precedence order: CLI flags > env (ATOMCODE_API_KEY /
//! ATOMCODE_BASE_URL / ATOMCODE_MODEL) > `~/.atomcode/config.toml`. From the config file
//! it reads the `[providers.<name>]` table named by `default_provider` (or `--provider`);
//! an `api_key` of the form `$VAR` is expanded from the environment. `api_key` is optional
//! (some gateways need none).

mod code;

use anyhow::{bail, Context, Result};
use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_review::{build_review_agent, Finding, ReviewAgentConfig};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Parser)]
#[command(name = "atomcodex", about = "AtomCode standalone CLI (new stack)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive coding agent (full assembly: tools+codeintel+web+skills+mcp+session+memory).
    Code(code::CodeArgs),
    /// List this project's resumable sessions.
    Sessions(code::SessionsArgs),
    /// Review the local git diff and report structured findings.
    Review(ReviewArgs),
}

#[derive(Parser)]
struct ReviewArgs {
    /// Base git ref to diff against (reviews `<base>...HEAD`). Omit to review uncommitted
    /// changes (`git diff HEAD`).
    #[arg(long, conflicts_with_all = ["pr", "diff_file"])]
    base: Option<String>,
    /// Review only staged changes (`git diff --staged`).
    #[arg(long, conflicts_with_all = ["pr", "diff_file"])]
    staged: bool,
    /// Review a GitHub pull request by number (`gh pr diff <N>`; needs the `gh` CLI).
    #[arg(long, conflicts_with = "diff_file")]
    pr: Option<u64>,
    /// Review a diff from a file, or `-` for stdin (works with any forge: GitLab/gitcode
    /// MRs, CI artifacts, etc. — e.g. `glab mr diff 5 | atomcodex review --diff-file -`).
    #[arg(long)]
    diff_file: Option<String>,
    /// Repository root (default: current directory).
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Model id (overrides $ATOMCODE_MODEL).
    #[arg(long)]
    model: Option<String>,
    /// Provider API key (overrides $ATOMCODE_API_KEY).
    #[arg(long)]
    api_key: Option<String>,
    /// Provider base URL (overrides $ATOMCODE_BASE_URL).
    #[arg(long)]
    base_url: Option<String>,
    /// Named `[providers.<name>]` entry to use from the config file (overrides the
    /// config's `default_provider`).
    #[arg(long)]
    provider: Option<String>,
    /// Config file path (default: ~/.atomcode/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// FULLY override the reviewer system prompt with this text (replaces the built-in
    /// persona entirely — you must then tell the model about its tools + report_finding).
    #[arg(long)]
    system_prompt: Option<String>,
    /// Like --system-prompt, but read the full prompt from a file (`-` for stdin).
    #[arg(long, conflicts_with = "system_prompt")]
    system_prompt_file: Option<String>,
    /// APPEND an extra section after the system prompt (built-in persona or the
    /// --system-prompt override). The normal customization channel: domain rules,
    /// ignore lists, repo style guides, PR metadata — keeps the built-in reviewer
    /// instructions intact.
    #[arg(long)]
    append_system_prompt: Option<String>,
    /// Like --append-system-prompt, but read the section from a file (`-` for stdin).
    #[arg(long, conflicts_with = "append_system_prompt")]
    append_system_prompt_file: Option<String>,
    /// Run a CUSTOM task instead of diff review (for chat / explain / summary). Replaces the
    /// built-in "review this diff" task with this text and SKIPS diff computation — the caller
    /// puts everything the model needs (question, target code, any diff context) into the text.
    /// Pair with --system-prompt to set the persona and --json to read the answer from `text`.
    #[arg(long, conflicts_with_all = ["base", "staged", "pr", "diff_file"])]
    task: Option<String>,
    /// Like --task, but read the task from a file (`-` for stdin).
    #[arg(long, conflicts_with_all = ["task", "base", "staged", "pr", "diff_file"])]
    task_file: Option<String>,
    /// Max seconds to wait for each stream event before failing the run (liveness guard
    /// against a stalled provider). Raise it for slow providers / very large contexts.
    #[arg(long, default_value_t = 180)]
    stream_timeout: u64,
    /// Emit findings as JSON instead of a human-readable report.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Code(args) => code::code(args).await,
        Cmd::Sessions(args) => code::sessions(args),
        Cmd::Review(args) => review(args).await,
    }
}

async fn review(args: ReviewArgs) -> Result<()> {
    // `-` means stdin; only ONE of diff/task/persona/append may read it.
    let stdin_users = [
        ("--diff-file", args.diff_file.as_deref()),
        ("--task-file", args.task_file.as_deref()),
        ("--system-prompt-file", args.system_prompt_file.as_deref()),
        ("--append-system-prompt-file", args.append_system_prompt_file.as_deref()),
    ];
    let on_stdin: Vec<&str> =
        stdin_users.iter().filter(|(_, v)| *v == Some("-")).map(|(n, _)| *n).collect();
    if on_stdin.len() > 1 {
        bail!("{} all read stdin; give all but one of them a file path", on_stdin.join(" and "));
    }
    let repo = args.repo.canonicalize().with_context(|| format!("repo not found: {}", args.repo.display()))?;

    // Two modes: a CUSTOM task (chat/explain/summary — no diff) or the built-in diff review.
    let custom_task = resolve_task(args.task.clone(), args.task_file.clone())?;
    let (task, trace_label) = match custom_task {
        Some(t) => {
            let label = format!("custom task ({} chars)", t.len());
            (t, label)
        }
        None => {
            let diff = obtain_diff(&repo, &args)?;
            if diff.trim().is_empty() {
                println!("No changes to review.");
                return Ok(());
            }
            let label = format!("{} changed line(s)", diff.lines().count());
            let t = format!(
                "Review the following diff. Investigate the surrounding code with your read-only \
                 tools, then report each issue via `report_finding`. Report only real issues, each \
                 anchored to a concrete file and line.\n\n```diff\n{diff}\n```"
            );
            (t, label)
        }
    };

    // Provider creds: flag > env (ATOMCODE_*) > config.toml provider entry.
    let entry = load_provider_entry(args.config.as_deref(), args.provider.as_deref())?;
    let entry = entry.as_ref();
    // Config values may be `$VAR` / `${VAR}` env refs — expand them all (not just api_key).
    let base_url = first_nonempty([
        args.base_url,
        env("ATOMCODE_BASE_URL"),
        entry.and_then(|e| e.base_url.clone()).map(|v| expand_env(&v)),
    ])
    .context("missing base URL: pass --base-url, set $ATOMCODE_BASE_URL, or add base_url to the config provider")?;
    let model = first_nonempty([
        args.model,
        env("ATOMCODE_MODEL"),
        entry.and_then(|e| e.model.clone()).map(|v| expand_env(&v)),
    ])
    .context("missing model: pass --model, set $ATOMCODE_MODEL, or add model to the config provider")?;
    // The AtomGit/gitcode gateways require AtomCode's proprietary request signing (a
    // closed-source overlay in the official binary). atomcodex uses the neutral provider
    // and cannot sign — fail fast with an actionable message instead of a confusing 401.
    if is_signing_gateway(&base_url) {
        bail!(
            "provider base_url '{base_url}' is an AtomGit/gitcode signing-enforced gateway, \
             which atomcodex cannot authenticate against (it needs AtomCode's proprietary \
             request signing). Use a standard provider with an explicit api_key — e.g. \
             `--provider openrouter`, or set ATOMCODE_API_KEY/ATOMCODE_BASE_URL/ATOMCODE_MODEL \
             to a plain OpenAI-compatible endpoint."
        );
    }
    // api_key is OPTIONAL — some gateways need none. Config values may be `$ENV` refs.
    let api_key = first_nonempty([
        args.api_key,
        env("ATOMCODE_API_KEY"),
        entry.and_then(|e| e.api_key.clone()).map(|k| expand_env(&k)),
    ])
    .unwrap_or_default();
    let context_window = entry.and_then(|e| e.context_window).unwrap_or(128_000);

    let mut cfg = ReviewAgentConfig::new(api_key, base_url, model, &repo);
    cfg.context_window = context_window;
    cfg.stream_timeout = std::time::Duration::from_secs(args.stream_timeout);
    // Full system-prompt override (flag text > file/stdin). None ⇒ built-in reviewer persona.
    cfg.persona = resolve_system_prompt(args.system_prompt.clone(), args.system_prompt_file.clone())?;
    // Appended section (domain rules / ignore lists / PR metadata) — composes on top.
    cfg.persona_append =
        resolve_system_prompt(args.append_system_prompt.clone(), args.append_system_prompt_file.clone())?;
    let model_label = cfg.model.clone();
    let (agent, report) = build_review_agent(cfg).map_err(|e| anyhow::anyhow!(e))?;

    // Live trace on stderr (stdout stays clean for findings / --json). The run is one LLM
    // turn loop — without this the terminal looks frozen while the model thinks + calls tools.
    eprintln!("Running {trace_label} with {model_label} …");
    let run = run_review_streaming(agent, task).await;

    // Trace summary: tool-usage profile + token spend — exactly what you need to optimize.
    if run.tool_calls > 0 {
        let profile: Vec<String> = run.tool_counts.iter().map(|(n, c)| format!("{n}×{c}")).collect();
        eprintln!("— trace — {} tool call(s): {}", run.tool_calls, profile.join(", "));
    }
    if let Some(u) = run.usage {
        eprintln!("— tokens — prompt {} / completion {} / cached {}", u.prompt, u.completion, u.cached);
    }

    let mut findings = report.findings();
    sort_findings(&mut findings);

    if args.json {
        println!("{}", render_json(&findings, &run.text, run.usage)?);
    } else if !findings.is_empty() {
        print!("{}", render_findings(&findings));
    } else if run.error.is_some() {
        // Don't claim "clean" — the run didn't finish, so we can't conclude there are no issues.
        println!("Review did not complete — no findings were collected.");
    } else {
        println!("No findings — the diff looks clean.");
    }
    if !args.json && !run.text.trim().is_empty() {
        println!("\n— reviewer summary —\n{}", run.text.trim());
    }

    // Exit policy: a clean run exits 0. On error, exit non-zero ONLY when nothing was
    // delivered — a stall AFTER findings were collected still produced the review, so warn
    // but succeed; a failure with no findings (auth/connect/immediate stall) is a real
    // failure CI must detect.
    if let Some(err) = run.error {
        if findings.is_empty() {
            bail!("review run failed before producing findings: {err}");
        }
        eprintln!("warning: review ended early ({err}); {} finding(s) collected before it stopped", findings.len());
    }
    Ok(())
}

/// Result of driving one review turn loop while live-tracing tool activity to stderr.
#[derive(Default)]
struct ReviewRun {
    /// Accumulated assistant prose (the final summary).
    text: String,
    /// Last error surfaced, if any.
    error: Option<String>,
    /// Final token usage.
    usage: Option<atomcode_kernel::stream::TokenUsage>,
    /// Per-tool call counts (the usage profile).
    tool_counts: std::collections::BTreeMap<String, usize>,
    /// Total tool calls.
    tool_calls: usize,
}

/// Spawn the review agent, kick off the turn, and stream a live execution trace to stderr:
/// each tool call (name + key args) and its result (ok/err + size), plus a final
/// tool-usage + token profile. Returns the accumulated summary text + stats.
async fn run_review_streaming(agent: Agent, task: String) -> ReviewRun {
    let mut handle = agent.spawn();
    let _ = handle.commands.send(AgentCommand::SendMessage { text: task, images: vec![] });

    let mut run = ReviewRun::default();
    let mut call_names: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::ToolStarted { call } => {
                run.tool_calls += 1;
                *run.tool_counts.entry(call.name.clone()).or_default() += 1;
                call_names.insert(call.id.clone(), call.name.clone());
                eprintln!("  → {} {}", call.name, tool_hint(&call.name, &call.arguments));
            }
            AgentEvent::ToolResult { result } => {
                let name = call_names.get(&result.call_id).map(String::as_str).unwrap_or("tool");
                let mark = if result.is_error { "✗" } else { "✓" };
                eprintln!("    {mark} {name} ({} chars)", result.content.chars().count());
            }
            AgentEvent::TextDelta(t) => run.text.push_str(&t),
            // Each turn emits ONE per-turn usage figure; SUM across turns for the run total.
            // (Last-wins kept only the final turn and silently under-reported the whole
            // agentic run — e.g. a 40-turn review looked like one 50k-prompt call.)
            AgentEvent::Usage(meta) => {
                let u = run.usage.get_or_insert(Default::default());
                u.prompt += meta.tokens.prompt;
                u.completion += meta.tokens.completion;
                u.cached += meta.tokens.cached;
            }
            AgentEvent::Error { message, .. } => {
                eprintln!("    [error] {message}");
                run.error = Some(message);
            }
            AgentEvent::Warning(w) => eprintln!("    [warn] {w}"),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    let _ = handle.commands.send(AgentCommand::Shutdown);
    let _ = handle.task.await;
    run
}

/// A short, human one-liner describing a tool call's salient argument, for the live trace.
pub(crate) fn tool_hint(name: &str, args_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    // report_finding is the deliverable — show priority + title.
    if name == "report_finding" {
        let pri = get("priority").unwrap_or_default();
        let title = get("title").unwrap_or_default();
        return format!("[{pri}] {}", truncate(&title, 80));
    }
    // Otherwise show the first salient field present.
    for k in ["file_path", "path", "pattern", "query", "name", "symbol"] {
        if let Some(val) = get(k) {
            return truncate(&val, 80);
        }
    }
    String::new()
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// First non-empty value in precedence order.
pub(crate) fn first_nonempty(vals: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    vals.into_iter().flatten().find(|s| !s.trim().is_empty())
}

/// Read an env var, returning `None` when unset or empty.
pub(crate) fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

/// True if `base_url`'s host is an AtomGit/gitcode signing-enforced LLM gateway — those
/// require AtomCode's proprietary request signing, which this neutral CLI cannot produce.
pub(crate) fn is_signing_gateway(base_url: &str) -> bool {
    const HOSTS: &[&str] =
        &["llm-api.atomgit.com", "api-ai.gitcode.com", "pre-llm-api-cce.atomgit.com"];
    // Match on host, not a bare substring, so a lookalike path can't trip it.
    let after_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    let host = after_scheme.split(['/', ':']).next().unwrap_or("");
    HOSTS.contains(&host)
}

/// Expand a WHOLE-VALUE env reference, consistent with the rest of the ecosystem:
/// `$VAR`, `${VAR}`, or `${VAR:-default}`. Any other value passes through unchanged
/// (no inline/partial substitution).
pub(crate) fn expand_env(value: &str) -> String {
    if value.starts_with("${") {
        // `${VAR}` or `${VAR:-default}` — only when cleanly closed; else pass through.
        if let Some(inner) = value.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
            return match inner.split_once(":-") {
                Some((var, default)) => std::env::var(var).unwrap_or_else(|_| default.to_string()),
                None => std::env::var(inner).unwrap_or_default(),
            };
        }
        return value.to_string();
    }
    match value.strip_prefix('$') {
        Some(var) => std::env::var(var).unwrap_or_default(),
        None => value.to_string(),
    }
}

/// A `[providers.<name>]` entry in `~/.atomcode/config.toml`. All fields optional so a
/// partial/foreign config still parses (extra keys like `type` are ignored).
#[derive(Deserialize, Clone, Default)]
pub(crate) struct ProviderEntry {
    #[serde(default)]
    pub(crate) api_key: Option<String>,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) base_url: Option<String>,
    #[serde(default)]
    pub(crate) context_window: Option<u32>,
}

#[derive(Deserialize, Default)]
struct FileConfig {
    #[serde(default)]
    default_provider: Option<String>,
    #[serde(default)]
    providers: HashMap<String, ProviderEntry>,
}

/// Parse a config.toml string into the subset we need (ignoring unrelated keys).
fn parse_file_config(toml_str: &str) -> Result<FileConfig> {
    toml::from_str(toml_str).context("failed to parse config.toml")
}

/// Pick the provider entry: `override_name` ⊳ the config's `default_provider`.
fn pick_provider(fc: &FileConfig, override_name: Option<&str>) -> Option<ProviderEntry> {
    let name = override_name.or(fc.default_provider.as_deref())?;
    fc.providers.get(name).cloned()
}

/// `~/.atomcode/config.toml` (honors $ATOMCODE_HOME, else $HOME / %USERPROFILE%).
fn default_config_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("ATOMCODE_HOME") {
        return Some(PathBuf::from(home).join("config.toml"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".atomcode").join("config.toml"))
}

/// Load the selected provider entry from the config file.
/// - default path absent → `Ok(None)` (flags/env can still supply everything);
/// - explicit `--config` path unreadable → `Err` (the user pointed at it);
/// - file present but MALFORMED → `Err` (don't silently fall through to a confusing
///   "missing base URL" later);
/// - file parses but has no matching provider → `Ok(None)`.
pub(crate) fn load_provider_entry(
    config_override: Option<&Path>,
    provider: Option<&str>,
) -> Result<Option<ProviderEntry>> {
    let (path, explicit) = match config_override {
        Some(p) => (p.to_path_buf(), true),
        None => match default_config_path() {
            Some(p) => (p, false),
            None => return Ok(None),
        },
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if explicit => return Err(anyhow::Error::new(e)).with_context(|| format!("cannot read config file: {}", path.display())),
        Err(_) => return Ok(None), // default path simply absent — fine
    };
    let fc = parse_file_config(&text).with_context(|| format!("malformed config file: {}", path.display()))?;
    Ok(pick_provider(&fc, provider))
}

/// Resolve the diff to review from the chosen source. Precedence: `--diff-file` (any
/// forge / stdin) > `--pr` (GitHub via `gh`) > local git (`--staged` / `--base` / HEAD).
fn obtain_diff(repo: &Path, args: &ReviewArgs) -> Result<String> {
    if let Some(df) = &args.diff_file {
        return read_diff_file(df);
    }
    if let Some(pr) = args.pr {
        return gh_pr_diff(repo, pr);
    }
    git_diff(repo, args.base.as_deref(), args.staged)
}

/// Resolve the custom task: inline `--task` text wins; else read `--task-file` (`-` = stdin).
/// `None` ⇒ no custom task, fall back to the built-in diff-review task.
fn resolve_task(text: Option<String>, file: Option<String>) -> Result<Option<String>> {
    if let Some(t) = text.filter(|s| !s.trim().is_empty()) {
        return Ok(Some(t));
    }
    if let Some(f) = file {
        let content = if f == "-" {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).context("failed to read task from stdin")?;
            buf
        } else {
            std::fs::read_to_string(&f).with_context(|| format!("failed to read task file: {f}"))?
        };
        if content.trim().is_empty() {
            bail!("task file is empty: {f}");
        }
        return Ok(Some(content));
    }
    Ok(None)
}

/// Resolve a FULL system-prompt override: inline `--system-prompt` text wins; else read
/// `--system-prompt-file` (path, or `-` for stdin); else `None` (use the built-in persona).
fn resolve_system_prompt(text: Option<String>, file: Option<String>) -> Result<Option<String>> {
    if let Some(t) = text.filter(|s| !s.trim().is_empty()) {
        return Ok(Some(t));
    }
    if let Some(f) = file {
        let content = if f == "-" {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf).context("failed to read system prompt from stdin")?;
            buf
        } else {
            std::fs::read_to_string(&f).with_context(|| format!("failed to read system prompt file: {f}"))?
        };
        return Ok(Some(content));
    }
    Ok(None)
}

/// Read a diff from a file path, or from stdin when the path is `-`.
fn read_diff_file(path: &str) -> Result<String> {
    if path == "-" {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).context("failed to read diff from stdin")?;
        Ok(buf)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("failed to read diff file: {path}"))
    }
}

/// Fetch a GitHub PR's diff via the `gh` CLI (`gh pr diff <N>`), run in the repo dir so
/// `gh` infers the owner/repo from the remote.
fn gh_pr_diff(repo: &Path, pr: u64) -> Result<String> {
    // NB: `gh` has NO `-C`/`--cwd` flag (unlike `git`) — set the process cwd instead so
    // it infers owner/repo from that directory's remote.
    let out = Command::new("gh")
        .current_dir(repo)
        .args(["pr", "diff", &pr.to_string()])
        .output()
        .context("failed to run `gh` — install the GitHub CLI, or pipe the diff via `--diff-file -` (e.g. for gitcode/GitLab)")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("`gh pr diff {pr}` failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Compute the LOCAL diff to review. `--staged` → staged changes; else `<base>...HEAD`
/// when a base is given; else all uncommitted changes (`git diff HEAD`).
fn git_diff(repo: &Path, base: Option<&str>, staged: bool) -> Result<String> {
    let mut args: Vec<String> = vec!["diff".into()];
    if staged {
        args.push("--staged".into());
    } else if let Some(base) = base {
        args.push(format!("{base}...HEAD"));
    } else {
        args.push("HEAD".into());
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .output()
        .context("failed to run `git` — is it installed and on PATH?")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("git diff failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Sort findings most-actionable first: by priority (P0 < P1 < P2 < P3), then by
/// confidence descending.
fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        priority_ord(&a.priority)
            .cmp(&priority_ord(&b.priority))
            .then(b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal))
    });
}

fn priority_ord(p: &str) -> u8 {
    match p {
        "P0" => 0,
        "P1" => 1,
        "P2" => 2,
        "P3" => 3,
        _ => 4,
    }
}

/// Human-readable report: a count header, then one block per finding.
fn render_findings(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "No findings — the diff looks clean.\n".to_string();
    }
    let mut counts = [0usize; 4];
    for f in findings {
        let o = priority_ord(&f.priority);
        if (o as usize) < 4 {
            counts[o as usize] += 1;
        }
    }
    let mut out = format!(
        "{} finding(s): {} P0, {} P1, {} P2, {} P3\n\n",
        findings.len(),
        counts[0],
        counts[1],
        counts[2],
        counts[3]
    );
    for f in findings {
        let loc = if f.line_start == f.line_end {
            format!("{}:{}", f.file_path, f.line_start)
        } else {
            format!("{}:{}-{}", f.file_path, f.line_start, f.line_end)
        };
        out.push_str(&format!("[{} {:.2}] {}  {}\n", f.priority, f.confidence, loc, f.title));
        for line in f.body.lines() {
            out.push_str(&format!("    {line}\n"));
        }
        out.push('\n');
    }
    out
}

/// Structured `--json` payload: findings plus the agent's final prose and token usage,
/// so an embedder gets the whole review from stdout (stderr stays human-only trace).
#[derive(Serialize)]
struct ReviewJson<'a> {
    findings: &'a [Finding],
    text: &'a str,
    usage: Option<atomcode_kernel::stream::TokenUsage>,
}

fn render_json(
    findings: &[Finding],
    text: &str,
    usage: Option<atomcode_kernel::stream::TokenUsage>,
) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&ReviewJson { findings, text: text.trim(), usage })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(priority: &str, confidence: f32, title: &str) -> Finding {
        Finding {
            title: title.into(),
            body: "b".into(),
            priority: priority.into(),
            confidence,
            file_path: "src/a.rs".into(),
            line_start: 1,
            line_end: 1,
            suggestion: String::new(),
            suggested_code: String::new(),
        }
    }

    #[test]
    fn resolve_task_inline_text_wins() {
        let got = resolve_task(Some("answer this".into()), Some("ignored.txt".into())).unwrap();
        assert_eq!(got.as_deref(), Some("answer this"));
    }

    #[test]
    fn resolve_task_none_when_unset() {
        assert!(resolve_task(None, None).unwrap().is_none());
        // blank inline text is treated as unset (falls back to diff review)
        assert!(resolve_task(Some("   ".into()), None).unwrap().is_none());
    }

    #[test]
    fn resolve_task_reads_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("task.txt");
        std::fs::write(&p, "explain this line").unwrap();
        let got = resolve_task(None, Some(p.to_string_lossy().into_owned())).unwrap();
        assert_eq!(got.as_deref(), Some("explain this line"));
    }

    #[test]
    fn resolve_task_empty_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.txt");
        std::fs::write(&p, "   \n").unwrap();
        assert!(resolve_task(None, Some(p.to_string_lossy().into_owned())).is_err());
    }

    #[test]
    fn json_envelope_carries_findings_text_usage() {
        let fs = vec![finding("P0", 0.9, "x")];
        let usage = atomcode_kernel::stream::TokenUsage { prompt: 10, completion: 5, cached: 2 };
        let out = render_json(&fs, "  summary prose  ", Some(usage)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["findings"][0]["title"], "x");
        assert_eq!(v["text"], "summary prose", "text is trimmed");
        assert_eq!(v["usage"]["prompt"], 10);
        assert_eq!(v["usage"]["completion"], 5);
        assert_eq!(v["usage"]["cached"], 2);
    }

    #[test]
    fn json_envelope_usage_null_when_absent() {
        let out = render_json(&[], "", None).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(v["findings"].as_array().unwrap().is_empty());
        assert_eq!(v["text"], "");
        assert!(v["usage"].is_null());
    }

    #[test]
    fn sorts_by_priority_then_confidence() {
        let mut fs = vec![
            finding("P2", 0.5, "c"),
            finding("P0", 0.4, "a"),
            finding("P1", 0.6, "b1"),
            finding("P1", 0.9, "b2"),
        ];
        sort_findings(&mut fs);
        let titles: Vec<&str> = fs.iter().map(|f| f.title.as_str()).collect();
        // P0 first; within P1 the higher-confidence one (b2) precedes b1; P2 last.
        assert_eq!(titles, vec!["a", "b2", "b1", "c"]);
    }

    #[test]
    fn render_empty_is_clean() {
        assert!(render_findings(&[]).contains("looks clean"));
    }

    #[test]
    fn render_has_header_and_blocks() {
        let mut fs = vec![finding("P0", 0.95, "fix: x"), finding("P2", 0.5, "tidy: y")];
        sort_findings(&mut fs);
        let out = render_findings(&fs);
        assert!(out.contains("2 finding(s): 1 P0, 0 P1, 1 P2, 0 P3"), "{out}");
        assert!(out.contains("[P0 0.95] src/a.rs:1  fix: x"), "{out}");
        assert!(out.contains("    b\n"), "body indented: {out}");
    }

    #[test]
    fn priority_ord_orders_and_defaults_unknown_last() {
        assert!(priority_ord("P0") < priority_ord("P3"));
        assert_eq!(priority_ord("weird"), 4);
    }

    const SAMPLE: &str = r#"
default_provider = "atomgit"
default_workdir = "/tmp"
auto_update = true

[providers.atomgit]
type = "openai"
model = "deepseek-v4-flash"
base_url = "https://llm-api.atomgit.com/v1"
context_window = 1000000

[providers.openrouter]
type = "openai"
api_key = "$OPENROUTER_API_KEY"
model = "stepfun/step-3.7-flash"
base_url = "https://openrouter.ai/api/v1"
"#;

    #[test]
    fn parses_default_provider_and_entry() {
        let fc = parse_file_config(SAMPLE).unwrap();
        let e = pick_provider(&fc, None).expect("default provider resolves");
        assert_eq!(e.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(e.base_url.as_deref(), Some("https://llm-api.atomgit.com/v1"));
        assert_eq!(e.context_window, Some(1_000_000));
        assert_eq!(e.api_key, None, "atomgit entry has no api_key");
    }

    #[test]
    fn provider_override_selects_named_entry() {
        let fc = parse_file_config(SAMPLE).unwrap();
        let e = pick_provider(&fc, Some("openrouter")).expect("named provider resolves");
        assert_eq!(e.model.as_deref(), Some("stepfun/step-3.7-flash"));
        assert_eq!(e.api_key.as_deref(), Some("$OPENROUTER_API_KEY"));
        assert!(pick_provider(&fc, Some("nope")).is_none(), "unknown provider → None");
    }

    #[test]
    fn ignores_unrelated_keys_and_missing_default() {
        // No default_provider, extra top-level keys → still parses; default pick → None.
        let fc = parse_file_config("language = \"zh\"\n[providers.x]\nmodel=\"m\"\nbase_url=\"u\"\n").unwrap();
        assert!(pick_provider(&fc, None).is_none());
        assert!(pick_provider(&fc, Some("x")).is_some());
    }

    #[test]
    fn detects_signing_gateways_by_host() {
        assert!(is_signing_gateway("https://llm-api.atomgit.com/v1"));
        assert!(is_signing_gateway("https://api-ai.gitcode.com/v1/chat/completions"));
        assert!(is_signing_gateway("https://pre-llm-api-cce.atomgit.com/v1"));
        // plain providers are fine.
        assert!(!is_signing_gateway("https://openrouter.ai/api/v1"));
        assert!(!is_signing_gateway("https://api.deepseek.com/v1"));
        // a lookalike path must NOT trip the host check.
        assert!(!is_signing_gateway("https://evil.com/llm-api.atomgit.com/v1"));
    }

    #[test]
    fn load_provider_entry_surfaces_malformed_config() {
        let d = tempfile::tempdir().unwrap();
        // Malformed TOML at an explicit --config path → Err (not silently None).
        let bad = d.path().join("bad.toml");
        std::fs::write(&bad, "this is = = not valid toml [[[").unwrap();
        assert!(load_provider_entry(Some(&bad), None).is_err(), "malformed config must error");
        // Explicit but missing path → Err.
        let missing = d.path().join("nope.toml");
        assert!(load_provider_entry(Some(&missing), None).is_err(), "explicit missing config errors");
        // Valid config, unknown provider → Ok(None).
        let good = d.path().join("good.toml");
        std::fs::write(&good, SAMPLE).unwrap();
        assert!(load_provider_entry(Some(&good), Some("nope")).unwrap().is_none());
        assert!(load_provider_entry(Some(&good), None).unwrap().is_some(), "default_provider resolves");
    }

    #[test]
    fn expand_env_resolves_dollar_refs() {
        std::env::set_var("ATOMCODE_CLIX_TEST_KEY", "secret-123");
        // $VAR and ${VAR} both resolve.
        assert_eq!(expand_env("$ATOMCODE_CLIX_TEST_KEY"), "secret-123");
        assert_eq!(expand_env("${ATOMCODE_CLIX_TEST_KEY}"), "secret-123");
        // ${VAR:-default} falls back when unset, uses the value when set.
        assert_eq!(expand_env("${NOPE_UNSET_VAR_XYZ:-fallback}"), "fallback");
        assert_eq!(expand_env("${ATOMCODE_CLIX_TEST_KEY:-fallback}"), "secret-123");
        // literals + unset + malformed pass through / empty as appropriate.
        assert_eq!(expand_env("sk-literal"), "sk-literal", "non-$ passes through");
        assert_eq!(expand_env("$NOPE_UNSET_VAR_XYZ"), "", "unset $VAR → empty");
        assert_eq!(expand_env("${unclosed"), "${unclosed", "malformed brace ref passes through");
    }

    #[test]
    fn first_nonempty_respects_precedence() {
        assert_eq!(
            first_nonempty([Some("  ".into()), None, Some("flag".into()), Some("env".into())]).as_deref(),
            Some("flag"),
            "first non-empty wins (blank skipped)"
        );
        assert_eq!(first_nonempty([None, Some("".into())]), None);
    }

    #[test]
    fn system_prompt_text_wins_over_file_and_none_default() {
        // inline text wins.
        assert_eq!(
            resolve_system_prompt(Some("CUSTOM PROMPT".into()), Some("ignored".into())).unwrap().as_deref(),
            Some("CUSTOM PROMPT")
        );
        // blank text falls through to None when no file.
        assert_eq!(resolve_system_prompt(Some("  ".into()), None).unwrap(), None);
        // nothing → None (built-in persona).
        assert_eq!(resolve_system_prompt(None, None).unwrap(), None);
        // file path is read.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("p.txt");
        std::fs::write(&p, "FROM FILE").unwrap();
        assert_eq!(
            resolve_system_prompt(None, Some(p.to_string_lossy().to_string())).unwrap().as_deref(),
            Some("FROM FILE")
        );
    }

    #[test]
    fn read_diff_file_reads_a_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("pr.diff");
        std::fs::write(&p, "diff --git a/x b/x\n+added\n").unwrap();
        let got = read_diff_file(p.to_str().unwrap()).unwrap();
        assert!(got.contains("+added"), "{got}");
        assert!(read_diff_file(d.path().join("nope.diff").to_str().unwrap()).is_err());
    }

    #[test]
    fn git_diff_reads_uncommitted_changes() {
        // A real tiny git repo: commit a file, modify it, expect the diff to show up.
        let d = tempfile::tempdir().unwrap();
        let repo = d.path();
        let git = |args: &[&str]| {
            Command::new("git").arg("-C").arg(repo).args(args).output().unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            return; // git unavailable in this environment → skip
        }
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        std::fs::write(repo.join("f.txt"), "two\n").unwrap();

        let diff = git_diff(repo, None, false).unwrap();
        assert!(diff.contains("-one") && diff.contains("+two"), "diff shows the edit: {diff}");
    }
}
