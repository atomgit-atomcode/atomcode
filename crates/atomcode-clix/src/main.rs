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
mod tel;

use anyhow::{bail, Context, Result};
use atomcode_kernel::agent::Agent;
use atomcode_kernel::event::{AgentCommand, AgentEvent, StopReason};
use atomcode_review::{
    build_review_agent_with_cancel, shared_review_deadline, Finding, ReviewAgentConfig,
};
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
    /// Disable anonymous usage telemetry for this invocation.
    #[arg(long = "no-telemetry")]
    no_telemetry: bool,
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
    /// Override built-in language review rules from this directory (`<dir>/<name>.md`,
    /// e.g. go.md / sql.md) — hot-tune rules without a rebuild. Missing names fall back
    /// to the built-ins.
    #[arg(long)]
    rules_dir: Option<PathBuf>,
    /// Disable the built-in language-rules injection entirely (e.g. for prompt A/B
    /// experiments that need a clean prompt).
    #[arg(long)]
    no_rules: bool,
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
    /// Hard cap on LLM rounds (tool-call iterations) for this review — the round safety
    /// fuse. Omit ⇒ UNLIMITED. On a large repo a small diff can otherwise send the model
    /// grepping/reading for an unbounded number of rounds; engineering callers bound it
    /// (e.g. `--max-rounds 35`). On the cap the run stops and reports findings gathered so far.
    #[arg(long)]
    max_rounds: Option<u32>,
    /// Absolute wall-clock cap (seconds) on the whole review. Omit ⇒ UNLIMITED. The only
    /// guard that also fires while a provider stalls mid-stream (keepalive bytes defeat
    /// `--stream-timeout`'s idle timer, and `--max-rounds` only checks at round boundaries).
    /// On the cap the run stops and reports findings gathered so far. E.g. `--max-duration 900`.
    #[arg(long)]
    max_duration: Option<u64>,
    /// Model context window in tokens — how much history the reviewer keeps before compacting.
    /// Overrides the config provider's `context_window` and the 128k built-in default. Set it to
    /// the REAL window of the provider behind `--base-url`/`--model` (e.g. a 1M custom LLM), so a
    /// wide-impact diff doesn't force the agent to re-read files it already saw. Omit ⇒ config
    /// value, else 128000.
    #[arg(long)]
    context_window: Option<u32>,
    /// Emit findings as JSON instead of a human-readable report.
    #[arg(long)]
    json: bool,
    /// Disable the `web_search` tool for this review. Use when the runtime has no/blocked web
    /// egress, so a web_search attempt can't fail and abort the whole review. Other read-only
    /// tools are unaffected. Default: web_search stays available.
    #[arg(long)]
    no_web: bool,
    /// Mount the code-graph tools (find_references/trace_callers/…) only when the repo has AT
    /// MOST this many git-tracked indexable source files; above it they're dropped (grep-only),
    /// since their O(repo) tree-sitter graph build blows the wall-clock budget on huge repos for
    /// no measured quality gain. Omit ⇒ UNLIMITED (always mount — bare-CLI default). Engineering
    /// callers reviewing huge repos (e.g. a kernel on NFS) set e.g. `--graph-max-files 8000`.
    /// Pass `0` to never mount the graph (force grep-only).
    #[arg(long)]
    graph_max_files: Option<u32>,
    /// Skip the coverage re-review pass (second agent pass over changed files that got zero
    /// findings). Engineering callers use this for isolated monorepo recipe PRs where a second
    /// pass mostly re-reads scaffold and burns the wall-clock budget. Default: coverage stays
    /// on after a finished first pass.
    #[arg(long)]
    no_coverage: bool,
    /// Load skill tools (`use_skill` / `list_skills`) from this directory. Repeatable
    /// (LOW→HIGH priority; later dirs override earlier on name collision). Each dir is
    /// scanned for `SKILL.md` (directory skill with bundled `scripts/` / `references/`)
    /// or single `<name>.md` files. Omit ⇒ NO skill tools mounted (bare-CLI behavior);
    /// only some deployments / repos opt into skills.
    #[arg(long = "skill-dir", value_name = "DIR")]
    skill_dirs: Vec<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // `atomcodex` shares the config tree with `atomcode` but is often invoked
    // directly, so it cannot rely on inheriting the variable from a parent.
    atomcode_config::distribution::bootstrap_home();

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
        (
            "--append-system-prompt-file",
            args.append_system_prompt_file.as_deref(),
        ),
    ];
    let on_stdin: Vec<&str> = stdin_users
        .iter()
        .filter(|(_, v)| *v == Some("-"))
        .map(|(n, _)| *n)
        .collect();
    if on_stdin.len() > 1 {
        bail!(
            "{} all read stdin; give all but one of them a file path",
            on_stdin.join(" and ")
        );
    }
    let repo = args
        .repo
        .canonicalize()
        .with_context(|| format!("repo not found: {}", args.repo.display()))?;

    // Two modes: a CUSTOM task (chat/explain/summary — no diff) or the built-in diff review.
    let custom_task = resolve_task(args.task.clone(), args.task_file.clone())?;
    // Language-rules section matched against the diff's changed files (diff mode only).
    let mut rules_section: Option<String> = None;
    // Changed-file set of the diff (diff mode only) — used to drop findings anchored to
    // files OUTSIDE the diff (a common hallucination: judging un-changed code as broken).
    let mut changed_files: Vec<String> = Vec::new();
    // The line-annotated diff (diff mode only) — kept so the coverage backstop can slice out
    // hunks for files the first pass left unreviewed and re-review just those.
    let mut annotated_diff = String::new();
    let (task, trace_label) = match custom_task {
        Some(t) => {
            let label = format!("custom task ({} chars)", t.len());
            (t, label)
        }
        None => {
            let diff = obtain_diff(&repo, &args)?;
            if diff.trim().is_empty() {
                // Honor --json even on an empty diff: emit a valid EMPTY envelope, not prose.
                // A bare "No changes" line makes downstream JSON parsers fail on `N...`; an
                // empty diff is a clean outcome (nothing to review), not a failure.
                if args.json {
                    println!("{}", render_json(&[], "No changes to review.", None)?);
                } else {
                    println!("No changes to review.");
                }
                return Ok(());
            }
            let label = format!("{} changed line(s)", diff.lines().count());
            // Changed-file set (from the `+++` lines) — drives rule matching AND the
            // out-of-diff finding filter below.
            changed_files = atomcode_review::changed_files_from_diff(&diff);
            // Language rules matched against the changed files.
            if !args.no_rules {
                let section = atomcode_review::render_rules_section(
                    &changed_files,
                    args.rules_dir.as_deref(),
                );
                if !section.is_empty() {
                    // Observability: confirm on stderr that rules actually got injected
                    // (the composed system prompt is not otherwise visible to callers).
                    eprintln!(
                        "[rules] injected for {} changed file(s) ({} chars)",
                        changed_files.len(),
                        section.len()
                    );
                    rules_section = Some(section);
                } else {
                    eprintln!("[rules] no language rules matched the changed files");
                }
            }
            // Explicit changed-file checklist: the agent decides what to read, and on
            // large diffs it sometimes skips an entire file (the single biggest source of
            // run-to-run recall variance). Force a one-by-one sweep with a confirmation in
            // the summary so a missed file becomes visible instead of silently dropped.
            let file_checklist = if changed_files.is_empty() {
                String::new()
            } else {
                let list = changed_files
                    .iter()
                    .map(|f| format!("- {f}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "\n\nYou MUST review EVERY one of the {} changed file(s) listed below, one \
                     at a time — investigate each file's changes and surrounding code before \
                     moving to the next; do NOT skip a file because it looks minor or \
                     non-core. In your closing summary, list each changed file and confirm you \
                     reviewed it (write \"no issues\" for the clean ones), so a missed file is \
                     visible.\n\nChanged files to review:\n{list}",
                    changed_files.len()
                )
            };
            // Prefix every hunk line with its REAL file line number so the model anchors
            // findings precisely instead of counting lines itself.
            let impact_plan = atomcode_review::render_review_impact_plan(&diff);
            let diff = atomcode_review::annotate_diff_line_numbers(&diff);
            let t = format!(
                "Review the following diff. Each hunk line is prefixed with its real file \
                 line number (`N: `) — use these numbers for `line_start`/`line_end`. \
                 Investigate the surrounding code with your read-only tools, then report \
                 each issue via `report_finding`. Report only real issues, each anchored \
                 to a concrete file and line.{file_checklist}\n\n{impact_plan}\n\n```diff\n{diff}\n```"
            );
            annotated_diff = diff;
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
    .context(
        "missing model: pass --model, set $ATOMCODE_MODEL, or add model to the config provider",
    )?;
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
        entry
            .and_then(|e| e.api_key.clone())
            .map(|k| expand_env(&k)),
    ])
    .unwrap_or_default();
    let context_window =
        resolve_context_window(args.context_window, entry.and_then(|e| e.context_window));

    let mut cfg = ReviewAgentConfig::new(api_key, base_url, model, &repo);
    cfg.context_window = context_window;
    cfg.stream_timeout = std::time::Duration::from_secs(args.stream_timeout);
    cfg.max_rounds = args.max_rounds;
    cfg.max_turn_duration = args.max_duration.map(std::time::Duration::from_secs);
    cfg.no_web = args.no_web;
    // Omit ⇒ keep the config default (usize::MAX = never degrade). A bound enables auto-degrade.
    if let Some(n) = args.graph_max_files {
        cfg.graph_max_indexed_files = n as usize;
    }
    // Skills: opt-in via --skill-dir (empty ⇒ no skill tools, bare-CLI behavior).
    // Canonicalize each dir so SKILL.md's `${CLAUDE_SKILL_DIR}` resolves absolute.
    cfg.skill_dirs = args
        .skill_dirs
        .iter()
        .map(|d| {
            d.canonicalize()
                .with_context(|| format!("--skill-dir not found: {}", d.display()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Full system-prompt override (flag text > file/stdin). None ⇒ built-in reviewer persona.
    cfg.persona =
        resolve_system_prompt(args.system_prompt.clone(), args.system_prompt_file.clone())?;
    // Appended sections compose after the persona: engine-injected language rules first,
    // then the caller's append (later text wins when guidance overlaps).
    let user_append = resolve_system_prompt(
        args.append_system_prompt.clone(),
        args.append_system_prompt_file.clone(),
    )?;
    cfg.persona_append = match (rules_section, user_append) {
        (Some(r), Some(u)) => Some(format!("{r}\n\n{u}")),
        (Some(r), None) => Some(r),
        (None, u) => u,
    };
    let model_label = cfg.model.clone();

    // Telemetry: the standalone reviewer runs its own kernel loop with NO turn-level
    // TelemetryHook, so we wrap its provider with a metering decorator — otherwise its LLM
    // rounds (the review's whole token spend) are invisible. A disabled sink no-ops; flushed
    // to disk after the run.
    let telemetry = tel::build_sink(args.config.as_deref(), args.no_telemetry);
    tel::maybe_show_notice(telemetry.is_enabled());
    let provider = tel::build_review_provider(&cfg).map_err(|e| anyhow::anyhow!(e))?;
    let provider = tel::meter_provider(provider, &telemetry, &cfg.base_url, &cfg.model);

    // ONE wall-clock cancel token + ONE timer shared by the first review pass AND the
    // coverage re-review pass. `--max-duration` is documented as the cap on the WHOLE
    // review; before this, each `build_review_agent_with` spawned its own timer from
    // zero, so the re-review could run the FULL duration AGAIN on top of time the
    // first pass already spent (observed: 200s first pass + 240s re-review = 440s on
    // a `--max-duration 240` run, blowing the caller's budget and CI timeouts).
    let review_deadline = shared_review_deadline(cfg.max_turn_duration);
    let (agent, report) =
        build_review_agent_with_cancel(&cfg, provider.clone(), review_deadline.clone());

    // Live trace on stderr (stdout stays clean for findings / --json). The run is one LLM
    // turn loop — without this the terminal looks frozen while the model thinks + calls tools.
    eprintln!("Running {trace_label} with {model_label} …");
    let run = run_review_streaming(agent, task).await;

    // Trace summary: tool-usage profile + token spend — exactly what you need to optimize.
    if run.tool_calls > 0 {
        let profile: Vec<String> = run
            .tool_counts
            .iter()
            .map(|(n, c)| format!("{n}×{c}"))
            .collect();
        eprintln!(
            "— trace — {} tool call(s): {}",
            run.tool_calls,
            profile.join(", ")
        );
    }
    if let Some(u) = run.usage {
        eprintln!(
            "— tokens — prompt {} / completion {} / cached {}",
            u.prompt, u.completion, u.cached
        );
    }

    let mut incomplete_reasons: Vec<String> = review_incomplete_reason("initial pass", &run)
        .into_iter()
        .collect();
    let mut findings = report.findings();
    // Drop findings anchored to files OUTSIDE the diff's changed set — the reviewer is
    // scoped to diff-introduced problems, but the model occasionally reads an un-changed
    // file and reports it (e.g. "this component is never rendered" anchored to a file not
    // in the diff). Deterministic guard, only when the changed set is known & non-empty
    // (empty ⇒ we can't trust it, so we don't filter).
    let dropped = drop_out_of_scope(&mut findings, &changed_files);
    if dropped > 0 {
        eprintln!(
            "[scope] dropped {dropped} finding(s) anchored outside the {} changed file(s)",
            changed_files.len()
        );
    }

    // Coverage backstop: on a wide diff the model sometimes declares "done" having reported on
    // only some changed files (observed: umi-ocr left 3/10 files unreviewed). Re-review JUST the
    // files that got zero findings (scoped sub-diff) once and merge — a deterministic guard the
    // model can't skip. Gated to diff mode with a known changed set (annotated_diff empty ⇒
    // task/custom mode, nothing to backstop). Lockfiles are already excluded by uncovered_files.
    //
    // Skip when:
    // - `--no-coverage` (engineering opt-out, e.g. isolated recipe monorepo PRs)
    // - initial pass cut short (Cancelled / Timeout / MaxRounds / …): a second agent only burns
    //   the shared --max-duration budget (observed: both Cancelled on 5min recipe runs)
    // - after priority / scaffold filter, nothing high-signal left
    if args.no_coverage {
        eprintln!("[coverage] skipped — --no-coverage");
    } else if !annotated_diff.is_empty() && incomplete_reasons.is_empty() {
        let uncovered = uncovered_files(&changed_files, &findings);
        let sub = sub_diff_for_files(&annotated_diff, &uncovered);
        if !sub.trim().is_empty() {
            eprintln!(
                "[coverage] {} changed file(s) had no findings; re-reviewing: {}",
                uncovered.len(),
                uncovered.join(", ")
            );
            let scoped_task = format!(
                "A prior review pass did NOT report on the changed file(s) below. Review EACH \
                 one thoroughly and report every real issue via `report_finding`; if a file is \
                 genuinely clean, that is fine. Each hunk line is prefixed with its real file \
                 line number (`N: `) — use these for `line_start`/`line_end`.\n\n```diff\n{sub}\n```"
            );
            let (agent2, report2) =
                build_review_agent_with_cancel(&cfg, provider.clone(), review_deadline.clone());
            let run2 = run_review_streaming(agent2, scoped_task).await;
            if run2.tool_calls > 0 {
                let profile: Vec<String> = run2
                    .tool_counts
                    .iter()
                    .map(|(n, c)| format!("{n}×{c}"))
                    .collect();
                eprintln!(
                    "— coverage trace — {} tool call(s): {}",
                    run2.tool_calls,
                    profile.join(", ")
                );
            }
            if let Some(reason) = review_incomplete_reason("coverage pass", &run2) {
                incomplete_reasons.push(reason);
            }
            let mut extra = report2.findings();
            drop_out_of_scope(&mut extra, &changed_files);
            let added = merge_findings(&mut findings, extra);
            eprintln!("[coverage] recovered {added} finding(s) from the re-review");
        } else if !changed_files.is_empty() {
            eprintln!(
                "[coverage] skipped — no high-signal uncovered files after filter \
                 (first_pass_findings={})",
                findings.len()
            );
        }
    } else if !annotated_diff.is_empty() && !incomplete_reasons.is_empty() {
        eprintln!(
            "[coverage] skipped — initial pass incomplete ({})",
            incomplete_reasons.join("; ")
        );
    }

    // Drain telemetry to disk AFTER all LLM work (first pass + any coverage re-review).
    telemetry.shutdown(tel::FLUSH_TIMEOUT).await;

    sort_findings(&mut findings);

    if args.json {
        println!("{}", render_json(&findings, &run.text, run.usage)?);
    } else if !findings.is_empty() {
        print!("{}", render_findings(&findings));
    } else if !incomplete_reasons.is_empty() {
        // Don't claim "clean" — at least one pass did not finish, so zero collected
        // findings is not evidence that the whole diff is clean.
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
    // Cut-short detection covers BOTH the initial and coverage passes. Otherwise a
    // clean first pass followed by a failed re-review could be reported as clean.
    if !incomplete_reasons.is_empty() {
        let why = incomplete_reasons.join("; ");
        if findings.is_empty() {
            bail!("review did not complete ({why}): no findings collected");
        }
        eprintln!(
            "warning: review ended early ({why}); {} finding(s) collected before it stopped",
            findings.len()
        );
    }
    Ok(())
}

/// Whether a review was CUT SHORT rather than finishing on the model's own terms: a
/// non-`Stopped` terminal (Cancelled via max-duration / Timeout / MaxRounds) or a surfaced
/// error. Combined with "no findings" this is a real failure — must not pass as a clean run.
fn review_incomplete(stop: StopReason, has_error: bool) -> bool {
    has_error || !matches!(stop, StopReason::Stopped)
}

/// Result of driving one review turn loop while live-tracing tool activity to stderr.
#[derive(Default)]
struct ReviewRun {
    /// The closing summary: assistant prose emitted AFTER the last tool call.
    /// Cleared on every `ToolStarted` so pre-call narration never leaks in.
    text: String,
    /// How the turn ended. `Stopped` = the model finished on its own; anything else
    /// (Cancelled via max-duration, Timeout, MaxRounds, ProviderError) means the review
    /// was CUT SHORT — must NOT be reported as a clean "no issues" run. Default `Stopped`.
    stop: StopReason,
    /// Last error surfaced, if any.
    error: Option<String>,
    /// Final token usage.
    usage: Option<atomcode_kernel::stream::TokenUsage>,
    /// Per-tool call counts (the usage profile).
    tool_counts: std::collections::BTreeMap<String, usize>,
    /// Total tool calls.
    tool_calls: usize,
}

fn review_incomplete_reason(label: &str, run: &ReviewRun) -> Option<String> {
    review_incomplete(run.stop, run.error.is_some()).then(|| {
        let reason = run
            .error
            .clone()
            .unwrap_or_else(|| format!("{:?}", run.stop));
        format!("{label}: {reason}")
    })
}

impl ReviewRun {
    /// Fold ONE agent event into the run, mutating accumulators in place.
    /// Returns `false` once the turn is terminal (the caller then stops reading).
    /// `call_names` maps tool-call id → name for the live stderr trace.
    fn apply(
        &mut self,
        ev: AgentEvent,
        call_names: &mut std::collections::HashMap<String, String>,
    ) -> bool {
        match ev {
            AgentEvent::ToolStarted { call } => {
                self.tool_calls += 1;
                *self.tool_counts.entry(call.name.clone()).or_default() += 1;
                call_names.insert(call.id.clone(), call.name.clone());
                // A tool call means whatever prose came before it was pre-call narration
                // ("let me read X…"), NOT the persona's Closing Summary. Drop it — only
                // text emitted AFTER the last tool call survives, which is exactly the
                // closing summary (persona §XI). Without this, every turn's narration
                // leaked into `text` and onto the PR comment.
                self.text.clear();
                eprintln!(
                    "  → {} {}",
                    call.name,
                    tool_hint(&call.name, &call.arguments)
                );
            }
            AgentEvent::ToolResult { result } => {
                let name = call_names
                    .get(&result.call_id)
                    .map(String::as_str)
                    .unwrap_or("tool");
                let mark = if result.is_error { "✗" } else { "✓" };
                eprintln!(
                    "    {mark} {name} ({} chars)",
                    result.content.chars().count()
                );
            }
            AgentEvent::TextDelta(t) => self.text.push_str(&t),
            // Each turn emits ONE per-turn usage figure; SUM across turns for the run total.
            // (Last-wins kept only the final turn and silently under-reported the whole
            // agentic run — e.g. a 40-turn review looked like one 50k-prompt call.)
            AgentEvent::Usage(meta) => {
                let u = self.usage.get_or_insert(Default::default());
                u.prompt += meta.tokens.prompt;
                u.completion += meta.tokens.completion;
                u.cached += meta.tokens.cached;
            }
            AgentEvent::Error { message, .. } => {
                eprintln!("    [error] {message}");
                self.error = Some(message);
            }
            AgentEvent::Warning(w) => eprintln!("    [warn] {w}"),
            AgentEvent::TurnComplete { reason } => {
                self.stop = reason;
                return false;
            }
            _ => {}
        }
        true
    }
}

/// Spawn the review agent, kick off the turn, and stream a live execution trace to stderr:
/// each tool call (name + key args) and its result (ok/err + size), plus a final
/// tool-usage + token profile. Returns the accumulated summary text + stats.
async fn run_review_streaming(agent: Agent, task: String) -> ReviewRun {
    let mut handle = agent.spawn();
    let _ = handle.commands.send(AgentCommand::SendMessage {
        text: task,
        images: vec![],
    });

    let mut run = ReviewRun::default();
    let mut call_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    while let Some(ev) = handle.events.recv().await {
        if !run.apply(ev, &mut call_names) {
            break;
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
    atomcode_capabilities::provider::is_atomgit_gateway(base_url)
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
    language: Option<String>,
    #[serde(default)]
    default_provider: Option<String>,
    #[serde(default)]
    providers: HashMap<String, ProviderEntry>,
}

#[derive(Default)]
pub(crate) struct ConfigSelection {
    pub(crate) provider: Option<ProviderEntry>,
    pub(crate) language: Option<atomcode_config::locale::Locale>,
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

fn select_config(fc: &FileConfig, provider: Option<&str>) -> ConfigSelection {
    ConfigSelection {
        provider: pick_provider(fc, provider),
        language: fc
            .language
            .as_deref()
            .and_then(|language| language.parse().ok()),
    }
}

/// `~/.atomcode/config.toml` (honors $ATOMCODE_HOME, else $HOME / %USERPROFILE%).
fn default_config_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("ATOMCODE_HOME") {
        return Some(PathBuf::from(home).join("config.toml"));
    }
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".atomcode").join("config.toml"))
}

/// Resolve the effective model context window: explicit `--context-window` flag wins, else the
/// config provider's `context_window`, else the 128k built-in default. Kept pure for testing.
fn resolve_context_window(flag: Option<u32>, entry: Option<u32>) -> u32 {
    flag.or(entry).unwrap_or(128_000)
}

/// Load the top-level language and selected provider from the config file.
/// - default path absent → an empty selection (flags/env can still supply everything);
/// - explicit `--config` path unreadable → `Err` (the user pointed at it);
/// - file present but MALFORMED → `Err` (don't silently fall through to a confusing
///   "missing base URL" later);
/// - file parses but has no matching provider → preserve the language with no provider.
pub(crate) fn load_config_selection(
    config_override: Option<&Path>,
    provider: Option<&str>,
) -> Result<ConfigSelection> {
    let (path, explicit) = match config_override {
        Some(p) => (p.to_path_buf(), true),
        None => match default_config_path() {
            Some(p) => (p, false),
            None => return Ok(ConfigSelection::default()),
        },
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if explicit => {
            return Err(anyhow::Error::new(e))
                .with_context(|| format!("cannot read config file: {}", path.display()))
        }
        Err(_) => return Ok(ConfigSelection::default()), // default path simply absent — fine
    };
    let fc = parse_file_config(&text)
        .with_context(|| format!("malformed config file: {}", path.display()))?;
    Ok(select_config(&fc, provider))
}

pub(crate) fn load_provider_entry(
    config_override: Option<&Path>,
    provider: Option<&str>,
) -> Result<Option<ProviderEntry>> {
    Ok(load_config_selection(config_override, provider)?.provider)
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
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("failed to read task from stdin")?;
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
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("failed to read system prompt from stdin")?;
            buf
        } else {
            std::fs::read_to_string(&f)
                .with_context(|| format!("failed to read system prompt file: {f}"))?
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
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("failed to read diff from stdin")?;
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
            .then(
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
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
        out.push_str(&format!(
            "[{} {:.2}] {}  {}\n",
            f.priority, f.confidence, loc, f.title
        ));
        for line in f.body.lines() {
            out.push_str(&format!("    {line}\n"));
        }
        out.push('\n');
    }
    out
}

/// Drop findings anchored to files OUTSIDE the diff's changed set; returns how many were
/// dropped. Safety valve: an empty `changed_files` (unknown / task mode) disables the
/// filter entirely — we never treat "we don't know the changed set" as "drop everything".
fn drop_out_of_scope(findings: &mut Vec<Finding>, changed_files: &[String]) -> usize {
    if changed_files.is_empty() {
        return 0;
    }
    let before = findings.len();
    findings.retain(|f| changed_files.iter().any(|c| c == &f.file_path));
    before - findings.len()
}

/// Hard cap on coverage re-review fan-out. Wide monorepo-style PRs otherwise re-review nearly
/// every file and roughly double wall-clock. High-signal paths (source / build recipes) first.
const MAX_COVERAGE_REREVIEW_FILES: usize = 8;

/// Priority for coverage re-review (higher first). Scaffold scores low so a capped second pass
/// still spends budget on real code when the first pass already found issues.
fn coverage_priority(path: &str) -> i32 {
    let lower = path.to_ascii_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    if is_coverage_scaffold_file(path) {
        return 10;
    }
    const SRC: &[&str] = &[
        ".c", ".cc", ".cpp", ".cxx", ".h", ".hpp", ".hh", ".go", ".rs", ".py", ".java", ".kt",
        ".swift", ".ts", ".tsx", ".js", ".jsx", ".m", ".mm", ".cs", ".rb", ".php", ".scala",
    ];
    for ext in SRC {
        if lower.ends_with(ext) {
            if base == "conanfile.py" || base == "cmakelists.txt" {
                return 95;
            }
            return 100;
        }
    }
    if base == "cmakelists.txt"
        || base.starts_with("dockerfile")
        || base == "makefile"
        || base == "meson.build"
        || base == "build.gradle"
        || base == "build.gradle.kts"
    {
        return 95;
    }
    if lower.ends_with(".patch") || lower.ends_with(".diff") {
        if base.contains("tests-t") || lower.contains("/tests-") || lower.contains("-tests-") {
            return 30;
        }
        return 75;
    }
    if base == "conandata.yml" || base == "conandata.yaml" {
        return 70;
    }
    if lower.ends_with(".yml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".toml")
        || lower.ends_with(".json")
        || lower.ends_with(".sh")
    {
        return 55;
    }
    if lower.ends_with(".md") {
        return 40;
    }
    50
}

/// Recipe bookkeeping that rarely needs a second pass once the first pass already reported
/// on real recipe code (conanfile / patches / tests).
fn is_coverage_scaffold_file(path: &str) -> bool {
    let base = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "notes.md"
            | "commands.json"
            | "manifest.yml"
            | "manifest.yaml"
            | "readme.md"
            | "changelog.md"
    )
}

/// Changed files that received ZERO findings and are worth a focused second look. Drives the
/// coverage backstop: on a wide diff the reviewer sometimes declares "done" having only
/// reported on some files (observed: umi-ocr left 3/10 files unreviewed). Re-reviewing just
/// these recovers the gap.
///
/// Filters:
/// - lockfiles / low-signal (`is_low_signal_file`)
/// - when first pass already has findings: drop scaffold (notes/commands/manifest)
/// - cap to [`MAX_COVERAGE_REREVIEW_FILES`] by priority (source / recipe first)
///
/// Expects `findings` already scope-filtered to `changed_files` (see [`drop_out_of_scope`]).
/// Empty `changed_files` (task mode) yields none.
fn uncovered_files(changed_files: &[String], findings: &[Finding]) -> Vec<String> {
    let first_pass_had_findings = !findings.is_empty();
    let mut files: Vec<String> = changed_files
        .iter()
        .filter(|c| !atomcode_review::is_low_signal_file(c))
        .filter(|c| !findings.iter().any(|f| &f.file_path == *c))
        .filter(|c| !(first_pass_had_findings && is_coverage_scaffold_file(c)))
        .cloned()
        .collect();
    if files.is_empty() {
        return files;
    }
    if files.len() <= MAX_COVERAGE_REREVIEW_FILES {
        return files;
    }
    files.sort_by(|a, b| {
        coverage_priority(b)
            .cmp(&coverage_priority(a))
            .then_with(|| a.cmp(b))
    });
    let dropped = files.len() - MAX_COVERAGE_REREVIEW_FILES;
    files.truncate(MAX_COVERAGE_REREVIEW_FILES);
    eprintln!(
        "[coverage] capped re-review to {MAX_COVERAGE_REREVIEW_FILES} highest-priority file(s); dropped {dropped} lower-priority uncovered file(s)"
    );
    files
}

/// Fold `extra` findings into `findings`, skipping duplicates (same file + start line +
/// title). Returns how many were actually added. Used by the coverage backstop so a re-review
/// that re-flags an issue the first pass already caught does not double-report it.
fn merge_findings(findings: &mut Vec<Finding>, extra: Vec<Finding>) -> usize {
    let mut added = 0;
    for e in extra {
        let dup = findings.iter().any(|f| {
            f.file_path == e.file_path && f.line_start == e.line_start && f.title == e.title
        });
        if !dup {
            findings.push(e);
            added += 1;
        }
    }
    added
}

/// Extract from a unified diff only the file sections for `files` (paths as returned by
/// [`changed_files_from_diff`](atomcode_review::changed_files_from_diff), i.e. `b/`-stripped).
/// Preserves each kept section verbatim so the scoped re-review sees real hunks + line context.
fn sub_diff_for_files(full_diff: &str, files: &[String]) -> String {
    let wanted =
        |path: &Option<String>| path.as_ref().is_some_and(|p| files.iter().any(|f| f == p));
    let mut out = String::new();
    let mut section = String::new();
    let mut section_path: Option<String> = None;
    for line in full_diff.lines() {
        if line.starts_with("diff --git ") && !section.is_empty() {
            if wanted(&section_path) {
                out.push_str(&section);
            }
            section.clear();
            section_path = None;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            let p = rest.trim();
            if p != "/dev/null" {
                section_path = Some(p.strip_prefix("b/").unwrap_or(p).to_string());
            }
        }
        section.push_str(line);
        section.push('\n');
    }
    if wanted(&section_path) {
        out.push_str(&section);
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
    serde_json::to_string_pretty(&ReviewJson {
        findings,
        text: text.trim(),
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_short_runs_are_incomplete() {
        // 正常完成、无 error → 完整（可当 clean）。
        assert!(!review_incomplete(StopReason::Stopped, false));
        // 关键回归：max-duration 的 cancel 发 Cancelled 但 error=None——必须判为未完成，
        // 否则 0 finding 时会被当成"审完无问题"假成功入库。
        assert!(review_incomplete(StopReason::Cancelled, false));
        // 其它切短终态同样未完成。
        assert!(review_incomplete(StopReason::Timeout, false));
        assert!(review_incomplete(StopReason::MaxRounds, false));
        // 有 error 即使 Stopped 也算未完成。
        assert!(review_incomplete(StopReason::Stopped, true));
    }

    #[test]
    fn coverage_pass_terminal_is_reported_with_its_own_reason() {
        let run = ReviewRun {
            stop: StopReason::ToolLoopDetected,
            ..ReviewRun::default()
        };
        assert_eq!(
            review_incomplete_reason("coverage pass", &run).as_deref(),
            Some("coverage pass: ToolLoopDetected")
        );

        let run = ReviewRun {
            stop: StopReason::Stopped,
            error: Some("stream closed without terminal".into()),
            ..ReviewRun::default()
        };
        assert_eq!(
            review_incomplete_reason("coverage pass", &run).as_deref(),
            Some("coverage pass: stream closed without terminal")
        );
    }

    #[test]
    fn closing_summary_survives_pre_call_narration_dropped() {
        use atomcode_kernel::tool::ToolCall;
        let tool = |id: &str, name: &str| AgentEvent::ToolStarted {
            call: ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: "{}".into(),
            },
        };
        // 模型每轮调工具前都会叙述（"let me read X…"），那是过程噪声，不该进 text；
        // 只有最后一次工具调用之后输出的 Closing Summary（persona §XI）才该保留。
        let events = [
            AgentEvent::TextDelta("Now let me read the kernel file…".into()),
            tool("c1", "read_file"),
            AgentEvent::TextDelta("Now let me check validation.go…".into()),
            tool("c2", "report_finding"),
            AgentEvent::TextDelta("## 审查总结\nP0: 1, P1: 2\n整体风险：HIGH".into()),
            AgentEvent::TurnComplete {
                reason: StopReason::Stopped,
            },
        ];
        let mut run = ReviewRun::default();
        let mut names = std::collections::HashMap::new();
        for ev in events {
            run.apply(ev, &mut names);
        }
        assert_eq!(
            run.text.trim(),
            "## 审查总结\nP0: 1, P1: 2\n整体风险：HIGH",
            "只保留末轮 Closing Summary，丢弃工具调用前的过程叙述",
        );
    }

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

    fn finding_in(file: &str) -> Finding {
        let mut f = finding("P1", 0.9, "t");
        f.file_path = file.into();
        f
    }

    #[test]
    fn scope_drops_findings_outside_changed_set() {
        let changed = vec!["a.go".to_string(), "pkg/b.go".to_string()];
        let mut fs = vec![
            finding_in("a.go"),
            finding_in("pkg/b.go"),
            finding_in("untouched.go"),
        ];
        let dropped = drop_out_of_scope(&mut fs, &changed);
        assert_eq!(dropped, 1, "the out-of-diff finding is dropped");
        assert!(fs.iter().all(|f| f.file_path != "untouched.go"));
        assert_eq!(fs.len(), 2);
    }

    #[test]
    fn scope_empty_changed_set_disables_filter() {
        // Unknown changed set (e.g. task mode) must NOT drop everything.
        let mut fs = vec![finding_in("x.go"), finding_in("y.go")];
        assert_eq!(drop_out_of_scope(&mut fs, &[]), 0);
        assert_eq!(
            fs.len(),
            2,
            "nothing dropped when the changed set is unknown"
        );
    }

    #[test]
    fn uncovered_reports_changed_files_with_no_findings() {
        // 3 changed files, findings only on one → the other two are uncovered.
        let changed = vec!["a.go".to_string(), "b.go".to_string(), "c.go".to_string()];
        let fs = vec![finding_in("a.go")];
        let mut got = uncovered_files(&changed, &fs);
        got.sort();
        assert_eq!(got, vec!["b.go".to_string(), "c.go".to_string()]);
    }

    #[test]
    fn uncovered_empty_when_every_file_covered() {
        let changed = vec!["a.go".to_string(), "b.go".to_string()];
        let fs = vec![finding_in("a.go"), finding_in("b.go")];
        assert!(uncovered_files(&changed, &fs).is_empty());
    }

    #[test]
    fn uncovered_skips_lockfiles_and_manifests() {
        // A lockfile with no finding is NOT "uncovered" — re-reviewing it wastes a pass.
        let changed = vec![
            "Cargo.lock".to_string(),
            "go.sum".to_string(),
            "web/package-lock.json".to_string(),
            "real.go".to_string(),
        ];
        let fs: Vec<Finding> = vec![];
        assert_eq!(uncovered_files(&changed, &fs), vec!["real.go".to_string()]);
    }

    #[test]
    fn uncovered_empty_when_changed_set_unknown() {
        // No changed set (task mode) → nothing to backstop.
        assert!(uncovered_files(&[], &[finding_in("x.go")]).is_empty());
    }

    #[test]
    fn uncovered_drops_scaffold_when_first_pass_already_found_issues() {
        // First pass found something on conanfile → don't re-review notes/commands.
        let changed = vec![
            "pkg/conanfile.py".to_string(),
            "pkg/notes.md".to_string(),
            "pkg/commands.json".to_string(),
            "pkg/util.c".to_string(),
        ];
        let fs = vec![finding_in("pkg/conanfile.py")];
        let mut got = uncovered_files(&changed, &fs);
        got.sort();
        assert_eq!(got, vec!["pkg/util.c".to_string()]);
    }

    #[test]
    fn uncovered_keeps_scaffold_when_first_pass_found_nothing() {
        // 0 findings overall: still allow scaffold into the pool (may be filtered by cap).
        let changed = vec!["pkg/notes.md".to_string(), "pkg/conanfile.py".to_string()];
        let fs: Vec<Finding> = vec![];
        let mut got = uncovered_files(&changed, &fs);
        got.sort();
        assert_eq!(
            got,
            vec![
                "pkg/conanfile.py".to_string(),
                "pkg/notes.md".to_string()
            ]
        );
    }

    #[test]
    fn uncovered_caps_and_prefers_source() {
        let mut changed: Vec<String> = (1..=10)
            .map(|i| format!("patches/00{i:02}-tests-t{i:04}.pl.patch"))
            .collect();
        changed.push("src/core.go".to_string());
        changed.push("conanfile.py".to_string());
        let fs: Vec<Finding> = vec![];
        let got = uncovered_files(&changed, &fs);
        assert_eq!(got.len(), MAX_COVERAGE_REREVIEW_FILES);
        assert!(got.contains(&"src/core.go".to_string()), "{got:?}");
        assert!(got.contains(&"conanfile.py".to_string()), "{got:?}");
    }

    const TWO_FILE_DIFF: &str = "diff --git a/a.go b/a.go\n\
--- a/a.go\n+++ b/a.go\n@@ -1 +1 @@\n-old\n+newA\n\
diff --git a/pkg/b.go b/pkg/b.go\n\
--- a/pkg/b.go\n+++ b/pkg/b.go\n@@ -1 +1 @@\n-old\n+newB\n";

    #[test]
    fn sub_diff_extracts_only_requested_file() {
        let got = sub_diff_for_files(TWO_FILE_DIFF, &["pkg/b.go".to_string()]);
        assert!(
            got.contains("diff --git a/pkg/b.go b/pkg/b.go"),
            "keeps b.go section: {got}"
        );
        assert!(got.contains("+newB"), "keeps b.go hunk: {got}");
        assert!(
            !got.contains("a.go"),
            "drops the a.go section entirely: {got}"
        );
    }

    #[test]
    fn sub_diff_empty_when_no_file_matches() {
        assert!(sub_diff_for_files(TWO_FILE_DIFF, &["nope.go".to_string()]).is_empty());
        assert!(sub_diff_for_files(TWO_FILE_DIFF, &[]).is_empty());
    }

    #[test]
    fn sub_diff_keeps_all_when_all_requested() {
        let got = sub_diff_for_files(TWO_FILE_DIFF, &["a.go".to_string(), "pkg/b.go".to_string()]);
        assert!(
            got.contains("+newA") && got.contains("+newB"),
            "both hunks kept: {got}"
        );
    }

    #[test]
    fn merge_adds_new_and_skips_duplicates() {
        let mut base = vec![finding_in("a.go")]; // a.go:1 "t"
                                                 // one genuine new finding, one exact duplicate of the existing one.
        let mut dup = finding_in("a.go"); // same file+line+title as base[0]
        dup.body = "different body but same identity".into();
        let extra = vec![finding_in("qrcode.go"), dup];
        let added = merge_findings(&mut base, extra);
        assert_eq!(added, 1, "only the qrcode.go finding is new");
        assert_eq!(base.len(), 2);
        assert!(
            base.iter().any(|f| f.file_path == "qrcode.go"),
            "new file folded in"
        );
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
        let usage = atomcode_kernel::stream::TokenUsage {
            prompt: 10,
            completion: 5,
            cached: 2,
        };
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
        assert!(
            out.contains("2 finding(s): 1 P0, 0 P1, 1 P2, 0 P3"),
            "{out}"
        );
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
        assert_eq!(
            e.base_url.as_deref(),
            Some("https://llm-api.atomgit.com/v1")
        );
        assert_eq!(e.context_window, Some(1_000_000));
        assert_eq!(e.api_key, None, "atomgit entry has no api_key");
    }

    #[test]
    fn provider_override_selects_named_entry() {
        let fc = parse_file_config(SAMPLE).unwrap();
        let e = pick_provider(&fc, Some("openrouter")).expect("named provider resolves");
        assert_eq!(e.model.as_deref(), Some("stepfun/step-3.7-flash"));
        assert_eq!(e.api_key.as_deref(), Some("$OPENROUTER_API_KEY"));
        assert!(
            pick_provider(&fc, Some("nope")).is_none(),
            "unknown provider → None"
        );
    }

    #[test]
    fn ignores_unrelated_keys_and_missing_default() {
        // No default_provider, extra top-level keys → still parses; default pick → None.
        let fc =
            parse_file_config("language = \"zh\"\n[providers.x]\nmodel=\"m\"\nbase_url=\"u\"\n")
                .unwrap();
        assert!(pick_provider(&fc, None).is_none());
        assert!(pick_provider(&fc, Some("x")).is_some());
    }

    #[test]
    fn selected_config_carries_top_level_language() {
        let fc = parse_file_config(
            "language = \"zh\"\ndefault_provider = \"x\"\n[providers.x]\nmodel=\"m\"\nbase_url=\"u\"\n",
        )
        .unwrap();
        let selected = select_config(&fc, None);

        assert_eq!(
            selected.language,
            Some(atomcode_config::locale::Locale::ZhCn)
        );
        assert!(selected.provider.is_some());
    }

    #[test]
    fn coding_config_keeps_top_level_language_without_a_provider() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("config.toml");
        std::fs::write(&path, "language = \"zh\"\n").unwrap();

        let selected = load_config_selection(Some(&path), None).unwrap();

        assert!(selected.provider.is_none());
        assert_eq!(
            selected.language,
            Some(atomcode_config::locale::Locale::ZhCn)
        );
    }

    #[test]
    fn detects_signing_gateways_by_host() {
        assert!(is_signing_gateway("https://llm-api.atomgit.com/v1"));
        assert!(is_signing_gateway(
            "https://api-ai.gitcode.com/v1/chat/completions"
        ));
        assert!(is_signing_gateway("https://pre-llm-api-cce.atomgit.com/v1"));
        // plain providers are fine.
        assert!(!is_signing_gateway("https://openrouter.ai/api/v1"));
        assert!(!is_signing_gateway("https://api.deepseek.com/v1"));
        // a lookalike path must NOT trip the host check.
        assert!(!is_signing_gateway(
            "https://evil.com/llm-api.atomgit.com/v1"
        ));
    }

    #[test]
    fn load_provider_entry_surfaces_malformed_config() {
        let d = tempfile::tempdir().unwrap();
        // Malformed TOML at an explicit --config path → Err (not silently None).
        let bad = d.path().join("bad.toml");
        std::fs::write(&bad, "this is = = not valid toml [[[").unwrap();
        assert!(
            load_provider_entry(Some(&bad), None).is_err(),
            "malformed config must error"
        );
        // Explicit but missing path → Err.
        let missing = d.path().join("nope.toml");
        assert!(
            load_provider_entry(Some(&missing), None).is_err(),
            "explicit missing config errors"
        );
        // Valid config, unknown provider → Ok(None).
        let good = d.path().join("good.toml");
        std::fs::write(&good, SAMPLE).unwrap();
        assert!(load_provider_entry(Some(&good), Some("nope"))
            .unwrap()
            .is_none());
        assert!(
            load_provider_entry(Some(&good), None).unwrap().is_some(),
            "default_provider resolves"
        );
    }

    #[test]
    fn expand_env_resolves_dollar_refs() {
        std::env::set_var("ATOMCODE_CLIX_TEST_KEY", "secret-123");
        // $VAR and ${VAR} both resolve.
        assert_eq!(expand_env("$ATOMCODE_CLIX_TEST_KEY"), "secret-123");
        assert_eq!(expand_env("${ATOMCODE_CLIX_TEST_KEY}"), "secret-123");
        // ${VAR:-default} falls back when unset, uses the value when set.
        assert_eq!(expand_env("${NOPE_UNSET_VAR_XYZ:-fallback}"), "fallback");
        assert_eq!(
            expand_env("${ATOMCODE_CLIX_TEST_KEY:-fallback}"),
            "secret-123"
        );
        // literals + unset + malformed pass through / empty as appropriate.
        assert_eq!(
            expand_env("sk-literal"),
            "sk-literal",
            "non-$ passes through"
        );
        assert_eq!(expand_env("$NOPE_UNSET_VAR_XYZ"), "", "unset $VAR → empty");
        assert_eq!(
            expand_env("${unclosed"),
            "${unclosed",
            "malformed brace ref passes through"
        );
    }

    #[test]
    fn resolve_context_window_flag_wins_then_entry_then_default() {
        // --context-window flag overrides everything.
        assert_eq!(
            resolve_context_window(Some(1_000_000), Some(128_000)),
            1_000_000
        );
        // No flag → config provider's context_window.
        assert_eq!(resolve_context_window(None, Some(200_000)), 200_000);
        // Neither → 128k built-in default.
        assert_eq!(resolve_context_window(None, None), 128_000);
    }

    #[test]
    fn first_nonempty_respects_precedence() {
        assert_eq!(
            first_nonempty([
                Some("  ".into()),
                None,
                Some("flag".into()),
                Some("env".into())
            ])
            .as_deref(),
            Some("flag"),
            "first non-empty wins (blank skipped)"
        );
        assert_eq!(first_nonempty([None, Some("".into())]), None);
    }

    #[test]
    fn system_prompt_text_wins_over_file_and_none_default() {
        // inline text wins.
        assert_eq!(
            resolve_system_prompt(Some("CUSTOM PROMPT".into()), Some("ignored".into()))
                .unwrap()
                .as_deref(),
            Some("CUSTOM PROMPT")
        );
        // blank text falls through to None when no file.
        assert_eq!(
            resolve_system_prompt(Some("  ".into()), None).unwrap(),
            None
        );
        // nothing → None (built-in persona).
        assert_eq!(resolve_system_prompt(None, None).unwrap(), None);
        // file path is read.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("p.txt");
        std::fs::write(&p, "FROM FILE").unwrap();
        assert_eq!(
            resolve_system_prompt(None, Some(p.to_string_lossy().to_string()))
                .unwrap()
                .as_deref(),
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
            Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            return; // git unavailable in this environment → skip
        }
        let assert_git = |args: &[&str]| {
            let out = git(args);
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        };
        assert_git(&["config", "user.email", "t@t"]);
        assert_git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "one\n").unwrap();
        assert_git(&["add", "."]);
        assert_git(&[
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--no-verify",
            "-qm",
            "chore: initialize test repo",
        ]);
        std::fs::write(repo.join("f.txt"), "two\n").unwrap();

        let diff = git_diff(repo, None, false).unwrap();
        assert!(
            diff.contains("-one") && diff.contains("+two"),
            "diff shows the edit: {diff}"
        );
    }
}
