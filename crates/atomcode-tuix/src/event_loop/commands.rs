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
use crate::modals::{Modal, ModelPicker, ProviderWizard, SessionPicker};
use crate::render::{Renderer, UiLine};
use crate::state::UiState;

/// Provider name used for the AtomGit OAuth provider entry in config.
const OAUTH_PROVIDER_NAME: &str = "AtomGit";

pub(super) fn execute_slash_command(
    cmd: &str,
    arg: &str,
    state: &mut UiState,
    ctx: &mut LoopCtx,
    renderer: &mut dyn Renderer,
    active_modal: &mut Option<Box<dyn Modal>>,
) -> Result<()> {
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
        "cd" => {
            let new_dir = resolve_cd(arg, &ctx.working_dir, ctx.previous_dir.as_deref());
            match new_dir {
                Ok(path) => {
                    ctx.previous_dir = Some(ctx.working_dir.clone());
                    ctx.working_dir = path.clone();
                    ctx.agent
                        .cmd_tx
                        .send(AgentCommand::ChangeDir(path.to_string_lossy().to_string()))
                        .ok();
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

            let dir_display = crate::platform::collapse_home(&ctx.working_dir.to_string_lossy());
            renderer.render(UiLine::Welcome {
                model: ctx.model_name.clone(),
                working_dir: dir_display,
            });
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
