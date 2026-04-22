// crates/atomcode-tuix/src/event_loop/commands.rs
//
// Slash-command dispatcher. Everything the user can invoke by typing
// `/name` lives here — built-in info commands, modal openers, the cd
// helper, and the blocking OAuth flow that suspends the reader + renderer.
//
// New commands should be:
//   1. Registered in `CommandRegistry::builtin` (crates/.../commands.rs)
//   2. Added as an arm in `execute_slash_command` below
//   3. Any long handler factored to a private helper in this file
//
// Modals open by pushing `Some(Box::new(...))` into `active_modal` — the
// handler arms for `/model`, `/resume`, `/provider` show the pattern.

use std::path::PathBuf;

use anyhow::Result;
use atomcode_core::agent::AgentCommand;
use atomcode_core::config::Config;
use atomcode_core::config::provider::ProviderConfig;

use super::{save_and_reload, LoopCtx};
use crate::modals::{DirPicker, IssueWizard, Modal, ModelPicker, ProviderWizard, SessionPicker};
use crate::render::{Renderer, UiLine};
use crate::state::UiState;

/// Maximum recent project dirs we keep in memory + persist to disk.
const MAX_RECENT_DIRS: usize = 5;

/// Provider name used for the AtomGit OAuth provider entry in config.
const OAUTH_PROVIDER_NAME: &str = "AtomGit";

pub(super) fn execute_slash_command(
    cmd: &str,
    arg: &str,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    active_modal: &mut Option<Box<dyn Modal>>,
    fixissue_pending: &mut Option<atomcode_core::atomgit::IssueRef>,
    fixissue_buffer: &mut String,
) -> Result<()> {
    // Built-in commands are all lowercase ASCII; normalise the user's
    // input so `/SESSION`, `/Session`, `/sEssIon` all hit the same arm
    // as `/session`. `arg` is left untouched — paths / URLs are
    // case-sensitive in general.
    let cmd_lower = cmd.to_ascii_lowercase();
    let cmd = cmd_lower.as_str();
    match cmd {
        "quit" | "exit" => {
            ctx.agent.cmd_tx.send(AgentCommand::Shutdown).ok();
        }
        "help" => {
            renderer.render(UiLine::CommandOutput(ctx.commands.help_text()));
            renderer.flush();
        }
        "config" => {
            // Head: current active provider + config path so users know
            // which provider is talking and where to edit.
            let mut txt = format!(
                "  Provider: {}\n  Config: {}\n\n",
                ctx.config.default_provider,
                Config::default_path().display(),
            );
            // Body: one minimal runnable example + pointer to the full
            // reference so users know where to get Claude / OpenAI /
            // Ollama variants without flooding the terminal here.
            txt.push_str(
                "  Example:\n\
                 \n\
                 ```toml\n\
                 default_provider = \"deepseek\"\n\
                 \n\
                 [providers.deepseek]\n\
                 type           = \"openai\"\n\
                 api_key        = \"sk-...\"\n\
                 model          = \"deepseek-chat\"\n\
                 base_url       = \"https://api.deepseek.com/v1\"\n\
                 context_window = 64000\n\
                 ```\n\
                 \n\
                 Full reference: docs/config.example.toml (every field, every provider flavour).\n\
                 Edit the file, then run /reload — no restart needed.\n",
            );
            renderer.render(UiLine::CommandOutput(txt));
            renderer.flush();
        }
        "reload" => {
            // Re-read ~/.atomcode/config.toml from disk and push it to the
            // running daemon. Streaming-safe: the agent picks the new config
            // up on the *next* turn; anything already in-flight finishes on
            // the old config (ReloadConfig is queued behind the current
            // AgentCommand stream, not a hot swap).
            let path = Config::default_path();
            match Config::load(&path) {
                Ok(new_cfg) => {
                    let new_default = new_cfg.default_provider.clone();
                    let new_model = new_cfg
                        .providers
                        .get(&new_default)
                        .map(|p| p.model.clone())
                        .unwrap_or_else(|| new_default.clone());
                    ctx.config = new_cfg.clone();
                    ctx.model_name = new_model.clone();
                    ctx.agent
                        .cmd_tx
                        .send(AgentCommand::ReloadConfig(new_cfg))
                        .ok();
                    renderer.render(UiLine::CommandOutput(format!(
                        "  Config reloaded. Active: {} · {}\n",
                        new_default, new_model,
                    )));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(format!(
                        "reload failed: {} (kept previous config)",
                        e
                    )));
                }
            }
            renderer.flush();
        }
        "clear" => {
            // Physical clear via the renderer (keeps cached footer state
            // coherent with the terminal). Scrollback is preserved by
            // most terminals — \x1b[3J would nuke it, which we don't
            // want; `clear_screen` emits \x1b[2J\x1b[H.
            renderer.clear_screen();
            let dir_display = ctx.working_dir.to_string_lossy().to_string();
            renderer.render(UiLine::Welcome {
                model: ctx.model_name.clone(),
                working_dir: dir_display,
            });
            renderer.flush();
        }
        "session" => {
            // Start fresh: tell the agent to drop conversation history,
            // clear the scrollback + type-ahead queue + UI state, and
            // redraw the welcome screen so the user sees they're in a
            // brand-new session. Ports `/session` from the legacy TUI.
            ctx.agent.cmd_tx.send(AgentCommand::ClearConversation).ok();
            state.total_tokens = 0;
            state.thinking_idx = 0;
            state.on_turn_complete();
            // New session = new session file on disk. Old session
            // (already saved at its last TurnComplete) stays on disk so
            // it can still be `/resume`d; we just stop writing into it.
            ctx.current_session = atomcode_core::session::Session::default_session(
                ctx.working_dir.clone(),
            );
            // `reset()` wipes the terminal AND the renderer's cached
            // footer/stream state, so the next Welcome renders against
            // a known (row 1, col 1) anchor. This is what makes
            // /session behave like a fresh launch.
            renderer.reset();
            let dir_display = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
            renderer.render(UiLine::Welcome {
                model: ctx.model_name.clone(),
                working_dir: dir_display,
            });
            renderer.render(UiLine::CommandOutput("  New session started.\n".into()));
            renderer.flush();
        }
        "model" => {
            if ctx.config.providers.is_empty() {
                renderer.render(UiLine::CommandOutput(
                    "  No providers configured.\n".into(),
                ));
                renderer.flush();
            } else {
                *active_modal = Some(Box::new(ModelPicker::open(&ctx.config)));
            }
        }
        "resume" => match ctx.session_manager.list() {
            Ok(all) => {
                let sessions: Vec<_> = all.into_iter().filter(|s| s.message_count > 0).collect();
                if sessions.is_empty() {
                    renderer.render(UiLine::CommandOutput(
                        "  No previous sessions found. Start a conversation first.\n".into(),
                    ));
                    renderer.flush();
                } else {
                    *active_modal = Some(Box::new(SessionPicker::open(sessions)));
                }
            }
            Err(e) => {
                renderer.render(UiLine::Error(format!("list sessions failed: {}", e)));
                renderer.flush();
            }
        },
        "provider" => {
            *active_modal = Some(Box::new(ProviderWizard::MainMenu { selected: 0 }));
            renderer.render(UiLine::CommandOutput(
                "  Provider management — Add / Edit / Delete / Set default. Esc to cancel.\n"
                    .into(),
            ));
            renderer.flush();
        }
        "status" => {
            let txt = format!(
                "  Model:  {}\n  Dir:    {}\n  Config: {}\n  Tokens: {}\n",
                ctx.model_name,
                ctx.working_dir.display(),
                Config::default_path().display(),
                state.total_tokens,
            );
            renderer.render(UiLine::CommandOutput(txt));
            renderer.flush();
        }
        "diff" => {
            let out = std::process::Command::new("git")
                .args(["diff", "--stat"])
                .current_dir(&ctx.working_dir)
                .output();
            match out {
                Ok(o) => {
                    let s = String::from_utf8_lossy(&o.stdout).to_string();
                    renderer.render(UiLine::CommandOutput(if s.is_empty() {
                        "  (no changes)\n".into()
                    } else {
                        s
                    }));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(format!("git diff failed: {}", e)));
                }
            }
            renderer.flush();
        }
        "undo" => {
            renderer.render(UiLine::CommandOutput("  Undo is not yet supported.\n".into()));
            renderer.flush();
        }
        "cost" => {
            renderer.render(UiLine::CommandOutput(format!(
                "  Session tokens: {}\n",
                state.total_tokens
            )));
            renderer.flush();
        }
        "context" => {
            // `/context` = breakdown only.
            // `/context prompt` = breakdown + full assembled system prompt
            // (the exact bytes the most recent turn sent). Useful when
            // the model is misbehaving and you want to verify what's
            // actually in the prompt.
            let show_prompt = arg.trim().eq_ignore_ascii_case("prompt");
            renderer.render(UiLine::CommandOutput(
                render_context_report(state, ctx, show_prompt),
            ));
            renderer.flush();
        }
        "login" => {
            run_login_flow(renderer, ctx)?;
        }
        "logout" => {
            match atomcode_core::auth::logout() {
                Ok(()) => {
                    renderer.render(UiLine::CommandOutput(
                        "  Signed out of AtomGit.\n".into(),
                    ));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(format!("logout failed: {}", e)));
                }
            }
            renderer.flush();
        }
        "whoami" => {
            let txt = if let Some(auth) = atomcode_core::auth::get_stored_auth() {
                let email = auth.user.email.as_deref().unwrap_or("—");
                let name = auth.user.name.as_deref().unwrap_or(&auth.user.username);
                format!(
                    "  {} ({})\n  {}\n  auth: {}\n",
                    name,
                    auth.user.username,
                    email,
                    atomcode_core::auth::auth_file_path().display(),
                )
            } else {
                "  Not signed in. Use /login to authenticate.\n".into()
            };
            renderer.render(UiLine::CommandOutput(txt));
            renderer.flush();
        }
        "upgrade" => {
            // Sub-dispatch: `/upgrade`, `/upgrade rollback`, `/upgrade --force`.
            // Keep parsing deliberately tolerant — users type these things
            // with assorted capitalization and whitespace; a command that
            // refuses `/upgrade Rollback` is user-hostile.
            let arg_norm = arg.trim().to_ascii_lowercase();
            if arg_norm == "rollback" {
                // Rollback is sync and fast (three renames). Run inline
                // so the user sees the result immediately without waiting
                // for an async task to schedule.
                match atomcode_core::self_update::run_rollback() {
                    Ok(sum) => {
                        // Route through the event channel so rendering
                        // and "set done → exit" logic stays in one place.
                        let _ = ctx.upgrade_tx.send(
                            atomcode_core::self_update::UpgradeEvent::RolledBack {
                                exe: sum.exe,
                                backup: sum.backup,
                            },
                        );
                    }
                    Err(e) => {
                        let _ = ctx.upgrade_tx.send(
                            atomcode_core::self_update::UpgradeEvent::Failed(format!("{:#}", e)),
                        );
                    }
                }
            } else {
                let force = arg_norm == "--force" || arg_norm == "-f";
                if !force && !arg_norm.is_empty() {
                    renderer.render(UiLine::Error(format!(
                        "unknown /upgrade argument: {}\n  usage: /upgrade [rollback|--force]",
                        arg
                    )));
                    renderer.flush();
                    return Ok(());
                }
                renderer.render(UiLine::CommandOutput(
                    "  正在检查更新...\n".into(),
                ));
                renderer.flush();
                let current = format!("v{}", env!("CARGO_PKG_VERSION"));
                let tx = ctx.upgrade_tx.clone();
                tokio::spawn(async move {
                    // The driver emits Done via `tx` on success; on error
                    // we translate to a Failed event so the TUI layer
                    // only has to handle one event stream.
                    if let Err(e) =
                        atomcode_core::self_update::run_upgrade(current, force, tx.clone()).await
                    {
                        let _ = tx.send(atomcode_core::self_update::UpgradeEvent::Failed(
                            format!("{:#}", e),
                        ));
                    }
                });
            }
        }
        "fixissue" => {
            // `/fixissue <url>` — fetch the issue via AtomGit API (blocking,
            // ~1s), verify the current user is the assignee, then inject a
            // synthesised prompt into the agent as if the user typed it.
            // Not-assigned / fetch-fail paths print the reason and stay Idle.
            let url = arg.trim();
            if url.is_empty() {
                renderer.render(UiLine::CommandOutput(
                    "  Usage: /fixissue <issue-url>\n  Example: /fixissue https://atomgit.com/owner/repo/issues/42\n  Or use the interactive wizard: /issue\n".into(),
                ));
                renderer.flush();
            } else {
                launch_fixissue(url, state, ctx, renderer, fixissue_pending, fixissue_buffer);
            }
        }
        "issue" => {
            // Interactive wizard — prompts for URL, then routes into the
            // same `launch_fixissue` helper as `/fixissue <url>`. See
            // `IssueWizard::emit_prompt` for the prompt line; the wizard
            // stashes the URL in `ctx.pending_issue_url` on Enter and
            // the event loop picks it up after modal close.
            //
            // If the user passes a URL inline (`/issue <url>`) we
            // shortcut the wizard and run the pipeline directly — nice
            // for power users who typed the URL into history already.
            let url = arg.trim();
            if !url.is_empty() {
                launch_fixissue(url, state, ctx, renderer, fixissue_pending, fixissue_buffer);
            } else {
                let mut wiz = IssueWizard::open();
                wiz.emit_prompt(renderer);
                *active_modal = Some(Box::new(wiz));
            }
        }
        "cd" => {
            // Bare `/cd` — open the interactive history picker (matches legacy
            // TUI behaviour). The picker's Enter-handler invokes `apply_cd`
            // itself, so there's nothing else to do here.
            if arg.is_empty() {
                if ctx.recent_dirs.is_empty() {
                    renderer.render(UiLine::CommandOutput(format!(
                        "  Working directory: {}\n  No recent projects. Use `/cd <path>` to switch.\n",
                        ctx.working_dir.display()
                    )));
                    renderer.flush();
                } else {
                    *active_modal = Some(Box::new(DirPicker::open(
                        ctx.recent_dirs.clone(),
                        ctx.working_dir.clone(),
                    )));
                }
                return Ok(());
            }
            let new_dir = resolve_cd(arg, &ctx.working_dir, ctx.previous_dir.as_deref());
            match new_dir {
                Ok(path) => {
                    apply_cd(ctx, path.clone());
                    renderer.render(UiLine::CommandOutput(format!(
                        "  Changed to: {}\n",
                        path.display()
                    )));
                }
                Err(e) => {
                    renderer.render(UiLine::Error(e));
                }
            }
            renderer.flush();
        }
        other => {
            renderer.render(UiLine::Error(format!("Unknown command: /{}", other)));
            renderer.flush();
        }
    }
    Ok(())
}

/// Build the `/context` report — horizontal bar + category breakdown,
/// optionally followed by the full system prompt when `show_prompt`.
///
/// Thin wrapper around `format_context_report` that pulls the inputs
/// (snapshot + model name + flag) out of state/ctx. Split for
/// unit-testability: the inner function takes plain values and can be
/// asserted on directly.
fn render_context_report(state: &UiState, ctx: &LoopCtx, show_prompt: bool) -> String {
    format_context_report(state.last_context.as_ref(), &ctx.model_name, show_prompt)
}

/// Pure-function core of `/context` — testable without constructing
/// `LoopCtx`. Returns the rendered CommandOutput body.
fn format_context_report(
    snapshot: Option<&crate::state::ContextSnapshot>,
    model_name: &str,
    show_prompt: bool,
) -> String {
    let Some(snap) = snapshot else {
        return "  Context Usage\n  \n  (run at least one turn first — stats are captured per turn)\n".into();
    };
    if snap.ctx_window == 0 {
        return "  Context Usage\n  \n  (waiting for first complete turn — partial stats only)\n".into();
    }

    let window = snap.ctx_window;
    // Sum components excluding tool_defs (which in most providers counts
    // against input tokens but atomcode tracks separately). Clamp used to
    // window so a single oversized tool_defs doesn't drive "free" negative.
    let sys = snap.system_tokens;
    let tools = snap.tool_defs_tokens;
    let cold = snap.cold_zone_tokens;
    // Sent = everything sent minus the system message (ctx's own accounting).
    // Cold zone is injected as a System message inside `sent`, so we avoid
    // double-counting: subtract cold from sent for the "messages" bucket.
    let messages = snap.sent_tokens.saturating_sub(cold);
    let total_used = sys.saturating_add(tools).saturating_add(cold).saturating_add(messages);
    let free = window.saturating_sub(total_used);

    // Horizontal bar: 40 cells, one segment per category with a distinct glyph.
    // Terminals universally render these blocks, no ANSI color required.
    const BAR_WIDTH: usize = 40;
    let cells = |tokens: usize| -> usize {
        if window == 0 { return 0; }
        (tokens as u128 * BAR_WIDTH as u128 / window as u128) as usize
    };
    let sys_cells = cells(sys);
    let tools_cells = cells(tools);
    let cold_cells = cells(cold);
    let msg_cells = cells(messages);
    // Guard: cell sum shouldn't exceed BAR_WIDTH (rounding can give +1).
    let used_cells = sys_cells + tools_cells + cold_cells + msg_cells;
    let free_cells = BAR_WIDTH.saturating_sub(used_cells.min(BAR_WIDTH));

    let mut bar = String::with_capacity(BAR_WIDTH * 3);
    bar.push_str(&"▒".repeat(sys_cells));       // system prompt
    bar.push_str(&"▓".repeat(tools_cells));     // tool defs
    bar.push_str(&"░".repeat(cold_cells));      // cold zone
    bar.push_str(&"█".repeat(msg_cells));       // messages
    bar.push_str(&"·".repeat(free_cells));      // free

    let pct = |t: usize| -> String {
        if window == 0 { return "  —".to_string(); }
        format!("{:>4.1}%", (t as f64 * 100.0) / window as f64)
    };
    let k = |t: usize| -> String {
        if t >= 1000 {
            format!("{:.1}K", t as f64 / 1000.0)
        } else {
            format!("{}", t)
        }
    };

    let used_pct = pct(total_used);

    let mut out = format!(
        "  Context Usage\n  \
         \n  \
         {bar}\n  \
         {used}/{window} tokens ({used_pct})\n  \
         \n  \
         Provider: {model}  ·  ctx: {ctx_name}\n  \
         \n  \
         ▒ System prompt : {sys_s:>7}  ({sys_p})\n  \
         ▓ Tool defs     : {tools_s:>7}  ({tools_p})\n  \
         ░ Cold zone     : {cold_s:>7}  ({cold_p})\n  \
         █ Messages      : {msgs_s:>7}  ({msgs_p})\n  \
         · Free          : {free_s:>7}  ({free_p})\n  \
         \n  \
         Messages in window: {n_msgs}\n",
        bar = bar,
        used = k(total_used),
        window = k(window),
        used_pct = used_pct,
        model = model_name,
        ctx_name = if snap.ctx_name.is_empty() { "default" } else { snap.ctx_name.as_str() },
        sys_s = k(sys), sys_p = pct(sys),
        tools_s = k(tools), tools_p = pct(tools),
        cold_s = k(cold), cold_p = pct(cold),
        msgs_s = k(messages), msgs_p = pct(messages),
        free_s = k(free), free_p = pct(free),
        n_msgs = snap.total_messages,
    );

    // `/context prompt` — append the full system-prompt bytes the last
    // turn sent. Kept out of the default output because the prompt is
    // 5–15 KB and would swamp the breakdown dashboard every invocation.
    // Hint line added when empty so the user knows WHY nothing showed
    // (snapshot is populated only by the rich emission path, which
    // fires once the first complete turn lands).
    if show_prompt {
        out.push('\n');
        out.push_str("  === SYSTEM PROMPT ===\n");
        if snap.system_prompt.is_empty() {
            out.push_str("  (empty — wait for one complete turn to capture)\n");
        } else {
            // Indent each line with two spaces to match the surrounding
            // CommandOutput formatting (every other block uses a 2-space
            // left gutter). Avoids the model-prompt bytes looking like
            // they're escaping the command-output indentation.
            for line in snap.system_prompt.lines() {
                out.push_str("  ");
                out.push_str(line);
                out.push('\n');
            }
        }
    }

    out
}

/// Prepare + dispatch the fixissue pipeline for a given URL. Shared by:
/// (a) the `/fixissue <url>` arm, (b) the `/issue <url>` arm, and (c)
/// the event loop's post-close hook when `IssueWizard` has stashed a
/// URL in `ctx.pending_issue_url`. Handles all three `Prepared` cases
/// (Run / Skip / Err) and prints appropriate scrollback feedback. On
/// Run it arms the post-completion hook (`fixissue_pending` +
/// `fixissue_buffer`), sends `AgentCommand::SendMessage`, and flips
/// UiState to Streaming via `state.on_submit()`.
pub(crate) fn launch_fixissue(
    url: &str,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    fixissue_pending: &mut Option<atomcode_core::atomgit::IssueRef>,
    fixissue_buffer: &mut String,
) {
    match atomcode_core::atomgit::fixissue::prepare(url, &ctx.working_dir) {
        Ok(atomcode_core::atomgit::fixissue::Prepared::Run {
            prompt,
            issue_title,
            issue_number,
            issue_ref,
        }) => {
            renderer.render(UiLine::CommandOutput(format!(
                "  [fixissue] issue #{}: {}\n  Handing off to agent... (will post summary + 'fixed' label on completion)\n",
                issue_number, issue_title,
            )));
            renderer.flush();
            *fixissue_pending = Some(issue_ref);
            fixissue_buffer.clear();
            ctx.agent
                .cmd_tx
                .send(AgentCommand::SendMessage(prompt))
                .ok();
            state.on_submit();
        }
        Ok(atomcode_core::atomgit::fixissue::Prepared::Skip { reason }) => {
            renderer.render(UiLine::CommandOutput(format!("  {}\n", reason)));
            renderer.flush();
        }
        Err(e) => {
            renderer.render(UiLine::CommandOutput(format!(
                "  fixissue failed: {:#}\n",
                e
            )));
            renderer.flush();
        }
    }
}

/// Commit a new working-directory choice: notify the agent, update cwd +
/// previous_dir on the shared context, push the new entry into the
/// recent-dirs ring, and persist. Shared by the `/cd <path>` arm and the
/// DirPicker modal's Enter handler so both paths keep state coherent.
pub(crate) fn apply_cd(ctx: &mut LoopCtx, path: PathBuf) {
    ctx.agent
        .cmd_tx
        .send(AgentCommand::ChangeDir(path.to_string_lossy().to_string()))
        .ok();
    ctx.previous_dir = Some(std::mem::replace(&mut ctx.working_dir, path.clone()));
    push_recent_dir(&mut ctx.recent_dirs, path);
    save_recent_dirs(&ctx.recent_dirs);
}

/// Move `new` to the front of `dirs`, dedup, and cap at `MAX_RECENT_DIRS`.
/// Does NOT persist — call `save_recent_dirs` after, or use `apply_cd`
/// which does both.
pub(crate) fn push_recent_dir(dirs: &mut Vec<PathBuf>, new: PathBuf) {
    dirs.retain(|d| d != &new);
    dirs.insert(0, new);
    dirs.truncate(MAX_RECENT_DIRS);
}

/// Read `~/.atomcode/recent_dirs.txt`. Silently drops missing directories
/// so stale entries from a deleted project don't linger in the picker.
pub(crate) fn load_recent_dirs() -> Vec<PathBuf> {
    let path = atomcode_core::config::Config::config_dir().join("recent_dirs.txt");
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .take(MAX_RECENT_DIRS)
                .collect()
        })
        .unwrap_or_default()
}

/// Persist `dirs` to `~/.atomcode/recent_dirs.txt`. Best-effort — a write
/// failure (read-only HOME, permission denied) is swallowed so it can
/// never break an interactive `/cd`.
pub(crate) fn save_recent_dirs(dirs: &[PathBuf]) {
    let path = atomcode_core::config::Config::config_dir().join("recent_dirs.txt");
    let content = dirs
        .iter()
        .map(|d| d.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(&path, content);
}

fn resolve_cd(
    arg: &str,
    cwd: &std::path::Path,
    prev: Option<&std::path::Path>,
) -> std::result::Result<PathBuf, String> {
    let home = crate::platform::home_dir();
    let target = if arg.is_empty() {
        home.ok_or_else(|| "home directory not known".to_string())?
    } else if arg == "-" {
        prev.map(|p| p.to_path_buf())
            .ok_or_else(|| "No previous directory".to_string())?
    } else if let Some(rest) = arg.strip_prefix('~') {
        let home = home.ok_or_else(|| "home directory not known".to_string())?;
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        if rest.is_empty() {
            home
        } else {
            home.join(rest)
        }
    } else {
        let p = PathBuf::from(arg);
        if p.is_absolute() {
            p
        } else {
            cwd.join(p)
        }
    };
    let canon = target
        .canonicalize()
        .map_err(|e| format!("{}: {}", target.display(), e))?;
    if !canon.is_dir() {
        return Err(format!("Not a directory: {}", canon.display()));
    }
    Ok(canon)
}

/// Build the AtomGit OAuth ProviderConfig. api_key is intentionally None —
/// it's loaded from auth.toml at runtime by `create_provider()`.
fn build_oauth_provider() -> ProviderConfig {
    ProviderConfig {
        provider_type: "openai".to_string(),
        api_key: None,
        model: "MiniMax-M2.7".to_string(),
        base_url: Some("https://api-ai.gitcode.com/v1".to_string()),
        system_prompt: None,
        user_agent: None,
        context_window: 64000,
        max_tokens: None,
        ephemeral: false,
    }
}

/// Drop out of raw mode, run the (blocking) OAuth login flow so the user
/// can interact with the browser callback in a normal terminal, then
/// re-enter raw mode and redraw the welcome screen. OAuth uses stdout
/// prints + opens a browser — mixing that with our footer-managing
/// raw-mode renderer would collide on stdin/stdout, so we suspend.
fn run_login_flow(renderer: &mut dyn Renderer, ctx: &mut LoopCtx) -> Result<()> {
    // Pause the reader thread BEFORE disabling raw mode so it stops
    // calling `event::poll` / `event::read`. Without this, the reader
    // would keep consuming bytes from stdin in cooked mode (keystrokes
    // the user made after the browser handoff, stray bytes from the
    // callback handshake, FocusGained/Lost) and those events would
    // either starve the OAuth child or land in `input_rx` as stale
    // events that eat the first real keystroke after login.
    //
    // `pause_blocking` waits for ack so the reader is guaranteed idle
    // before `suspend_for_external` flips raw mode.
    if let Some(reader) = ctx.reader.as_ref() {
        let _ = reader.pause_blocking();
    }

    // Suspend: disables bracketed paste (otherwise the callback URL
    // paste would arrive wrapped in `\x1b[200~ ... \x1b[201~` and
    // corrupt the CSRF state parameter) and raw mode, then flushes.
    // The OAuth flow owns the terminal until it returns.
    renderer.suspend_for_external();

    let result = atomcode_core::auth::login()
        .and_then(|auth| atomcode_core::auth::save_auth(&auth).map(|()| auth));

    // Resume: re-enable raw + bracketed-paste AND reset cached state
    // (the cooked-mode child wrote to stdout, so our cursor tracking
    // is lying — next render must anchor against a fresh screen).
    renderer.resume_from_external();

    // Unpause the reader AFTER raw mode is back on. The reader skipped
    // the entire OAuth window so `input_rx` has no stale events to
    // drain — the first keystroke the user presses after login lands
    // cleanly.
    if let Some(reader) = ctx.reader.as_ref() {
        reader.resume();
    }

    match result {
        Ok(auth) => {
            // Register the AtomGit OAuth provider and switch to it so the
            // freshly logged-in token is actually used. Without this the
            // status bar / next turn would keep using whatever provider was
            // active before login.
            let provider = build_oauth_provider();
            let model = provider.model.clone();
            ctx.config
                .providers
                .insert(OAUTH_PROVIDER_NAME.to_string(), provider);
            ctx.config.default_provider = OAUTH_PROVIDER_NAME.to_string();
            ctx.model_name = model.clone();
            save_and_reload(ctx, renderer);

            let name = auth
                .user
                .name
                .as_deref()
                .unwrap_or(&auth.user.username)
                .to_string();
            renderer.render(UiLine::CommandOutput(format!(
                "  Signed in as {} ({}). Model switched to {}.\n",
                name, auth.user.username, model
            )));
            renderer.flush();
        }
        Err(e) => {
            renderer.render(UiLine::Error(format!("login failed: {}", e)));
            renderer.flush();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a subdir inside a tempdir and return both. Paths are
    /// canonicalized because `resolve_cd` canonicalizes its output, and
    /// on macOS `/var/folders/...` → `/private/var/folders/...`.
    fn make_dirs() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cwd = tmp.path().canonicalize().expect("canon cwd");
        let sub = cwd.join("sub");
        std::fs::create_dir(&sub).expect("mkdir sub");
        let sub = sub.canonicalize().expect("canon sub");
        (tmp, cwd, sub)
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        let (_tmp, cwd, sub) = make_dirs();
        let got = resolve_cd("sub", &cwd, None).expect("relative resolves");
        assert_eq!(got, sub);
    }

    #[test]
    fn absolute_path_ignores_cwd() {
        let (_tmp, _cwd, sub) = make_dirs();
        let alt_cwd = PathBuf::from("/"); // unrelated cwd
        let got = resolve_cd(sub.to_str().unwrap(), &alt_cwd, None)
            .expect("absolute resolves");
        assert_eq!(got, sub);
    }

    #[test]
    fn dash_uses_previous_dir() {
        let (_tmp, cwd, sub) = make_dirs();
        let got = resolve_cd("-", &sub, Some(&cwd)).expect("dash uses prev");
        assert_eq!(got, cwd);
    }

    #[test]
    fn dash_without_previous_errors() {
        let (_tmp, cwd, _sub) = make_dirs();
        let err = resolve_cd("-", &cwd, None).expect_err("dash w/o prev");
        assert!(err.contains("No previous directory"), "got: {}", err);
    }

    #[test]
    fn nonexistent_path_errors() {
        let (_tmp, cwd, _sub) = make_dirs();
        let err = resolve_cd("nope-does-not-exist", &cwd, None)
            .expect_err("nonexistent errors");
        assert!(err.contains("nope-does-not-exist"), "got: {}", err);
    }

    #[test]
    fn file_path_rejected_with_not_a_directory() {
        let (_tmp, cwd, _sub) = make_dirs();
        let file = cwd.join("a.txt");
        std::fs::write(&file, "hi").expect("write");
        let err = resolve_cd(file.to_str().unwrap(), &cwd, None)
            .expect_err("file is not a dir");
        assert!(err.contains("Not a directory"), "got: {}", err);
    }

    #[test]
    fn tilde_expands_to_home() {
        // Only run when HOME is actually resolvable; skip quietly on
        // hosts where it isn't (some CI sandboxes).
        let Some(home) = crate::platform::home_dir() else {
            return;
        };
        let Ok(canon_home) = home.canonicalize() else {
            return;
        };
        let (_tmp, cwd, _sub) = make_dirs();
        let got = resolve_cd("~", &cwd, None).expect("~ resolves");
        assert_eq!(got, canon_home);
    }

    #[test]
    fn context_report_without_snapshot_prompts_to_run_turn() {
        let out = format_context_report(None, "claude-opus-4-7", false);
        assert!(out.contains("run at least one turn"));
        // Never leak a window/totals when there's nothing to show
        assert!(!out.contains("tokens ("));
    }

    #[test]
    fn context_report_with_zero_window_flags_partial_stats() {
        let snap = crate::state::ContextSnapshot {
            system_tokens: 100,
            sent_tokens: 200,
            tool_defs_tokens: 0,
            cold_zone_tokens: 0,
            total_messages: 5,
            ctx_window: 0,
            ctx_name: String::new(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "test-model", false);
        assert!(out.contains("waiting for first complete turn"));
    }

    #[test]
    fn context_report_renders_full_breakdown() {
        let snap = crate::state::ContextSnapshot {
            system_tokens: 8_000,
            sent_tokens: 30_000,  // includes cold
            tool_defs_tokens: 14_500,
            cold_zone_tokens: 2_000,
            total_messages: 42,
            ctx_window: 128_000,
            ctx_name: "default".into(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "claude-opus-4-7", false);

        // Header
        assert!(out.contains("Context Usage"));
        // Bar renders (unicode blocks present)
        assert!(out.contains("▒") || out.contains("█"));
        // Category labels
        assert!(out.contains("System prompt"));
        assert!(out.contains("Tool defs"));
        assert!(out.contains("Cold zone"));
        assert!(out.contains("Messages"));
        assert!(out.contains("Free"));
        // Token values (K formatting)
        assert!(out.contains("8.0K"));   // system
        assert!(out.contains("14.5K"));  // tool defs
        assert!(out.contains("2.0K"));   // cold zone
        assert!(out.contains("128.0K")); // window
        // Messages count
        assert!(out.contains("42"));
        // ctx name + model
        assert!(out.contains("default"));
        assert!(out.contains("claude-opus-4-7"));
    }

    #[test]
    fn context_report_messages_excludes_cold_zone() {
        // sent_tokens = messages + cold_zone (cold is injected as a
        // System message inside `sent`). Renderer must subtract so
        // "Messages" doesn't double-count.
        let snap = crate::state::ContextSnapshot {
            system_tokens: 1_000,
            sent_tokens: 10_000,
            tool_defs_tokens: 0,
            cold_zone_tokens: 3_000,
            total_messages: 10,
            ctx_window: 100_000,
            ctx_name: "default".into(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "m", false);
        // Messages bucket should be 10K - 3K = 7K, not 10K.
        let messages_line = out.lines().find(|l| l.contains("Messages"))
            .expect("messages line must exist");
        assert!(messages_line.contains("7.0K"),
            "expected Messages=7.0K (sent-cold), got line: {}", messages_line);
    }

    #[test]
    fn context_report_free_is_nonneg_under_rounding() {
        // Pathological: sum of components exactly = window. Free must
        // render as 0, never blow up the subtraction.
        let snap = crate::state::ContextSnapshot {
            system_tokens: 20_000,
            sent_tokens: 80_000,
            tool_defs_tokens: 20_000,
            cold_zone_tokens: 0,
            total_messages: 50,
            ctx_window: 120_000,
            ctx_name: "default".into(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "m", false);
        // Free = window - (sys + tools + cold + messages)
        //      = 120_000 - (20_000 + 20_000 + 0 + 80_000) = 0
        assert!(out.contains("Free"));
        // Should not panic and should render — look for "0" tokens on the Free line
        let free_line = out.lines().find(|l| l.contains("Free"))
            .expect("free line must exist");
        assert!(free_line.contains("0"), "free line: {}", free_line);
    }

    #[test]
    fn context_report_without_show_prompt_omits_system_prompt_section() {
        // Default `/context` output must not include the prompt dump
        // even when the snapshot HAS a cached prompt. Otherwise the
        // breakdown dashboard gets buried under 5-15K chars every call.
        let snap = crate::state::ContextSnapshot {
            system_tokens: 1_000,
            sent_tokens: 5_000,
            tool_defs_tokens: 500,
            cold_zone_tokens: 0,
            total_messages: 8,
            ctx_window: 100_000,
            ctx_name: "default".into(),
            system_prompt: "You are AtomCode.\nSOME SENTINEL BYTES".into(),
        };
        let out = format_context_report(Some(&snap), "m", false);
        assert!(!out.contains("SYSTEM PROMPT"),
            "SYSTEM PROMPT header must not appear in default /context output");
        assert!(!out.contains("SOME SENTINEL BYTES"),
            "raw prompt body must not leak into default /context output");
    }

    #[test]
    fn context_report_with_show_prompt_appends_cached_prompt() {
        let snap = crate::state::ContextSnapshot {
            system_tokens: 1_000,
            sent_tokens: 5_000,
            tool_defs_tokens: 500,
            cold_zone_tokens: 0,
            total_messages: 8,
            ctx_window: 100_000,
            ctx_name: "default".into(),
            system_prompt: "You are AtomCode.\nRULE_LINE_ABC\nEND".into(),
        };
        let out = format_context_report(Some(&snap), "m", true);
        assert!(out.contains("=== SYSTEM PROMPT ==="));
        // Each line indented with leading 2 spaces — verify one line
        // survives through the gutter indentation.
        assert!(out.contains("  RULE_LINE_ABC"),
            "prompt lines should keep content after 2-space indent");
        // Breakdown still present (append, not replace)
        assert!(out.contains("Context Usage"));
        assert!(out.contains("System prompt"));
    }

    #[test]
    fn context_report_show_prompt_with_empty_cached_prompt_shows_hint() {
        // Partial snapshot: no turn has landed rich stats yet, so
        // system_prompt is "". `/context prompt` should tell the user
        // that — not just silently show an empty section.
        let snap = crate::state::ContextSnapshot {
            system_tokens: 100,
            sent_tokens: 200,
            tool_defs_tokens: 0,
            cold_zone_tokens: 0,
            total_messages: 3,
            ctx_window: 100_000,
            ctx_name: "default".into(),
            system_prompt: String::new(),
        };
        let out = format_context_report(Some(&snap), "m", true);
        assert!(out.contains("=== SYSTEM PROMPT ==="));
        assert!(out.contains("(empty"),
            "empty cached prompt must show an explanation, got: {}", out);
    }

    #[test]
    fn build_oauth_provider_has_expected_defaults() {
        // Guardrail against accidental edits to the OAuth-provider
        // seed values (api_key must be None; base_url must point to
        // the AtomGit gateway).
        let p = build_oauth_provider();
        assert_eq!(p.provider_type, "openai");
        assert!(p.api_key.is_none(), "api_key must be None — loaded from auth.toml");
        assert_eq!(p.base_url.as_deref(), Some("https://api-ai.gitcode.com/v1"));
        assert!(p.context_window > 0);
    }
}
