// Swap in mimalloc on Windows — the default HeapAlloc is the biggest single
// contributor to per-keystroke render latency (hundreds of small Line/Span
// clones per frame). No-op on macOS/Linux where the system allocator is fine.
#[cfg(target_os = "windows")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

mod schedule_cmd;
mod schedule_os;
mod telemetry_cmd;
mod vision;
use atomcode::uninstall;

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary
// runs, so unit tests never persist into the developer's real `~/.atomcode`.
// Tests that set their own ATOMCODE_HOME still win (isolate_home is a no-op when
// the var is already set).
#[cfg(test)]
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

use atomcode_capabilities::mcp::{
    load_mcp_config, login_mcp_oauth, merge_http_oauth_mcp_server_into_json_file,
    merge_stdio_mcp_server_into_json_file, McpHttpAuthConfig, McpOAuthLoginOptions, McpTokenStore,
    McpTransportConfig,
};
use atomcode_config::config::Config;

use atomcode_auth as auth;
use atomcode_telemetry::{
    config::{resolve, ProcessEnv},
    event::SessionMode,
    notice, CliOverride, CurrentContext, Event, Telemetry,
};

/// Set to `true` at the start of `run_headless` so the panic hook and the
/// top-level error handler can skip TUI cleanup. In headless mode raw mode
/// was never enabled, so calling `disable_raw_mode` would be a wasted ioctl
/// and on Windows can panic if the console handle isn't a real TTY.
static HEADLESS_MODE: AtomicBool = AtomicBool::new(false);

/// Set once the startup synchronous upgrade check has run this launch, so the
/// post-parse detached stager can skip a redundant second `latest.json` fetch —
/// the sync path already checked (and applied, if newer) on this same launch.
static SYNC_UPGRADE_CHECKED: AtomicBool = AtomicBool::new(false);

/// Restore terminal state if (and only if) we ever entered TUI mode.
/// No-op in headless mode — see [`HEADLESS_MODE`].
///
/// TUI mode (v4.23.2+) runs entirely in the primary screen via the
/// append-only RetainedRenderer — we never emit `\x1b[?1049h`, so there
/// is no `LeaveAlternateScreen` counterpart to issue here.
///
/// On the GRACEFUL path mouse mode, cursor visibility, autowrap, DECSTBM
/// and the Kitty keyboard protocol are restored by `RetainedRenderer` /
/// `TerminalGuard` Drops. But the release profile sets `panic = "abort"`,
/// so on a crash NO destructor unwinds and none of those Drops run —
/// this hook is the only cleanup that executes. Disabling raw mode alone
/// left the Kitty protocol armed, so the parent shell echoed every
/// post-crash keypress as a literal `[27u` / `[99;5u` CSI-u report. We
/// therefore emit the full panic-safe restore sequence (idempotent on
/// the graceful path) before dropping raw mode.
fn notify_stop_reason(
    reason: atomcode_kernel::event::StopReason,
) -> atomcode_capabilities::notify::NotifyStopReason {
    use atomcode_capabilities::notify::NotifyStopReason as N;
    use atomcode_kernel::event::StopReason as T;
    match reason {
        T::Stopped => N::Natural,
        T::Cancelled => N::Cancelled,
        T::MaxRounds | T::MaxContinuations => N::TurnLimit,
        T::RepeatLoop | T::ToolLoopDetected => N::StepLimit,
        T::ProviderError | T::Timeout | T::PromptRejected | T::RateLimited => N::Error,
        _ => N::Error,
    }
}

fn headless_completion_exit_code(
    completion: &atomcode_coding::TurnCompletion,
    current: i32,
) -> i32 {
    match completion {
        // Snapshot failure is a failure of the completion contract itself;
        // the kernel reason inside it cannot turn the variant into success.
        atomcode_coding::TurnCompletion::SnapshotUnavailable { .. } => current.max(1),
        atomcode_coding::TurnCompletion::Completed { reason, .. } => match reason {
            atomcode_kernel::event::StopReason::Cancelled => 130,
            atomcode_kernel::event::StopReason::Stopped
            | atomcode_kernel::event::StopReason::RateLimited => current,
            _ => current.max(1),
        },
    }
}

fn headless_completion_notify_reason(
    completion: &atomcode_coding::TurnCompletion,
) -> atomcode_capabilities::notify::NotifyStopReason {
    match completion {
        atomcode_coding::TurnCompletion::Completed { reason, .. } => notify_stop_reason(*reason),
        atomcode_coding::TurnCompletion::SnapshotUnavailable { .. } => {
            atomcode_capabilities::notify::NotifyStopReason::Error
        }
    }
}

fn restore_terminal_if_tui() {
    if HEADLESS_MODE.load(Ordering::Relaxed) {
        return;
    }
    atomcode_tuix::panic_restore_terminal();
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Resolve the working directory at startup. **Always** uses the current
/// working directory unless the user explicitly passed `-C / --dir`.
///
/// We deliberately do **not** read `~/.atomcode/recent_dirs.txt` (or any other
/// "remembered" path). The previous implementation silently substituted the
/// first entry of recent_dirs for the user's cwd, which made commands like
/// `atomcode -p "describe this project"` operate on whatever directory the
/// TUI happened to visit last — a violation of least surprise. recent_dirs
/// remains a TUI picker convenience only; it must never override cwd.
fn resolve_working_dir(cli_dir: Option<PathBuf>) -> PathBuf {
    if let Some(d) = cli_dir {
        std::fs::canonicalize(&d).unwrap_or(d)
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// Truncate a string to at most `max_chars` *characters* (not bytes), replacing
/// any newlines with spaces and appending "..." when truncated.
///
/// Used for headless-mode log lines on stderr. **Counts characters, not bytes**,
/// so multi-byte UTF-8 (e.g. CJK) is safe — `&s[..N]` would panic when N falls
/// inside a multi-byte char.
fn truncate_log_line(s: &str, max_chars: usize) -> String {
    let single_line: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if single_line.chars().count() > max_chars {
        let head: String = single_line.chars().take(max_chars).collect();
        format!("{}...", head)
    } else {
        single_line
    }
}

/// Append a streaming reasoning/thinking `chunk` to `out`, maintaining a
/// single-line `[thinking] ...` representation across many tiny deltas.
///
/// `open` tracks whether a `[thinking]` line is currently open (i.e. has a
/// prefix written but no trailing newline). The first chunk gets a fresh
/// `[thinking] ` prefix; subsequent chunks append directly. Embedded newlines
/// inside a chunk are preserved, with each non-empty new line getting its own
/// `[thinking] ` prefix so multi-line thinking stays readable.
///
/// Pulled out of `run_headless` so it can be unit-tested without spinning up
/// the agent loop. Regression target: the old per-chunk `eprintln!` produced
/// "one word per line" output for streaming reasoning models.
fn format_thinking_chunk(out: &mut String, open: &mut bool, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    if !*open {
        out.push_str("[thinking] ");
        *open = true;
    }
    let mut parts = chunk.split('\n');
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    for part in parts {
        out.push('\n');
        *open = false;
        if !part.is_empty() {
            out.push_str("[thinking] ");
            out.push_str(part);
            *open = true;
        }
    }
}

fn format_verbose_tool_chunk(chunk: &str) -> std::borrow::Cow<'_, str> {
    match chunk.strip_prefix('\u{1e}') {
        Some(progress) => std::borrow::Cow::Owned(format!("[progress] {}\n", progress.trim_end())),
        None => std::borrow::Cow::Borrowed(chunk),
    }
}

/// Close any in-flight `[thinking]` line by writing a newline if one is open.
/// Mirrors the inline `close_thinking_line` used inside `run_headless`, but
/// writes to a buffer so it can be unit-tested.
fn close_thinking_chunk(out: &mut String, open: &mut bool) {
    if *open {
        out.push('\n');
        *open = false;
    }
}

/// True if `--dev` is present in argv. Used to skip every auto-update
/// path (pre-parse `apply_pending_upgrade` and the post-parse detached
/// stager). Scanned manually because one of those paths runs before clap
/// touches argv. The flag is also declared on
/// `Cli` so `clap::Parser` accepts it without erroring after the early
/// scan.
fn is_dev_mode() -> bool {
    std::env::args().skip(1).any(|a| a == "--dev")
}

/// True when the currently-running binary's filename ends in `.bak`.
/// `self_update::replace_binary` renames the previous version to
/// `atomcode.bak` (or `atomcode.exe.bak`) during an upgrade so the user
/// can roll back. Running that backup must NOT auto-upgrade — otherwise
/// rolling back is impossible: any launch of `.bak` would just overwrite
/// itself with the latest version again.
///
/// Defensive: if we can't read `current_exe()` for any reason, assume
/// we're the live binary (not backup) so auto-upgrade still works for
/// the common case.
fn is_running_as_backup() -> bool {
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .map(|n| n.ends_with(".bak"))
        .unwrap_or(false)
}

/// Scan argv by hand to extract the value of --lang <VALUE> or --lang=VALUE.
/// This runs BEFORE clap parses the arguments, so that the i18n locale
/// can be set in time for clap to render localised --help text.
fn scan_argv_for_lang() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1; // skip program name
    while i < args.len() {
        if args[i] == "--lang" && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        if let Some(val) = args[i].strip_prefix("--lang=") {
            return Some(val.to_string());
        }
        i += 1;
    }
    None
}

/// Scan the config file (default path only) for the `language` field.
/// Returns `None` if the config file does not exist, cannot be parsed, or
/// has no `language` key. This is a lightweight pre-parse -- the full config
/// is loaded later in `run()` after clap has parsed CLI flags.
fn scan_config_language() -> Option<atomcode_tuix::i18n::Locale> {
    let path = atomcode_config::config::Config::default_path();
    if !path.exists() {
        return None;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return None,
    };
    let cfg: atomcode_config::config::Config = match toml::from_str(&content) {
        Ok(c) => c,
        Err(_) => return None,
    };
    cfg.language
}

/// Build the top-level clap Command with i18n-localised about and help text.
/// This replaces the default Cli::parse() flow so that --help output
/// respects the current locale (set by scan_argv_for_lang above).
fn build_i18n_command() -> clap::Command {
    use atomcode_tuix::i18n::{t, Msg};

    let cmd = Cli::command();

    // Mutate the top-level about
    let cmd = cmd.about(t(Msg::CliAbout).into_owned());

    // Mutate top-level argument help texts
    let cmd = cmd
        .mut_arg("continue_last", |a| {
            a.help(t(Msg::CliHelpContinue).into_owned())
        })
        .mut_arg("provider", |a| a.help(t(Msg::CliHelpProvider).into_owned()))
        .mut_arg("model", |a| a.help(t(Msg::CliHelpModel).into_owned()))
        .mut_arg("lang", |a| a.help(t(Msg::CliHelpLang).into_owned()))
        .mut_arg("config", |a| a.help(t(Msg::CliHelpConfig).into_owned()))
        .mut_arg("dir", |a| a.help(t(Msg::CliHelpDir).into_owned()))
        .mut_arg("prompt", |a| a.help(t(Msg::CliHelpPrompt).into_owned()))
        .mut_arg("prompt_file", |a| {
            a.help(t(Msg::CliHelpPromptFile).into_owned())
        })
        .mut_arg("verbose", |a| a.help(t(Msg::CliHelpVerbose).into_owned()))
        .mut_arg("dev", |a| a.help(t(Msg::CliHelpDev).into_owned()))
        .mut_arg("no_telemetry", |a| {
            a.help(t(Msg::CliHelpNoTelemetry).into_owned())
        })
        .mut_arg("dangerously_skip_permissions", |a| {
            a.help(t(Msg::CliHelpDangerouslySkipPermissions).into_owned())
        });

    // Mutate subcommand about texts
    let cmd = cmd
        .mut_subcommand("login", |s| s.about(t(Msg::CliAboutLogin).into_owned()))
        .mut_subcommand("logout", |s| s.about(t(Msg::CliAboutLogout).into_owned()))
        .mut_subcommand("status", |s| s.about(t(Msg::CliAboutStatus).into_owned()))
        .mut_subcommand("upgrade", |s| s.about(t(Msg::CliAboutUpgrade).into_owned()))
        .mut_subcommand("rollback", |s| {
            s.about(t(Msg::CliAboutRollback).into_owned())
        })
        .mut_subcommand("mcp", |s| s.about(t(Msg::CliAboutMcp).into_owned()))
        .mut_subcommand("daemon", |s| s.about(t(Msg::CliAboutDaemon).into_owned()))
        .mut_subcommand("webui", |s| s.about(t(Msg::CliAboutWebui).into_owned()))
        .mut_subcommand("telemetry", |s| {
            s.about(t(Msg::CliAboutTelemetry).into_owned())
        })
        .mut_subcommand("plugin", |s| s.about(t(Msg::CliAboutPlugin).into_owned()))
        .mut_subcommand("uninstall", |s| {
            s.about(t(Msg::CliAboutUninstall).into_owned())
        })
        .mut_subcommand("setup", |s| s.about(t(Msg::CliAboutSetup).into_owned()))
        .mut_subcommand("hooks", |s| s.about(t(Msg::CliAboutHooks).into_owned()));

    cmd
}

/// Body of the detached upgrade-prep worker. One call to
/// `prepare_deferred_upgrade` (which fetches the manifest, downloads the
/// next version's binary if newer, verifies sha256, and writes
/// `pending.json`). On success the next parent-atomcode start will pick
/// up `pending.json` and apply. Silent: stdout/stderr are already /dev/null
/// (see `spawn_detached_upgrade_prep`), so any output would be discarded.
async fn run_prepare_upgrade_worker() -> i32 {
    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    // UpgradeEvent stream is per-byte progress; we don't surface it here
    // (parent is gone), so drain to /dev/null.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<atomcode_updater::UpgradeEvent>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    match atomcode_updater::prepare_deferred_upgrade(&current, tx).await {
        Ok(_) => 0,
        Err(_) => 1,
    }
}

/// Whether the startup-time SYNCHRONOUS upgrade path should fire. Restored (with the
/// detached stager kept alongside) after 31daa6ee removed it: the everyday startup cost
/// this guards is only a small `latest.json` fetch, so a fresh release is applied on THIS launch
/// instead of requiring two restarts. Returns false for `.bak`, dev, `ATOMCODE_PLAIN`,
/// headless `-p`, subcommands, `auto_update = false`, or offline mode.
fn should_try_sync_upgrade() -> bool {
    if is_running_as_backup() {
        return false;
    }
    if is_dev_mode() {
        return false;
    }
    // Package-managed (distro-pm / HarmonyBrew) builds: upgrades belong to the package
    // manager. Match the detached stager's gate so we don't spin up the check for a
    // guaranteed no-op (`prepare_deferred_upgrade` early-returns on pm builds anyway).
    if atomcode_updater::is_package_managed() {
        return false;
    }

    // PlainRenderer 模式（ATOMCODE_PLAIN=1）：跳过同步自更新检查。
    // 自更新用 eprintln! 直接写 stderr，和 PlainRenderer 的 stdout
    // 流式输出交错，破坏启动体验。后台异步自更新不受影响，用户仍可
    // 手动 /upgrade。
    if std::env::var("ATOMCODE_PLAIN")
        .ok()
        .filter(|v| !v.is_empty())
        .is_some()
    {
        return false;
    }

    let args: Vec<String> = std::env::args().collect();
    let any = |needle: &[&str]| {
        args.iter().skip(1).any(|a| {
            needle
                .iter()
                .any(|n| a == n || a.starts_with(&format!("{}=", n)))
        })
    };

    if any(&["-p", "--prompt", "--prompt-file"]) {
        return false;
    }
    if args.iter().skip(1).any(|a| {
        matches!(
            a.as_str(),
            "login"
                | "logout"
                | "status"
                | "upgrade"
                | "rollback"
                | "uninstall"
                | "mcp"
                | "telemetry"
                | "completion"
                | "--version"
                | "-V"
                | "--help"
                | "-h"
        )
    }) {
        return false;
    }

    // Load config once to honor both `auto_update = false` and `offline_mode`.
    // Runs pre-seed, so offline is resolved directly rather than via the process
    // verdict. Env wins over config; only forced On skips. Failure to load = assume
    // defaults (auto_update true, offline Off) — fresh installs benefit.
    let path = atomcode_config::config::Config::default_path();
    let offline_mode = if path.exists() {
        if let Ok(cfg) = atomcode_config::config::Config::load(&path) {
            if !cfg.auto_update {
                return false;
            }
            cfg.offline_mode
        } else {
            atomcode_config::config::offline::OfflineMode::Off
        }
    } else {
        atomcode_config::config::offline::OfflineMode::Off
    };
    // Offline (env wins over config; only forced On skips) disables binary self-update,
    // same as auto_update=false. Works even with no config file (e.g. air-gapped container).
    if atomcode_config::config::offline::offline_resolved(
        offline_mode,
        std::env::var(atomcode_config::config::offline::ATOMCODE_OFFLINE_ENV)
            .ok()
            .as_deref(),
    ) {
        return false;
    }

    true
}

/// Synchronous startup upgrade: fetch the manifest, and if a newer version is out,
/// stage + apply it NOW and re-exec, so the user gets the new binary on THIS launch.
/// Bounded by a 120s timeout; on timeout/error it just continues (the detached stager
/// and `/upgrade` remain as fallbacks). Restored verbatim from the pre-31daa6ee path.
async fn sync_stage_and_apply_if_newer() {
    use atomcode_updater::{self as self_update, UpgradeEvent};

    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UpgradeEvent>();

    // Progress consumer: renders ManifestFetched / Downloading / Verifying
    // as a single-line updating status on stderr. Percent-debounced so a
    // 15 MB download at 64 KiB chunks doesn't flood the terminal.
    let progress = tokio::spawn(async move {
        use std::io::Write;
        let mut last_pct: i32 = -1;
        while let Some(ev) = rx.recv().await {
            match ev {
                UpgradeEvent::ManifestFetched { version } => {
                    eprintln!("✨ New version available: {}", version);
                }
                UpgradeEvent::Downloading { bytes, total } => {
                    let pct = if total == 0 {
                        0
                    } else {
                        ((bytes * 100) / total) as i32
                    };
                    if pct != last_pct {
                        eprint!(
                            "\r   Downloading {}% ({:.1} / {:.1} MB)      ",
                            pct,
                            bytes as f64 / 1_048_576.0,
                            total as f64 / 1_048_576.0
                        );
                        let _ = std::io::stderr().flush();
                        last_pct = pct;
                    }
                }
                UpgradeEvent::Verifying => {
                    eprintln!("\n✓ Verifying sha256");
                }
                _ => {}
            }
        }
    });

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        self_update::prepare_deferred_upgrade(&current, tx),
    )
    .await;

    // Wait briefly for the progress consumer to drain — it closes when
    // the sender drops at the end of prepare_deferred_upgrade.
    let _ = progress.await;

    match outcome {
        Ok(Ok(Some(_staged))) => {
            // Staged successfully. Apply right now so the user gets the new
            // binary on this same invocation.
            match self_update::apply_pending_upgrade() {
                Ok(Some(applied)) => {
                    eprintln!("✓ Upgrading to {}...", applied.version);
                    // Save the CURRENT version (before upgrade) so TUI can show "Upgraded old → new"
                    std::env::set_var(UPGRADED_FROM_ENV, &current);
                    match self_update::re_exec_self(Some(&applied.exe)) {
                        Ok(_infallible) => unreachable!("re_exec_self returned Ok"),
                        Err(e) => {
                            eprintln!(
                                "Upgrade applied but re-exec failed ({}). The new version will be used on the next launch.",
                                e
                            );
                            std::env::remove_var(UPGRADED_FROM_ENV);
                        }
                    }
                }
                _ => {
                    // Stage succeeded but apply didn't — weird, just continue.
                }
            }
        }
        Ok(Ok(None)) => {
            // Already latest, no-op.
        }
        Ok(Err(_)) | Err(_) => {
            // Network error or 120 s timeout. Don't spam the user —
            // `/upgrade` will surface the real error if they ask.
            eprintln!("Note: could not check for updates at startup (will retry in background).");
        }
    }
}

/// Spawn a detached copy of this binary that runs the upgrade-prep worker
/// and exits. "Detached" means:
///   * New session on Unix (`setsid`) — parent's Ctrl+C goes to parent's
///     foreground process group only; the child is in its own and ignores it.
///   * `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` on Windows, same idea
///   * stdin/stdout/stderr → /dev/null so the child can't scribble over the
///     parent's terminal and has no reason to stay attached to it.
///
/// Does NOT wait for the child (we intentionally don't — that would recreate
/// the cancel-on-exit problem we're trying to solve). If spawning fails we
/// just drop the error; auto-upgrade is best-effort.
fn spawn_detached_upgrade_prep() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return,
    };

    let mut cmd = std::process::Command::new(&exe);
    cmd.env(INTERNAL_PREPARE_UPGRADE_ENV, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                // SAFETY(pre_exec): runs in the forked child before exec —
                // async-signal-safe libc ONLY. No allocation, locks, panics, or
                // non-reentrant calls, or the child can deadlock. libc::setsid() is safe.
                // Detach from parent's controlling terminal / process group.
                // Return value ignored — setsid only fails when caller is
                // already a process group leader (not our case post-fork).
                libc::setsid();
                Ok(())
            });
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }

    let _ = cmd.spawn();
}

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("ATOMCODE_BUILD_ID"),
    env!("ATOMCODE_BUILD_DIRTY"),
    ")"
);

/// The name this binary is invoked as, taken from `[[bin]] name` rather than
/// repeated as a literal. It reaches the user in three places that must agree:
/// the `Usage:` line, the `--help` header, and the `complete -F` registration a
/// generated completion script installs. Renaming the bin used to leave all
/// three claiming the old name — and shell completion bound to a command that
/// no longer exists.
const BIN_NAME: &str = env!("CARGO_BIN_NAME");

#[derive(Parser)]
#[command(name = BIN_NAME, version = VERSION, about = "AI coding assistant in your terminal")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Continue the previous session instead of starting a new one
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,

    /// Provider to use (overrides config default)
    #[arg(long)]
    provider: Option<String>,

    /// Model to use (overrides config provider model)
    #[arg(long)]
    model: Option<String>,

    /// Set interface language (e.g. en, zh-CN, zh)
    #[arg(long)]
    lang: Option<String>,

    /// Path to config file
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    config: Option<PathBuf>,

    /// FIRST-RUN ONLY: seed the user's config from this file when they don't yet
    /// have one (`~/.atomcode/config.toml` absent). Copies it in once, then never
    /// touches it again — the user owns the writable copy. On read/parse failure,
    /// falls back to normal onboarding (never blocks startup). For offline/managed
    /// deploys (e.g. a bundled `atomcode-default-config.toml` shipped next to the
    /// binary): point this at that file via the launcher. Env: `ATOMCODE_SEED_CONFIG`.
    /// No-op when the user already has a config, so it's safe to always pass.
    /// Env `ATOMCODE_SEED_CONFIG` is honored as a fallback when the flag is absent.
    #[arg(long, value_name = "PATH", value_hint = clap::ValueHint::FilePath)]
    seed_config: Option<PathBuf>,

    /// Working directory (defaults to current directory)
    #[arg(long, short = 'C', value_hint = clap::ValueHint::DirPath)]
    dir: Option<PathBuf>,

    /// Prompt to run in headless (non-interactive) mode. If omitted, launches the TUI.
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// Read the prompt from a file (alternative to -p). Useful for long prompts
    /// that would exceed ARG_MAX or whose trailing newlines matter.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "prompt",
        value_hint = clap::ValueHint::FilePath
    )]
    prompt_file: Option<std::path::PathBuf>,

    /// Show tool calls, token usage, and turn summary on stderr (headless mode only).
    /// Without this flag, headless output is the assistant reply only — Claude Code -p style.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Disable auto-update for this launch. Skips applying any staged
    /// upgrade and skips the detached background stager. Use during
    /// local development so a
    /// fresh `cargo run` build isn't silently overwritten by the
    /// released binary.
    #[arg(long)]
    dev: bool,

    /// Disable telemetry for this invocation.
    #[arg(long = "no-telemetry", default_value_t = false, global = true)]
    pub no_telemetry: bool,

    /// Skip all permission prompts — auto-approve every tool call (bash,
    /// file edits, MCP, etc.). Equivalent to Claude Code's
    /// --dangerously-skip-permissions. The TUI shows a red ⚠ BYPASS
    /// badge while active. Use in CI/CD, eval harnesses, or when you
    /// trust the agent's built-in safety constraints.
    #[arg(
        short = 'y',
        long = "dangerously-skip-permissions",
        default_value_t = false
    )]
    pub dangerously_skip_permissions: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Sign in with AtomGit OAuth and claim CodingPlan models in one
    /// flow: OAuth (if needed) → claim → fetch models → register
    /// providers → fetch status. Reports each step and exits.
    Login,
    /// Logout from AtomCode
    Logout,
    /// Show current login status
    Status,
    /// Upgrade atomcode in-place to the latest released version
    Upgrade {
        /// Reinstall even when already on the latest version
        #[arg(long)]
        force: bool,
    },
    /// Roll back to the previous version (swap with .bak on disk)
    Rollback,
    /// Hidden alias for `atomcode login` — kept so existing scripts /
    /// muscle memory don't break after `/codingplan` and `atomcode
    /// codingplan` were folded into the unified `/login` flow.
    #[command(hide = true)]
    Codingplan,
    /// Manage MCP server entries in `.mcp.json` (similar to `claude mcp add`)
    #[command(subcommand)]
    Mcp(McpCli),
    /// Start the HTTP daemon for IDE integration (VS Code extension connects to this)
    Daemon {
        /// Port to listen on
        #[arg(long, default_value_t = atomcode_config::distribution::DAEMON_PORT)]
        port: u16,
        /// Client identifier for telemetry (e.g. "vscode", "atomcode-air")
        #[arg(long)]
        client: Option<String>,
        /// Idle-shutdown timeout in seconds; 0 disables. Env
        /// ATOMCODE_DAEMON_IDLE_TIMEOUT overrides. Default 1800 (30 min).
        #[arg(long)]
        idle_timeout: Option<u64>,
    },
    /// 启动本地浏览器 webui（进程内起 server，无需额外二进制）
    Webui {
        /// 端口（默认 13457，刻意错开 VSCode 守护进程的 13456，避免抢端口导致扩展 401/无响应）
        #[arg(long, default_value_t = atomcode_daemon::WEBUI_DEFAULT_PORT)]
        port: u16,
        /// 绑定地址（默认 127.0.0.1；用 0.0.0.0 暴露到局域网/外网，注意仅 token 保护、无 TLS）
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
    },
    /// Telemetry controls
    Telemetry {
        #[command(subcommand)]
        action: TelemetryAction,
    },
    /// Manage skill/command plugins (mirrors `claude plugin ...`).
    /// Operates on `$ATOMCODE_HOME/plugins/` shared with the TUI's `/plugin`
    /// slash command — anything installed via either path is visible to both.
    #[command(subcommand)]
    Plugin(PluginCli),
    /// Uninstall AtomCode: remove the binary, PATH edit, and (interactively)
    /// data under ~/.atomcode/. With no flags, runs interactively and asks
    /// per-group; pass --yes / --purge / --keep-data for non-interactive use.
    Uninstall {
        /// Skip prompts; use per-group default decisions
        /// (binary=yes, credentials=no, state=yes).
        #[arg(long)]
        yes: bool,
        /// Wipe ~/.atomcode/ entirely.
        #[arg(long, conflicts_with = "keep_data")]
        purge: bool,
        /// Keep ~/.atomcode/ entirely (only remove binary + PATH edit).
        #[arg(long)]
        keep_data: bool,
        /// Print the plan; do nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Install seed files (skills/commands/hooks/MCP) to `~/.atomcode/`.
    Setup {
        /// Take over a stale lock AND force reinstall even if seeds are already present.
        #[arg(long)]
        force: bool,
    },
    /// Manage hooks (list, test, enable/disable)
    #[command(subcommand)]
    Hooks(HookCommands),
    /// Manage local scheduled tasks (add/list/remove/enable/disable).
    #[command(subcommand)]
    Schedule(schedule_cmd::ScheduleCli),
    /// Generate a shell completion script on stdout.
    Completion(CompletionCommand),
    /// Internal: askpass helper invoked by sudo/ssh via SUDO_ASKPASS / SSH_ASKPASS.
    /// Not intended for direct user invocation.
    #[command(name = "__askpass", hide = true)]
    Askpass {
        /// The prompt string forwarded by sudo/ssh (e.g. "[sudo] password:").
        prompt: String,
    },
    /// Run as an Agent Client Protocol (ACP) agent over stdio.
    ///
    /// stdout is reserved exclusively for the ACP JSON-RPC stream.
    /// Provider and model are taken from the active configuration
    /// (same as the TUI/headless path); per-session cwd comes from
    /// the ACP client's `session/new` request.
    #[command(hide = true)]
    Acp,
}

#[derive(clap::Args)]
struct CompletionCommand {
    /// Shell to generate completions for.
    #[arg(value_enum, default_value_t = Shell::Bash)]
    shell: Shell,
}

/// Parse and serve `atomcode completion [SHELL]` before normal startup.
///
/// `Cli::try_parse` is intentionally used instead of hand-parsing argv so
/// global options and clap validation retain their canonical semantics. We
/// only surface a parse error early when argv contains the completion
/// subcommand token; every other invocation falls through to the existing
/// localized parse path.
fn try_print_shell_completion() -> bool {
    if !is_completion_invocation(std::env::args_os().skip(1)) {
        return false;
    }

    match Cli::try_parse() {
        Ok(Cli {
            command: Some(Commands::Completion(command)),
            ..
        }) => {
            print_shell_completion(command.shell, &mut std::io::stdout());
            true
        }
        Ok(_) => false,
        Err(error) => error.exit(),
    }
}

fn is_completion_invocation(args: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let Some(arg) = arg.to_str() else {
            return false;
        };
        match arg {
            // `--` ends root option parsing; anything after it is input, not
            // AtomCode's completion subcommand.
            "--" => return false,
            // Root flags that do not consume a value.
            "-c"
            | "--continue"
            | "-v"
            | "--verbose"
            | "--dev"
            | "--no-telemetry"
            | "-y"
            | "--dangerously-skip-permissions" => {}
            // Root options that consume the following argv item.
            "--provider" | "--model" | "--lang" | "--config" | "--seed-config" | "-C" | "--dir"
            | "-p" | "--prompt" | "--prompt-file" => {
                if args.next().is_none() {
                    return false;
                }
            }
            // Long options may carry their value after `=`.
            value
                if [
                    "--provider=",
                    "--model=",
                    "--lang=",
                    "--config=",
                    "--seed-config=",
                    "--dir=",
                    "--prompt=",
                    "--prompt-file=",
                ]
                .iter()
                .any(|prefix| value.starts_with(prefix)) => {}
            // The first root positional token is the subcommand.
            value if !value.starts_with('-') => return value == "completion",
            // Unknown/combined options belong to the canonical parser.
            _ => return false,
        }
    }
    false
}

fn completion_command() -> clap::Command {
    let source = Cli::command();
    let visible_subcommands = source
        .get_subcommands()
        .filter(|command| !command.is_hide_set())
        .cloned()
        .collect::<Vec<_>>();

    clap::Command::new(BIN_NAME)
        .version(VERSION)
        .about("AI coding assistant in your terminal")
        .args(source.get_arguments().cloned())
        .groups(source.get_groups().cloned())
        .subcommands(visible_subcommands)
}

fn print_shell_completion(shell: Shell, out: &mut dyn Write) {
    let mut command = completion_command();
    clap_complete::generate(shell, &mut command, BIN_NAME, out);
}

/// Subcommands for hooks management
#[derive(Subcommand)]
enum HookCommands {
    /// List all loaded hooks with their status
    List,
    /// Test a specific hook by name
    Test {
        /// Hook name to test
        name: String,
    },
    /// Show hook configuration paths
    Paths,
}

#[derive(Subcommand)]
enum PluginCli {
    /// Marketplace registry operations (add/remove/update/list).
    #[command(subcommand)]
    Marketplace(MarketplaceCli),
    /// Install a plugin from a registered marketplace.
    /// Spec format: `<plugin>@<marketplace>` (matches the slash command).
    Install {
        /// e.g. `ascend-model-agent-plugin@ascend-model-agent-plugin`
        spec: String,
    },
    /// Uninstall a previously-installed plugin (does not touch its marketplace).
    Uninstall {
        /// e.g. `ascend-model-agent-plugin@ascend-model-agent-plugin`
        spec: String,
    },
    /// Trust an installed plugin's hooks so they run (records the current hook-set hash).
    Trust {
        /// Plugin name (as installed), or `plugin@marketplace` for disambiguation.
        name: String,
    },
    /// Untrust an installed plugin's hooks (they stop running).
    Untrust {
        /// Plugin name (as installed), or `plugin@marketplace` for disambiguation.
        name: String,
    },
    /// List installed plugins.
    List,
}

#[derive(Subcommand)]
enum MarketplaceCli {
    /// Clone a marketplace git repo and register it locally.
    Add {
        /// Git URL (https or ssh) of a marketplace repo.
        url: String,
    },
    /// Drop a registered marketplace. Refuses if any plugin still installed.
    Remove {
        /// Marketplace name (the key shown by `marketplace list`).
        name: String,
    },
    /// Re-pull a registered marketplace and refresh its plugin index.
    Update { name: String },
    /// List registered marketplaces.
    List,
}

#[derive(Subcommand)]
enum McpCli {
    /// Add or replace a stdio MCP server (`mcpServers.<name>` with `command` + `args`)
    Add {
        /// Server key (tools appear as `mcp__<name>__…`)
        name: String,
        /// Executable and arguments, e.g. `npx @playwright/mcp@latest`
        #[arg(required = true, num_args = 1..)]
        command: Vec<String>,
        /// Write `~/.atomcode/mcp.json` instead of `<dir>/.mcp.json`
        #[arg(long)]
        global: bool,
        /// Directory for project `.mcp.json` (defaults to current directory)
        #[arg(short = 'C', long, value_hint = clap::ValueHint::DirPath)]
        dir: Option<PathBuf>,
    },
    /// Add GitHub's remote MCP server using OAuth.
    AddGithubOauth {
        /// Server key (tools appear as `mcp__<name>__…`)
        #[arg(default_value = "github")]
        name: String,
        /// Write `~/.atomcode/mcp.json` instead of `<dir>/.mcp.json`
        #[arg(long)]
        global: bool,
        /// Directory for project `.mcp.json` (defaults to current directory)
        #[arg(short = 'C', long, value_hint = clap::ValueHint::DirPath)]
        dir: Option<PathBuf>,
    },
    /// Complete OAuth login for a remote MCP server.
    Login {
        /// Server key in mcpServers (for GitHub, usually `github`)
        name: String,
        /// OAuth provider to use.
        #[arg(long, default_value = "github")]
        provider: String,
        /// OAuth client id. Defaults to ATOMCODE_GITHUB_MCP_CLIENT_ID.
        #[arg(long)]
        client_id: Option<String>,
        /// Environment variable containing the OAuth client secret.
        #[arg(long)]
        client_secret_env: Option<String>,
        /// OAuth scopes. Defaults to GitHub MCP's broad repo-oriented set.
        #[arg(long, value_delimiter = ',')]
        scopes: Vec<String>,
    },
    /// Remove saved OAuth credentials for a remote MCP server.
    Logout {
        /// Server key in mcpServers.
        name: String,
    },
}

#[derive(clap::Subcommand)]
pub enum TelemetryAction {
    /// Show current telemetry state and queue stats
    Status,
    /// Enable telemetry (writes to ~/.atomcode/config.toml)
    Enable,
    /// Disable telemetry (writes to ~/.atomcode/config.toml)
    Disable,
    /// Print pending queued events (never-sent)
    Dump {
        #[arg(long, default_value_t = 50)]
        last: usize,
        #[arg(long)]
        pretty: bool,
    },
    /// Clear queued events (does not change enabled state)
    Clear,
    /// Recover legacy .partial files after all older AtomCode processes stop
    Recover,
}

/// Environment variable set by this process for its re-exec'd child, so
/// the child knows which version it was just upgraded from and can show
/// a one-time "✓ Upgraded to vX.Y.Z" banner on the welcome screen.
/// The child clears this env var after reading it so grandchildren
/// (spawned tools, subprocesses) don't inherit a stale hint.
const UPGRADED_FROM_ENV: &str = "ATOMCODE_UPGRADED_FROM";

/// Env var the parent sets when spawning a detached upgrade-prep worker.
/// The child detects it at the very top of `main` and runs one
/// `prepare_deferred_upgrade` cycle in its own session (setsid'd) so the
/// parent can be Ctrl+C'd without cancelling the download.
const INTERNAL_PREPARE_UPGRADE_ENV: &str = "ATOMCODE_INTERNAL_PREPARE_UPGRADE";

fn main() {
    // Completion generation must be a pure, fast CLI operation: no helper
    // thread, Tokio runtime, log file, config read, telemetry, or updater.
    // Shells may invoke completion helpers frequently, so even best-effort
    // startup work here would turn Tab into network/filesystem activity.
    if try_print_shell_completion() {
        return;
    }

    // Settle where the config tree is before anything reads it, so all eight
    // resolvers — and every child process that inherits our environment —
    // agree by construction. Deliberately AFTER the completion fast path: that
    // branch touches no config, and the comment above asks for it to stay bare.
    atomcode_config::distribution::bootstrap_home();

    // Run the entire program on a thread with a large, explicit stack.
    // Rust gives the *main* OS thread the platform-default stack — on
    // Windows that's only ~1 MB (vs 8 MB on Linux/macOS). The TUI event
    // loop, the synchronous codingplan/OAuth work, and the rustls TLS
    // handshakes all run on it via `block_on`, and a deep call chain there
    // can overflow 1 MB. A stack overflow on Windows kills the process via
    // an OS exception (STATUS_STACK_OVERFLOW) WITHOUT a Rust panic — so it
    // never reaches the crash-log hook and looks like a silent exit. A
    // 16 MB stack removes that platform asymmetry. (See the Windows
    // post-scan onboarding crash investigation.)
    let child = std::thread::Builder::new()
        .name("atomcode-main".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(real_main)
        .expect("failed to spawn atomcode main thread");
    // Under `panic = "abort"` a panic in the child already aborts the whole
    // process, so `join` only returns an error on an abnormal thread exit;
    // mirror Rust's conventional panic exit code in that case.
    if child.join().is_err() {
        std::process::exit(101);
    }
}

fn real_main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Worker threads (for `tokio::spawn`ed tasks) get a generous stack
        // too — same rationale as the main thread above.
        .thread_stack_size(8 * 1024 * 1024)
        .build()
        .expect("failed to build tokio runtime");
    rt.block_on(async_main());
}

fn merge_startup_notices(
    config_notice: Option<String>,
    session_notice: Option<String>,
) -> Option<String> {
    match (config_notice, session_notice) {
        (Some(config), Some(session)) => Some(format!("{config}\n{session}")),
        (Some(config), None) => Some(config),
        (None, Some(session)) => Some(session),
        (None, None) => None,
    }
}

async fn async_main() {
    let process_start = std::time::Instant::now();
    // Wire `tracing::` diagnostics to `<config_dir>/logs/atomcode.log` (file-only,
    // TUI-safe). Must run before anything that emits traces so nothing is lost.
    init_file_logging();
    // Set Windows console to UTF-8 so CJK and other multi-byte characters
    // render correctly instead of showing garbled output (mojibake).
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::Globalization::CP_UTF8;
        use windows_sys::Win32::System::Console::{
            GetConsoleCP, GetConsoleOutputCP, SetConsoleCP, SetConsoleOutputCP,
        };
        unsafe {
            SetConsoleOutputCP(CP_UTF8);
            SetConsoleCP(CP_UTF8);

            // Also check output code page, same best-effort as input.
            let actual_cp = GetConsoleCP();
            let actual_out_cp = GetConsoleOutputCP();
            if actual_cp != CP_UTF8 || actual_out_cp != CP_UTF8 {
                let _ = eprintln!(
                    "\n⚠  Console code pages — input: {} (expected 65001/UTF-8), output: {}.\n\
                       Chinese/Japanese/Korean IME input/output may show garbled text.\n\
                       → Use Windows Terminal for native UTF-8 support.\n\
                       → Or enable Beta: Use Unicode UTF-8 in Region settings.\n",
                    actual_cp, actual_out_cp,
                );
            }
        }
    }

    // Detached upgrade-prep worker mode. The parent atomcode spawns a
    // subprocess with this env var set; that subprocess does one full
    // download + verify + `pending.json` write, then exits. Because the
    // subprocess is setsid'd (see `spawn_detached_upgrade_prep`), it
    // survives Ctrl+C / quit in the parent — which is the whole point,
    // since the previous in-process download was tied to the parent's
    // tokio runtime and got cancelled on any quick exit.
    if std::env::var(INTERNAL_PREPARE_UPGRADE_ENV).is_ok() {
        let code = run_prepare_upgrade_worker().await;
        std::process::exit(code);
    }

    // If this invocation is the `.bak` backup binary (left behind by a
    // previous upgrade), skip all upgrade bootstrapping. `apply_pending_upgrade`
    // would rewrite ourselves with the latest version and destroy the
    // rollback target; the whole point of keeping `.bak` is for the user
    // to be able to run / keep the old version. The only upgrade path
    // still reachable from a `.bak` launch is the explicit `/upgrade`
    // slash command inside the TUI — that's user-initiated and fine.
    let is_backup = is_running_as_backup();
    let dev_mode = is_dev_mode();
    if dev_mode {
        eprintln!("[dev] auto-update disabled");
    }

    // Bootstrap: if a prior session staged an upgrade, apply it NOW — before
    // we spin up tokio, the TUI, or any other heavy state. On success we
    // re-exec the new binary (Unix: same PID; Windows: child+exit). The user
    // sees one continuous "atomcode" invocation, just 100-300ms longer than
    // normal. On failure we log and carry on with the current binary; the
    // circuit-breaker in `apply_pending_upgrade` ensures a broken release
    // can't wedge this loop indefinitely.
    if !is_backup && !dev_mode {
        let stage_start = std::time::Instant::now();
        // Capture current version BEFORE applying upgrade, so we can pass it to the re-exec'd child
        let current_version = format!("v{}", env!("CARGO_PKG_VERSION"));
        match atomcode_updater::apply_pending_upgrade() {
            Ok(Some(applied)) => {
                eprintln!("✓ Upgrading to {}...", applied.version);
                // Pass the CURRENT version (before upgrade) to the re-exec'd child so the TUI
                // can surface a welcome-screen confirmation exactly once.
                std::env::set_var(UPGRADED_FROM_ENV, &current_version);
                match atomcode_updater::re_exec_self(Some(&applied.exe)) {
                    Ok(_infallible) => unreachable!("re_exec_self returned Ok"),
                    Err(e) => {
                        eprintln!(
                        "Upgrade applied but re-exec failed ({}). The new version will be used on the next launch.",
                        e
                    );
                        std::env::remove_var(UPGRADED_FROM_ENV);
                        std::process::exit(1);
                    }
                }
            }
            Ok(None) => {
                // Nothing was staged by a prior session. Do a fresh synchronous
                // check → stage → apply so a newly-released version is picked up on
                // THIS launch (restores the pre-31daa6ee "one restart upgrades you"
                // behavior). The everyday no-update case is just a small latest.json
                // fetch; should_try_sync_upgrade's gates + a 120s timeout keep
                // headless / opted-out / offline runs fast, and the detached stager
                // below stays as the survives-quick-exit fallback.
                if should_try_sync_upgrade() {
                    // Remember we checked, so the post-parse detached stager doesn't
                    // re-fetch the manifest a second time this launch.
                    SYNC_UPGRADE_CHECKED.store(true, Ordering::Relaxed);
                    sync_stage_and_apply_if_newer().await;
                }
            }
            Err(e) => {
                eprintln!("Note: pending upgrade could not be applied ({}). Continuing with current version.", e);
            }
        }
        tracing::info!(
            target: "atomcode::startup",
            stage = "pending_upgrade",
            elapsed_ms = stage_start.elapsed().as_millis() as u64,
            total_ms = process_start.elapsed().as_millis() as u64,
            "startup stage completed"
        );
    } // end `if !is_backup`

    // Set a minimal pre-telemetry panic hook (replaced after telemetry init in run()).
    std::panic::set_hook(Box::new(|info| {
        write_crash_log(info);
        restore_terminal_if_tui();
        eprintln!("\nAtomCode crashed: {}", info);
        if let Some(location) = info.location() {
            eprintln!(
                "  at {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            );
        }
        eprintln!("\nPlease report this at: https://atomgit.com/atomgit_atomcode/atomcode/issues");
    }));

    match run().await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            restore_terminal_if_tui();
            eprintln!("\nAtomCode error: {:#}", e);
            std::process::exit(1);
        }
    }
}

async fn run() -> Result<i32> {
    let run_start = std::time::Instant::now();
    // -- Pre-parse --lang to set locale BEFORE clap renders --help --
    // clap help text is generated during parse, so we must resolve
    // the locale first (from --lang flag, env vars, or config) so that
    // the i18n system is ready when clap calls our dynamic about/help closures.
    let pre_lang = scan_argv_for_lang();
    // Also read config language field so --help respects /language setting.
    let pre_config_lang = scan_config_language();
    let pre_locale =
        atomcode_tuix::i18n::resolve_initial_locale(pre_lang.as_deref(), pre_config_lang);
    atomcode_tuix::i18n::set_locale(pre_locale);

    // Build the clap Command with i18n-injected about/help text, then parse.
    // Check if --help or -h was requested by scanning argv.
    // If so, render the i18n-localised help via our custom Command and exit.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        let help_cmd = build_i18n_command();
        help_cmd
            .try_get_matches_from(std::env::args_os())
            .unwrap_or_else(|e| e.exit());
        // Unreachable: e.exit() prints the localised help and exits.
    }

    // No --help was passed. Parse normally to get the Cli struct.
    let cli = Cli::parse();

    // ── Askpass early exit ────────────────────────────────────────────────────
    // Handle `atomcode __askpass <prompt>` before ANY TUI/telemetry setup.
    // sudo/ssh invoke this helper synchronously; it must not spawn async
    // runtimes, connect to telemetry, or open a terminal.
    if let Some(Commands::Askpass { prompt }) = &cli.command {
        #[cfg(unix)]
        {
            use std::path::Path;
            let sock = std::env::var("ATOMCODE_ASKPASS_SOCK").ok();
            let token = std::env::var("ATOMCODE_ASKPASS_TOKEN").ok();
            match (sock, token) {
                (Some(s), Some(t)) => {
                    match atomcode::askpass::run_askpass(prompt, Path::new(&s), &t) {
                        Some(pw) => {
                            use std::io::Write;
                            print!("{pw}");
                            let _ = std::io::stdout().flush();
                            return Ok(0);
                        }
                        None => return Ok(1),
                    }
                }
                _ => return Ok(1),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = prompt;
            return Ok(1);
        }
    }
    // ── End askpass early exit ────────────────────────────────────────────────

    let is_admin = atomcode_capabilities::process_utils::is_running_as_admin();

    // ── Telemetry init ────────────────────────────────────────────────────────
    // Load config early (before subcommand dispatch) so we can read the
    // [telemetry] section AND seed the offline verdict + note before any
    // tool/persona/provider assembly. Failure to load config is non-fatal;
    // telemetry will operate on defaults (enabled, built-in endpoint).
    let config_path_for_tel = cli.config.clone().unwrap_or_else(Config::default_path);
    let early_config = if config_path_for_tel.exists() {
        Config::load(&config_path_for_tel).ok()
    } else {
        None
    };
    let telemetry_cfg = early_config
        .as_ref()
        .map(|c| c.telemetry.clone())
        .unwrap_or_default();

    // Seed the offline verdict + note ONCE from config + env, before any tool/telemetry assembly.
    atomcode_config::config::offline::seed_offline_from_config(early_config.as_ref());
    let atomcode_dir = Config::config_dir();
    let cli_override = CliOverride {
        disabled: cli.no_telemetry,
    };
    let resolved = resolve(
        &telemetry_cfg,
        &cli_override,
        atomcode_dir.clone(),
        &ProcessEnv,
        atomcode_config::config::offline::is_offline_active(),
    );

    // First-run notice: only show when telemetry would be active.
    if resolved.state.is_enabled() {
        if let Ok(true) = notice::should_show_and_mark(&resolved.atomcode_dir) {
            eprintln!("{}", notice::NOTICE_TEXT);
        }
    }

    let telemetry = Telemetry::init(resolved.clone(), env!("CARGO_PKG_VERSION").into());
    install_panic_hook(telemetry.clone());

    // Emit install_completed if this is the first launch after a referral install
    telemetry
        .maybe_emit_install_completed(&resolved.atomcode_dir)
        .await;
    // ── End telemetry init ────────────────────────────────────────────────────

    // Handle subcommands. Most are self-contained (`handle_command` runs
    // and exits); `Login` (and its hidden alias `Codingplan`) run the
    // full OAuth + CodingPlan setup flow and then fall through to the
    // TUI.

    let force_verbose = false;
    if let Some(cmd) = cli.command {
        match cmd {
            Commands::Login | Commands::Codingplan => {
                // Unified login flow: OAuth (if needed) → claim → fetch
                // models → register providers → fetch status. Falls
                // through to TUI startup regardless of outcome. On
                // success the freshly saved config.toml is picked up by
                // `Config::load` further down. On failure the TUI opens
                // in onboarding mode (no providers) so the user can
                // retry via `/login` without re-launching the binary.
                // Emits open_atomcode (mode=headless) then take_codingplan
                // (emitted internally by run_codingplan_core via coding_plan::run).
                HEADLESS_MODE.store(true, Ordering::Relaxed);
                let repo = atomcode_telemetry::detect_repo_origin(
                    &std::env::current_dir().unwrap_or_default(),
                );
                telemetry.set_account_id(auth::get_stored_auth().map(|a| a.user.id.to_string()));
                let scope_ctx = CurrentContext {
                    repo_origin: Some(repo),
                    mode: Some(SessionMode::Headless),
                    ..CurrentContext::current()
                };
                // Emit the open event inside the async task-local scope — this
                // is a cheap, non-blocking mpsc send.
                let dsp = cli.dangerously_skip_permissions;
                let tel_for_event = telemetry.clone();
                CurrentContext::scope(scope_ctx.clone(), || async move {
                    tel_for_event.track(Event::OpenAtomcode {
                        dangerously_skip_permissions: dsp,
                    });
                })
                .await;
                // The OAuth + claim flow is fully synchronous and builds a
                // `reqwest::blocking` client, which stands up its own tokio
                // runtime. Running it directly on an async worker thread panics
                // when that inner runtime is dropped ("Cannot drop a runtime in
                // a context where blocking is not allowed"). Move it onto a
                // dedicated blocking thread — the same convention the plugin
                // bootstrap uses — and re-establish the telemetry task-local
                // there, since spawn_blocking threads don't inherit it.
                let outcome = {
                    let telemetry = telemetry.clone();
                    tokio::task::spawn_blocking(move || {
                        CurrentContext::scope_blocking(scope_ctx, || {
                            run_codingplan_core(Some(&telemetry))
                        })
                    })
                    .await
                    .unwrap_or_else(|e| Err(anyhow::anyhow!("codingplan login task failed: {e}")))
                };
                match outcome {
                    Ok(report) => {
                        print!("{}", report);
                    }
                    Err(e) => {
                        eprintln!("login setup failed: {:#}", e);
                    }
                }
                println!("\n  Starting AtomCode...\n");
                HEADLESS_MODE.store(false, Ordering::Relaxed);
                // Fall through to TUI startup below
            }
            Commands::Daemon {
                port,
                client,
                idle_timeout,
            } => {
                HEADLESS_MODE.store(true, Ordering::Relaxed);
                eprintln!("Starting AtomCode daemon on port {}...", port);
                eprintln!("Press Ctrl+C to stop.");
                // Run the bundled server IN-PROCESS (same `run_server` the webui uses),
                // instead of re-exec'ing into a separate `atomcode-daemon` binary that
                // may not be installed. This is an equivalent daemon entrypoint to the
                // standalone `atomcode-daemon` binary, so it MUST enforce the local token
                // identically (mirror of `atomcode-daemon/src/main.rs`): mint/resolve a
                // token, enable enforcement, and write `~/.atomcode/daemon-<port>.json`.
                // Otherwise `atomcode daemon` would be an unauthenticated bypass of the
                // very surface the token guards.
                let idle = idle_timeout
                    .or_else(|| {
                        std::env::var("ATOMCODE_DAEMON_IDLE_TIMEOUT")
                            .ok()
                            .and_then(|s| s.parse().ok())
                    })
                    .unwrap_or(30 * 60);
                let startup_mode = match client.as_deref() {
                    Some("vscode") => atomcode_telemetry::SessionMode::Vscode,
                    Some("jetbrains") => atomcode_telemetry::SessionMode::Jetbrains,
                    Some("webui") => atomcode_telemetry::SessionMode::Webui,
                    Some("atomcode-air") => atomcode_telemetry::SessionMode::AtomcodeAir,
                    _ => atomcode_telemetry::SessionMode::Ide,
                };
                let token_store = atomcode_daemon::auth_token::WebuiTokenStore::new();
                let daemon_token = atomcode_daemon::resolve_daemon_token(
                    std::env::var("ATOMCODE_DAEMON_TOKEN").ok(),
                    &token_store,
                );
                let res = atomcode_daemon::run_server(atomcode_daemon::ServerOpts {
                    host: "127.0.0.1".to_string(),
                    port,
                    cli_override: CliOverride {
                        disabled: cli.no_telemetry,
                    },
                    idle_timeout_secs: idle,
                    startup_mode,
                    webui_tokens: Some(token_store),
                    quiet: false,
                    working_dir_override: None,
                    prebound_listener: None,
                    app_user_id: None,
                    daemon_token_file: Some(daemon_token),
                })
                .await;
                telemetry
                    .shutdown(std::time::Duration::from_millis(500))
                    .await;
                if let Err(e) = res {
                    eprintln!("Fatal: daemon server error: {e:#}");
                    return Ok(1);
                }
                return Ok(0);
            }
            Commands::Webui { port, host } => {
                HEADLESS_MODE.store(true, Ordering::Relaxed);
                let msg = atomcode_daemon::ensure_server_and_open(&host, port, false).await;
                eprintln!("{msg}");
                // server 是后台 task；保持进程存活直到用户 Ctrl+C
                let _ = tokio::signal::ctrl_c().await;
                // Shutdown telemetry after Ctrl+C.
                telemetry
                    .shutdown(std::time::Duration::from_millis(500))
                    .await;
                return Ok(0);
            }
            Commands::Telemetry { action } => {
                HEADLESS_MODE.store(true, Ordering::Relaxed);
                let config_file_path = Config::default_path();
                match action {
                    TelemetryAction::Status => {
                        telemetry_cmd::status(&atomcode_dir, &telemetry_cfg)?
                    }
                    TelemetryAction::Enable => telemetry_cmd::enable(&config_file_path)?,
                    TelemetryAction::Disable => {
                        telemetry_cmd::disable(&config_file_path, &telemetry).await?
                    }
                    TelemetryAction::Dump { last, pretty } => {
                        telemetry_cmd::dump(&atomcode_dir, last, pretty)?
                    }
                    TelemetryAction::Clear => telemetry_cmd::clear(&atomcode_dir)?,
                    TelemetryAction::Recover => telemetry_cmd::recover(&atomcode_dir)?,
                }
                // Flush telemetry before exiting.
                telemetry
                    .shutdown(std::time::Duration::from_millis(500))
                    .await;
                return Ok(0);
            }
            Commands::Setup { force } => {
                HEADLESS_MODE.store(true, Ordering::Relaxed);
                let exit_code = run_setup_command(force);
                telemetry
                    .shutdown(std::time::Duration::from_millis(500))
                    .await;
                return Ok(exit_code);
            }
            Commands::Acp => {
                // stdout is the ACP JSON-RPC channel — no banner or diagnostic output here.
                HEADLESS_MODE.store(true, Ordering::Relaxed);
                // Load config the same way the TUI path does so provider/model resolution
                // is identical (honors --provider, --model, and config.toml).
                let config_path = cli.config.clone().unwrap_or_else(Config::default_path);
                let mut config = if config_path.exists() {
                    Config::load(&config_path).unwrap_or_default()
                } else {
                    Config::default()
                };
                apply_cli_runtime_overrides(
                    &mut config,
                    cli.provider.as_deref(),
                    cli.model.as_deref(),
                );
                let working_dir = resolve_working_dir(cli.dir.clone());
                let runtime_cfg = runtime_config_from(
                    &config,
                    &working_dir,
                    cli.provider.as_deref(),
                    // No telemetry injection into ACP sessions (each is independent).
                    None,
                    cli.dangerously_skip_permissions,
                    // ACP sessions are interactive: approval prompts park until the
                    // client answers (not fail-closed like headless -p).
                    true,
                );
                // Honor `--dangerously-skip-permissions`: auto-approve kernel approval
                // requests in the turn loop instead of round-tripping to the client.
                let auto_approve = runtime_cfg.dangerously_skip_permissions;
                // The factory creates and binds a distinct authenticated provider for
                // every ACP session. Sharing one pre-built provider would pin gateway
                // session affinity to whichever session bound it first.
                let provider_factory = atomcode_daemon::coding_provider_factory();
                let engine = atomcode::acp::engine::EngineConfig::from_coding_config(
                    runtime_cfg.agent_config(),
                );
                // Flush telemetry before the long-running stdio loop.
                telemetry
                    .shutdown(std::time::Duration::from_millis(500))
                    .await;
                return atomcode::acp::serve_stdio(atomcode::acp::AcpServeOptions {
                    engine: Some(engine),
                    provider_factory: Some(provider_factory),
                    auto_approve,
                })
                .await
                .map(|_| 0);
            }
            Commands::Schedule(sub) => {
                // Handled here (not via the generic `other` arm) so the executor's
                // exit code survives: `schedule run` returns 0/1/130 and the OS
                // scheduler keys failure detection on it. The generic arm collapses
                // every Ok to 0, which would report every scheduled run as success.
                HEADLESS_MODE.store(true, Ordering::Relaxed);
                let code = schedule_cmd::handle_schedule(sub).await?;
                telemetry
                    .shutdown(std::time::Duration::from_millis(500))
                    .await;
                return Ok(code);
            }
            other => {
                let result = handle_command(other, &telemetry).await.map(|_| 0);
                // Flush any events emitted by the subcommand (e.g. login_success)
                // before the process exits. Bounded by the same 500ms budget as
                // other exit paths.
                telemetry
                    .shutdown(std::time::Duration::from_millis(500))
                    .await;
                return result;
            }
        }
    }

    // Default: start TUI

    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);

    // FIRST-RUN seed for offline / managed deploys (e.g. a government intranet
    // that ships a bundled default config): if the user has no config yet and a
    // `--seed-config <path>` (or `ATOMCODE_SEED_CONFIG` env) source is given, copy
    // it into place once. No-op when a config already exists, so it's safe for the
    // launcher to always pass. Any failure is non-fatal → normal onboarding.
    let seed_source = cli.seed_config.clone().or_else(|| {
        std::env::var_os("ATOMCODE_SEED_CONFIG")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
    });
    match atomcode_config::config::Config::seed_user_config(&config_path, seed_source.as_deref()) {
        atomcode_config::config::SeedOutcome::Seeded => {
            if let Some(src) = seed_source.as_deref() {
                eprintln!(
                    "[seed] initialized {} from {}",
                    config_path.display(),
                    src.display()
                );
            }
        }
        atomcode_config::config::SeedOutcome::Invalid(e) => {
            eprintln!("Warning: --seed-config ignored (not a valid config): {e}");
        }
        atomcode_config::config::SeedOutcome::IoError(e) => {
            eprintln!("Warning: --seed-config could not be applied: {e}");
        }
        // AlreadyConfigured / NoSource → nothing to do, stay quiet.
        _ => {}
    }

    let config_load_start = std::time::Instant::now();
    let (mut config, config_startup_notice) = if config_path.exists() {
        match Config::load_with_diagnostics(&config_path) {
            Ok((config, warnings)) if warnings.is_empty() => (config, None),
            Ok((config, warnings)) => {
                let warning_list = warnings
                    .iter()
                    .map(|warning| format!("  - {warning}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let notice = format!(
                    "Some provider sections in {} could not be loaded:\n{}",
                    config_path.display(),
                    warning_list
                );
                eprintln!("Warning: {notice}");
                (config, Some(notice))
            }
            Err(error) => {
                let notice = format!(
                    "Failed to load {} ({error}); using default configuration.",
                    config_path.display()
                );
                eprintln!("Warning: {notice}");
                (Config::default(), Some(notice))
            }
        }
    } else {
        // No config yet — TUI Welcome screen will guide first-run setup
        (Config::default(), None)
    };
    tracing::info!(
        target: "atomcode::startup",
        stage = "config_load",
        elapsed_ms = config_load_start.elapsed().as_millis() as u64,
        total_ms = run_start.elapsed().as_millis() as u64,
        "startup stage completed"
    );
    atomcode_config::proxy::apply_process_proxy_config(&config.network.proxy);

    // ── i18n locale ──
    // Locale was already pre-resolved above (before clap parse) so --help
    // could be localised. Re-resolve with the full config (which may specify
    // a language key) to honour config-over-env priority.
    if config.language.is_some() {
        let locale =
            atomcode_tuix::i18n::resolve_initial_locale(cli.lang.as_deref(), config.language);
        atomcode_tuix::i18n::set_locale(locale);
    }

    // ── Plugin marketplace bootstrap + post-upgrade refresh ──
    //
    // Two best-effort hooks (auto-install default skills marketplace
    // on first startup, `git pull` every installed marketplace after a
    // self-upgrade) used to fire here synchronously, blocking the
    // input box for 1–3s on a warm path (and 5–10s on first clone).
    // Both now run as a detached `spawn_blocking` from inside
    // `atomcode_tuix::run` after the skill registry is constructed —
    // see lib.rs near `spawn_plugin_bootstrap`. Newly-installed skills
    // are picked up by a `skill_registry.reload()` + wake pulse the
    // background task fires on completion, so the slash menu refreshes
    // without a restart.

    apply_cli_runtime_overrides(&mut config, cli.provider.as_deref(), cli.model.as_deref());

    let working_dir = resolve_working_dir(cli.dir.clone());

    // Determine if we're running in headless mode BEFORE loading MCP.
    // Headless mode requires MCP tools immediately; TUI can load them in background.
    let is_headless = cli.prompt.is_some() || cli.prompt_file.is_some();

    // Continue the previous session only when the user explicitly opts
    // in via `-c` / `--continue`. Bare `atomcode` starts a fresh
    // session — no auto-resume, no scrollback replay. Users who want to
    // pick a specific older session can still use `/resume` inside the
    // TUI.
    let resume_session_id = if cli.continue_last {
        atomcode_daemon::legacy_convert::catalog_for_project(&working_dir)?
            .into_iter()
            .find(|entry| entry.message_count > 0)
            .map(|entry| entry.id)
    } else {
        None
    };

    // The native runtime builds its own provider + tools from
    // config, so the `tool_registry`/`tool_context` assembled above are no longer
    // wired to an agent loop; they remain constructed (unchanged lifetime) pending
    // a follow-up cleanup.
    let mut runtime_cfg = runtime_config_from(
        &config,
        &working_dir,
        cli.provider.as_deref(),
        Some(telemetry.clone()),
        cli.dangerously_skip_permissions,
        // Interactive (TUI) ⇒ approvals park until answered; headless (`-p`) keeps the
        // fail-closed timeout so an unanswered approval can't park the run forever.
        !is_headless,
    );
    runtime_cfg.next_prompt_suggestions = !is_headless;
    let model_name = runtime_cfg.model.clone();
    let provider_bootstrap = if is_headless {
        atomcode_coding::ProviderBootstrap::Required
    } else {
        interactive_provider_bootstrap(&runtime_cfg)
    };
    let runtime_start = std::time::Instant::now();
    let (native_runtime, native_coding_cfg, continued_session) = spawn_native_cli_runtime(
        &runtime_cfg,
        resume_session_id,
        provider_bootstrap,
        !is_headless,
        // TUI-only: the interactive checkpoint replaces the hard round-cap
        // error. Headless (`-p`) keeps the fail-closed hard error (no picker).
        !is_headless,
    )
    .await?;
    tracing::info!(
        target: "atomcode::startup",
        stage = "runtime_start",
        elapsed_ms = runtime_start.elapsed().as_millis() as u64,
        total_ms = run_start.elapsed().as_millis() as u64,
        "startup stage completed"
    );
    // TUI replay remains a presentation projection during S4; runtime resume above
    // has already converged and loaded the native snapshot under one lease.
    let resume_project_bucket =
        atomcode_capabilities::session::SessionManager::project_hash(&working_dir);
    let session_to_continue = match continued_session
        .as_ref()
        .map(|session| session.id.as_str())
    {
        Some(id) => atomcode_daemon::legacy_convert::load_catalog_session_view_in_project(
            &resume_project_bucket,
            id,
        )?,
        None => None,
    }
    .map(atomcode_tuix::session::Session::from_catalog_view)
    .transpose()?;
    let session_startup_notice = continued_session
        .as_ref()
        .and_then(|session| {
            session
                .forked_from
                .as_deref()
                .map(|source_id| (source_id, session.id.as_str()))
        })
        .map(|(source_id, fork_id)| {
            atomcode_tuix::i18n::t(atomcode_tuix::i18n::Msg::SessionBusyForked {
                source_id,
                fork_id,
            })
            .into_owned()
        });
    let startup_notice = merge_startup_notices(config_startup_notice, session_startup_notice);
    let (mut native_headless_runtime, mut native_tui_runtime) = if is_headless {
        (Some(native_runtime), None)
    } else {
        (None, Some((native_runtime, native_coding_cfg)))
    };

    // Spawner for in-TUI session switches (/session, /bg, disk /resume): each one
    // builds a fresh native runtime from the CURRENT in-process config. A launch-time
    // `--provider` is applied to that config above, so the override remains authoritative
    // for live views and subsequent runtime respawns without being persisted to disk.
    let runtime_spawn_override: atomcode_tuix::RuntimeSpawnOverride = {
        let tel = telemetry.clone();
        // Capture the bypass flag so in-TUI re-spawns also honor
        // --dangerously-skip-permissions — not just the launch handle.
        let skip_perms = cli.dangerously_skip_permissions;
        std::sync::Arc::new(
            move |config: &atomcode_config::config::Config,
                  working_dir: &std::path::Path,
                  session: &atomcode_tuix::session::Session| {
                let mut runtime_cfg = runtime_config_from(
                    config,
                    working_dir,
                    None,
                    Some(tel.clone()),
                    skip_perms,
                    // In-TUI re-spawns (/session, /bg, /resume) are always interactive.
                    true,
                );
                // TUI-only: a `max_rounds` hit becomes the interactive
                // continue/stop checkpoint (the render arm lives in the TUI
                // event loop). `spawn_deferred_tui_runtime` is only ever the
                // in-TUI respawn factory, so this is never a headless path.
                runtime_cfg.round_cap_checkpoint = true;
                runtime_cfg.next_prompt_suggestions = true;
                spawn_deferred_tui_runtime(runtime_cfg, session)
            },
        )
    };

    // Resolve effective prompt: --prompt-file reads from disk; -p is inline.
    // clap's conflicts_with ensures `-p` and `--prompt-file` can't both be given.
    let effective_prompt: Option<String> = match (cli.prompt.as_ref(), cli.prompt_file.as_ref()) {
        (Some(p), None) => Some(p.clone()),
        (None, Some(path)) => match std::fs::read_to_string(path) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!(
                    "error: failed to read --prompt-file {}: {}",
                    path.display(),
                    e
                );
                std::process::exit(2);
            }
        },
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents this"),
    };

    // Build the session-scope context: repo_origin, mode.
    // session_id and account_id are managed on Telemetry directly via
    // set_session_id() / set_account_id(). Seed account_id from stored auth so
    // events from this session correlate to the user even before any explicit
    // login action this run; login()/logout() update it later as needed.
    // mode: Headless when a prompt is supplied (-p / --prompt-file);
    //       Tui when the user launches the interactive terminal UI.
    let repo = atomcode_telemetry::detect_repo_origin(
        &std::env::current_dir().unwrap_or_else(|_| working_dir.clone()),
    );
    telemetry.set_account_id(auth::get_stored_auth().map(|a| a.user.id.to_string()));
    let session_mode = if effective_prompt.is_some() {
        SessionMode::Headless
    } else {
        SessionMode::Tui
    };
    // Launch-level fallback: any telemetry emitted outside a mode-bearing
    // CurrentContext scope (e.g. an un-scoped spawned task) attributes to this
    // process mode instead of `null`. Per-scope mode still overrides it.
    telemetry.set_default_mode(Some(session_mode));
    // Bind telemetry to the continued session's id (if any). A fresh run needs
    // nothing here: the agent bootstraps telemetry + header + datalog from its
    // own session id. The TUI manages its own binding via
    // `bind_telemetry_to_session`.
    if let Some(ref s) = session_to_continue {
        if let Ok(uuid) = uuid::Uuid::parse_str(s.id.as_str()) {
            telemetry.set_session_id(uuid);
        }
    }
    let scope_ctx = CurrentContext {
        repo_origin: Some(repo),
        mode: Some(session_mode),
        ..CurrentContext::current()
    };

    let result = CurrentContext::scope(scope_ctx, || async {
        // Emit open_atomcode once at agent-flow entry. Meta-commands
        // (--version, --help, --update, login, logout, status, upgrade,
        // rollback, telemetry) return via handle_command before reaching
        // this point and must NOT emit open_atomcode.
        telemetry.track(Event::OpenAtomcode {
            dangerously_skip_permissions: cli.dangerously_skip_permissions,
        });

        // Headless mode: -p / --prompt-file triggers non-interactive execution.
        let exit_code = if let Some(prompt) = effective_prompt {
            let verbose = cli.verbose || force_verbose;
            let capture = false;
            // Don't `?`-propagate here: an error must still fall through to the
            // telemetry.shutdown() below, otherwise this session's un-drained
            // mpsc events are lost. Capture the Result and let it bubble up only
            // *after* the flush. The run routes through the native runtime handle.
            let notifications_cfg = config.notifications.clone();
            let engine_runtime = native_headless_runtime
                .take()
                .expect("native headless runtime built above");
            match run_native_headless(
                notifications_cfg,
                engine_runtime,
                prompt,
                cli.provider.as_deref(),
                verbose,
                capture,
                working_dir.clone(),
                cli.dangerously_skip_permissions,
                is_admin,
                false, // strict_unattended=false: preserve -p behaviour exactly
            )
            .await
            {
                Err(e) => Err(e),
                Ok((ec, _captured)) => Ok::<i32, anyhow::Error>(ec),
            }
        } else {
            // Fire-and-forget: spawn a setsid'd subprocess to stage the next
            // release if one is out. Detached so a Ctrl+C in this parent doesn't
            // also kill the download — that was the whole reason "exit and come
            // back" wasn't picking up v_next on short sessions. Only armed when
            // the user hasn't opted out via `auto_update = false` AND we're not
            // running as `atomcode.bak` (backup should stay pinned; see the
            // `is_running_as_backup` guard up top).
            // In distro-pm (HarmonyBrew) builds the package manager owns
            // upgrades, so skip spawning the detached prep process entirely —
            // `prepare_deferred_upgrade` would no-op anyway.
            if config.auto_update
                && !is_running_as_backup()
                && !cli.dev
                && !atomcode_updater::is_package_managed()
                // Skip when the startup synchronous path already checked (and applied,
                // if newer) this launch — otherwise both fetch `latest.json`. The
                // detached stager stays as the fallback for launches where the sync
                // path was skipped (e.g. ATOMCODE_PLAIN).
                && !SYNC_UPGRADE_CHECKED.load(Ordering::Relaxed)
            {
                spawn_detached_upgrade_prep();
            }

            // Redirect fd 2 → $ATOMCODE_HOME/stderr.log before the TUI takes
            // ownership of the terminal. NSPasteboard deprecation warnings
            // (arboard clipboard polling, ~1.5 s interval) and any other
            // rogue C-lib stderr writes would otherwise land at the raw-mode
            // cursor position, painting into the input box.
            //
            // Only fires here — the TUI branch. Headless (-p/--prompt-file)
            // leaves stderr pointing at the real terminal so the user sees
            // actual errors in their shell/CI output.
            redirect_stderr_to_log_file();

            // The runtime task already runs the engine behind this handle; the TUI
            // drives it. In-TUI /session and /resume spawn via
            // runtime_spawn_override.
            let (runtime, coding_cfg) = native_tui_runtime
                .take()
                .expect("native TUI runtime built above");
            let provider_selection = coding_cfg.provider_name.clone();
            let tui_runtime = into_tui_native_runtime(runtime, coding_cfg);
            // Same as the headless arm: don't `?` — a TUI run that ends in an
            // error must still reach the shutdown/flush below. Ok(()) → exit 0;
            // the error propagates only after telemetry is drained.
            // A running session owns its resolved provider/model. Shared config
            // changes only define the default for sessions opened afterwards;
            // they must not retarget an already-open runtime.
            let provider_selection_mode = atomcode_tuix::ProviderSelectionMode::Pinned;
            tracing::info!(
                target: "atomcode::startup",
                stage = "tui_enter",
                total_ms = run_start.elapsed().as_millis() as u64,
                "handing control to TUI"
            );
            match atomcode_tuix::run(
                config,
                provider_selection,
                model_name,
                provider_selection_mode,
                atomcode_config::ConfigStore::new(config_path.clone()),
                tui_runtime,
                runtime_spawn_override,
                working_dir,
                session_to_continue,
                startup_notice,
                telemetry.clone(),
                cli.dangerously_skip_permissions,
                is_admin,
            )
            .await
            {
                Ok(()) => Ok(0),
                Err(e) => Err(e.into()),
            }
        };

        // Flush telemetry on EVERY exit path — Ok and Err alike. Both session
        // arms above return their Result into `exit_code` instead of using `?`,
        // so an errored TUI/headless run still drains the in-memory mpsc queue
        // here before the error bubbles up to async_main's exit(1). Without this
        // the tail of any session that ended in an error was silently dropped.
        telemetry
            .shutdown(std::time::Duration::from_millis(500))
            .await;
        exit_code
    })
    .await;

    result
}

fn spawn_deferred_tui_runtime(
    cfg: atomcode_coding::CodingRuntimeConfig,
    session: &atomcode_tuix::session::Session,
) -> atomcode_tuix::SpawnedRuntime {
    let session_id = session.id.as_str().to_string();
    let snapshot = session.to_conversation_snapshot();
    // Base agent config for the VL preprocessor's one-off provider builds,
    // derived from this runtime's config before `cfg` is moved into the spawn.
    let vl_base = cfg.agent_config();
    let (native_control, mut events, runtime_state) =
        atomcode_daemon::spawn_native_runtime_for_session_deferred_with_preprocessor(
            cfg,
            session_id.clone(),
            snapshot,
            Some(std::sync::Arc::new(
                crate::vision::VlImagePreprocessor::new(
                    atomcode_daemon::coding_provider_factory(),
                    vl_base,
                ),
            )),
        );
    let control = atomcode_tuix::RuntimeControl::deferred(native_control, runtime_state);
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if event_tx
                .send(atomcode_tuix::RuntimeEventPayload::SequencedNative(event))
                .is_err()
            {
                break;
            }
        }
    });
    atomcode_tuix::SpawnedRuntime {
        endpoint: atomcode_tuix::RuntimeEndpoint { native: control },
        event_rx,
        session_id: Some(session_id),
    }
}

fn into_tui_native_runtime(
    runtime: atomcode_coding::CodingRuntime,
    _coding_cfg: atomcode_coding::CodingAgentConfig,
) -> atomcode_tuix::SpawnedRuntime {
    let session_id = runtime.session.as_ref().map(|session| session.id.clone());
    let atomcode_coding::CodingRuntime {
        handle,
        mut events,
        task,
        ..
    } = runtime;
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let control =
        atomcode_tuix::RuntimeControl::ready_with_event_tx(handle.clone(), event_tx.clone());
    let control_for_events = control.clone();
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if event_tx
                .send(atomcode_tuix::RuntimeEventPayload::SequencedNative(event))
                .is_err()
            {
                break;
            }
        }
        let _ = task.await;
        control_for_events.detach_delivery_event_tx();
    });
    atomcode_tuix::SpawnedRuntime {
        endpoint: atomcode_tuix::RuntimeEndpoint { native: control },
        event_rx,
        session_id,
    }
}

/// On macOS the NSPasteboard runtime prints deprecation warnings to
/// stderr when arboard calls into AppKit (via clipboard polling for
/// the "ctrl+v to paste image" hint). In raw mode, stderr shares the
/// TTY with the TUI paint stream, so those warnings paint into the
/// input box at whatever cursor row happens to be active. Other libs
/// (LSP, MCP shells) can leak the same way.
///
/// Redirect fd 2 to `$ATOMCODE_HOME/stderr.log` once we know we're
/// entering interactive TUI mode. plain / headless / piped paths
/// don't call this — they want stderr to reach the terminal so the
/// user sees real errors.
///
/// Best-effort: if the home dir can't be created or the file can't
/// be opened, do nothing and let stderr leak (the original bug); we
/// don't want to take down atomcode startup because logging failed.
#[cfg(unix)]
fn redirect_stderr_to_log_file() {
    use std::os::unix::io::AsRawFd;
    let Some(home) = std::env::var_os("ATOMCODE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".atomcode")))
    else {
        return;
    };
    if std::fs::create_dir_all(&home).is_err() {
        return;
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("stderr.log"))
    else {
        return;
    };
    // Write a session marker so users can see in stderr.log where
    // each atomcode session starts — helps separate one run's noise
    // from another's when grepping for actual problems.
    // Use epoch seconds (std::time only — no chrono dep needed).
    let epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let marker = format!("\n--- atomcode session start (unix={epoch_secs}) ---\n");
    let _ = std::io::Write::write_all(&mut std::io::BufWriter::new(&file), marker.as_bytes());
    // SAFETY: dup2 swaps the file descriptor table entry for fd 2
    // to point at `file`'s underlying fd. This is a standard, safe
    // operation; the worst case (dup2 fails) is the redirect doesn't
    // happen and we log nothing — same as the no-redirect baseline.
    unsafe {
        libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
    }
    // Intentionally keep `file` alive via the dup2 — the kernel
    // holds a reference to the underlying inode, so even after
    // `file` is dropped, fd 2 stays pointing at the same file.
    // No need to std::mem::forget.
}

#[cfg(not(unix))]
fn redirect_stderr_to_log_file() {
    // Windows: NSPasteboard is mac-only; arboard on Windows uses
    // OpenClipboard which doesn't NSLog. Not a known leak path.
    // No-op for now; revisit if a similar Windows issue surfaces.
}

/// The persistent tracing log path: `<config_dir>/logs/atomcode.log`. Pure so the
/// join rule is unit-testable; the config dir is resolved by `Config::config_dir()`
/// (which is `ATOMCODE_HOME`- AND sudo-aware via `real_home_dir`), so the log lands
/// next to config/sessions instead of diverging under `sudo` — plain `dirs::home_dir()`
/// there points at root's home, where the user would never find the log.
fn atomcode_log_path(config_dir: std::path::PathBuf) -> std::path::PathBuf {
    config_dir.join("logs").join("atomcode.log")
}

/// The default `RUST_LOG` directive when the env var is unset: `info` for everything,
/// with the chatty transport crates pinned to `warn` so the log stays about atomcode.
const DEFAULT_LOG_DIRECTIVES: &str =
    "info,hyper=warn,hyper_util=warn,h2=warn,rustls=warn,reqwest=warn,tower=warn,mio=warn";

/// Roll size cap for the tracing log. Above this the live file is rotated to a single
/// `.old` generation, bounding on-disk usage at ~2× this (an always-on `info` log
/// would otherwise append forever across every session).
const LOG_ROTATE_BYTES: u64 = 5 * 1024 * 1024;

/// If the log already exceeds [`LOG_ROTATE_BYTES`], move it aside to `<path>.old`
/// (single generation) so the live file restarts small. Best-effort; errors ignored.
fn rotate_log_if_large(path: &std::path::Path) {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > LOG_ROTATE_BYTES {
        let mut old = path.as_os_str().to_owned();
        old.push(".old"); // atomcode.log -> atomcode.log.old
        let _ = std::fs::rename(path, std::path::PathBuf::from(old));
    }
}

/// Install a global tracing subscriber that writes to `<config_dir>/logs/atomcode.log`.
///
/// The whole workspace emits `tracing::` diagnostics but historically installed NO
/// subscriber, so every line (including the `atomcode-label:` middleware trace) went
/// to the no-op dispatcher and vanished. This wires them to a file.
///
/// FILE-ONLY BY DESIGN: the TUI owns the terminal, and the stderr redirect only runs
/// in the detached-daemon path — writing tracing output to real stderr would corrupt
/// the interactive display. So we always write to our own file handle, never stderr.
///
/// Fail-open: any error (can't create dir/file, subscriber already set) leaves the
/// process running with logging simply disabled. Called once, early.
fn init_file_logging() {
    use std::io::Write as _;
    let path = atomcode_log_path(Config::config_dir());
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    rotate_log_if_large(&path);
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    // Session marker so users can tell one run's lines from another's when grepping.
    let epoch_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(file, "--- atomcode session start (unix={epoch_secs}) ---");

    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| tracing_subscriber::EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_DIRECTIVES));

    // `Mutex<File>` is tracing-subscriber's OWN built-in `MakeWriter`: its
    // `MutexGuardWriter` holds the lock for the WHOLE formatted event, so concurrent
    // traces from tokio workers can't interleave mid-line (a per-`write()` re-lock
    // would). No custom writer needed.
    let _ = tracing_subscriber::fmt()
        .with_ansi(false) // file, not a terminal
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .try_init(); // Err only if a subscriber is already set — fine, ignore.
}

/// Apply launch-time provider/model overrides to the process-owned config.
///
/// The runtime already resolves `--provider` directly, but TUI respawns and live views
/// read `config.default_provider`. Keeping those two sources different makes the footer
/// correct while synchronized WebUI tabs expose and reload the wrong provider. This is
/// deliberately in-memory only; persistence remains owned by `/model` and WebUI settings.
fn apply_cli_runtime_overrides(
    config: &mut atomcode_config::config::Config,
    provider_override: Option<&str>,
    model_override: Option<&str>,
) {
    let provider_name = provider_override
        .map(str::to_string)
        // Fall back to the canonical active selection (new-schema `default_model`
        // then legacy `default_provider`), not `default_provider` alone.
        .or_else(|| config.effective_model_selection())
        .unwrap_or_default();
    // Route the optional `--model` override to whichever schema holds the
    // selection (legacy `[providers.*]` or new-schema `[models.*]`); bail if the
    // selection is unknown so `--provider bogus` is a no-op as before.
    if let Some(model) = model_override {
        if let Some(p) = config.providers.get_mut(&provider_name) {
            p.model = model.to_string();
        } else if let Some(m) = config.models.get_mut(&provider_name) {
            m.model = model.to_string();
        } else {
            return;
        }
    } else if !config.selection_exists(&provider_name) {
        return;
    }
    if provider_override.is_some() {
        // `default_model` is canonical (`effective_model_selection` prefers it);
        // sync the legacy field too so a new-schema override takes effect.
        config.default_model = Some(provider_name.clone());
        config.default_provider = provider_name;
    }
}

/// Derive the coding runtime config from the current config + working dir.
/// Shared by the initial runtime and the in-TUI runtime-spawn override so
/// both resolve the provider identically.
///
/// `provider_override` is the `--provider` flag: it must flow through the SAME
/// `active_provider` resolution (honor the override, fall back when the default
/// points to a deleted section). Reading `default_provider` directly silently
/// ignored `--provider`, so a headless `--provider X` run picked the config
/// default instead of X.
pub(crate) fn runtime_config_from(
    config: &atomcode_config::config::Config,
    working_dir: &std::path::Path,
    provider_override: Option<&str>,
    telemetry: Option<std::sync::Arc<atomcode_telemetry::Telemetry>>,
    dangerously_skip_permissions: bool,
    interactive: bool,
) -> atomcode_coding::CodingRuntimeConfig {
    let mut runtime = atomcode_coding::CodingRuntimeConfig::from_config(
        config,
        working_dir,
        provider_override,
        telemetry,
        dangerously_skip_permissions,
        interactive,
    );
    // The process locale has already resolved CLI `--lang` > config > env.
    runtime.preferred_language = Some(atomcode_tuix::i18n::current_locale());
    runtime
}

/// Select the interactive startup mode through the same unified provider/model
/// resolution result that will be assembled by the runtime.
///
/// Inspecting the raw config here would either see only the legacy schema or
/// repeat resolution with subtly different fallback behavior. `runtime_cfg`
/// already owns the exact resolved provider/model for this spawn.
fn interactive_provider_bootstrap(
    runtime_cfg: &atomcode_coding::CodingRuntimeConfig,
) -> atomcode_coding::ProviderBootstrap {
    if !runtime_cfg.model.is_empty() {
        atomcode_coding::ProviderBootstrap::RecoverAuthentication
    } else {
        atomcode_coding::ProviderBootstrap::Unavailable(
            atomcode_coding::ProviderUnavailableReason::NotConfigured,
        )
    }
}

pub(crate) async fn spawn_native_cli_runtime(
    cfg: &atomcode_coding::CodingRuntimeConfig,
    resume_session_id: Option<String>,
    bootstrap: atomcode_coding::ProviderBootstrap,
    fork_on_session_in_use: bool,
    // TUI-only opt-in: turn a `max_rounds` hit into the interactive
    // continue/stop checkpoint. The checkpoint render arm lives in the TUI
    // event loop, so headless (`-p`) callers pass `false` — otherwise the
    // kernel would emit a checkpoint Request with no requester and fail-closed.
    round_cap_checkpoint: bool,
) -> anyhow::Result<(
    atomcode_coding::CodingRuntime,
    atomcode_coding::CodingAgentConfig,
    Option<ContinuedCliSession>,
)> {
    let mut agent = cfg.agent_config();
    agent.round_cap_checkpoint = round_cap_checkpoint;
    let (session, imported_lease, continued_session) = match resume_session_id {
        Some(id) => {
            let manager =
                atomcode_capabilities::session::SessionManager::for_project(&agent.working_dir);
            match manager.acquire_lease(&id) {
                Ok(lease) => {
                    atomcode_daemon::legacy_convert::converge_session(&manager, &lease)?;
                    (
                        atomcode_coding::SessionMode::Resume(id.clone()),
                        Some(lease),
                        Some(ContinuedCliSession {
                            id,
                            forked_from: None,
                        }),
                    )
                }
                Err(error) if should_fork_busy_continue(fork_on_session_in_use, &error) => {
                    let fork_id = uuid::Uuid::new_v4().to_string();
                    let (forked, lease) = manager.fork_native_session(
                        &id,
                        &fork_id,
                        atomcode_capabilities::session::now_ms(),
                    )?;
                    (
                        atomcode_coding::SessionMode::Resume(forked.meta.id.clone()),
                        Some(lease),
                        Some(ContinuedCliSession {
                            id: forked.meta.id,
                            forked_from: Some(id),
                        }),
                    )
                }
                Err(error) => return Err(error.into()),
            }
        }
        None => (atomcode_coding::SessionMode::Fresh, None, None),
    };
    let prepare = atomcode_coding::PrepareOptions {
        subagents: atomcode_coding::SubagentPolicy::Enabled,
        session,
        plugin_skill_dirs: atomcode_daemon::gather_plugin_skill_dirs_for(&cfg.working_dir),
        mcp: cfg.mcp,
        rate_limit_source: Some(atomcode_daemon::coding_plan_rate_limit_source()),
        ..atomcode_coding::PrepareOptions::default()
    };
    let start = atomcode_coding::CodingRuntimeStart {
        agent: agent.clone(),
        prepare,
        provider_factory: atomcode_daemon::coding_provider_factory(),
        plugin_hooks: atomcode_daemon::installed_plugin_hook_source(),
        // Restore the TUI's VL image recognition (dropped when the legacy
        // bridge was retired): convert images to text for non-vision models
        // inside the async turn, so it never blocks the UI. See `vision`.
        image_preprocessor: Some(std::sync::Arc::new(
            crate::vision::VlImagePreprocessor::new(
                atomcode_daemon::coding_provider_factory(),
                agent.clone(),
            ),
        )),
    };
    let runtime = match imported_lease {
        Some(lease) => {
            atomcode_coding::CodingRuntime::start_with_session_lease(start, bootstrap, lease).await
        }
        None => atomcode_coding::CodingRuntime::start_with_bootstrap(start, bootstrap).await,
    }
    .map_err(anyhow::Error::new)?;
    if cfg.dangerously_skip_permissions {
        runtime
            .handle
            .set_mode(atomcode_coding::RuntimeMode::Auto)
            .await
            .map_err(anyhow::Error::new)?;
    }
    Ok((runtime, agent, continued_session))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContinuedCliSession {
    id: String,
    forked_from: Option<String>,
}

fn should_fork_busy_continue(
    fork_on_session_in_use: bool,
    error: &atomcode_capabilities::session::SessionStoreError,
) -> bool {
    fork_on_session_in_use
        && matches!(
            error,
            atomcode_capabilities::session::SessionStoreError::SessionInUse { .. }
        )
}

/// Decide whether to auto-approve a headless approval request.
///
/// A request only reaches this point when a gate (BashWorkspaceGate /
/// ApprovalMiddleware) already escalated the tool call — i.e. it is NOT
/// trivially safe.  `-p` (skip_permissions) blanket-approves bash; scheduled
/// runs (strict_unattended=true) refuse everything because no human is present
/// to vet a destructive or out-of-workspace command.
pub(crate) fn headless_auto_approve(
    strict_unattended: bool,
    skip_permissions: bool,
    tool: &str,
) -> bool {
    if strict_unattended {
        return false; // scheduled: deny everything that was escalated to approval
    }
    skip_permissions || tool == "bash" // -p: current behaviour unchanged
}

pub(crate) async fn run_native_headless(
    notifications_cfg: atomcode_config::config::NotificationConfig,
    runtime: atomcode_coding::CodingRuntime,
    prompt: String,
    _provider_name: Option<&str>,
    verbose: bool,
    capture: bool,
    working_dir: PathBuf,
    skip_permissions: bool,
    is_admin: bool,
    strict_unattended: bool,
) -> Result<(i32, Option<String>)> {
    use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse, APPROVAL_KIND};
    use atomcode_coding::{CodingRuntimeEvent, TurnCompletion, UserInput};
    use atomcode_kernel::event::{AgentEvent as KernelEvent, StopReason};

    HEADLESS_MODE.store(true, Ordering::Relaxed);
    if skip_permissions {
        eprintln!(
            "{}",
            atomcode_config::i18n::t(atomcode_config::i18n::Msg::BypassWarningHeadless)
        );
    }
    if is_admin {
        eprintln!(
            "{}",
            atomcode_config::i18n::t(atomcode_config::i18n::Msg::AdminWarningHeadless)
        );
    }

    let atomcode_coding::CodingRuntime {
        handle,
        mut events,
        task,
        ..
    } = runtime;
    handle
        .wait_mcp_ready(atomcode_capabilities::mcp::CONNECT_TIMEOUT)
        .await
        .map_err(anyhow::Error::new)?;
    handle
        .submit(UserInput::from(prompt))
        .await
        .map_err(anyhow::Error::new)?;
    let started = std::time::Instant::now();
    let mut captured = capture.then(String::new);
    let mut last_text_ended_with_newline = true;
    let mut tool_calls = 0usize;
    let mut total_tokens = 0usize;
    let mut rounds = 0usize;
    let mut had_denial = false;
    let mut exit_code = 0;
    let mut saw_turn_terminal = false;
    let mut thinking_line_open = false;

    fn close_native_thinking(open: &mut bool) {
        let mut output = String::new();
        close_thinking_chunk(&mut output, open);
        if !output.is_empty() {
            eprint!("{output}");
            let _ = io::stderr().flush();
        }
    }

    while let Some(envelope) = events.recv().await {
        match envelope.event {
            CodingRuntimeEvent::Agent(KernelEvent::TextDelta(text)) => {
                close_native_thinking(&mut thinking_line_open);
                if !text.is_empty() {
                    last_text_ended_with_newline = text.ends_with('\n');
                }
                if let Some(buffer) = captured.as_mut() {
                    buffer.push_str(&text);
                }
                print!("{text}");
                io::stdout().flush()?;
            }
            CodingRuntimeEvent::Agent(KernelEvent::Reasoning(text)) if verbose => {
                let mut output = String::new();
                format_thinking_chunk(&mut output, &mut thinking_line_open, &text);
                eprint!("{output}");
                let _ = io::stderr().flush();
            }
            CodingRuntimeEvent::Agent(KernelEvent::ToolStarted { call }) => {
                close_native_thinking(&mut thinking_line_open);
                tool_calls += 1;
                if verbose {
                    eprintln!(
                        "[tool→ {}] {}",
                        call.name,
                        truncate_log_line(&call.arguments, 120)
                    );
                }
            }
            CodingRuntimeEvent::Agent(KernelEvent::ToolResult { result }) if verbose => {
                close_native_thinking(&mut thinking_line_open);
                eprintln!(
                    "[tool← {}] {} chars",
                    if result.is_error { "error" } else { "ok" },
                    format_verbose_tool_chunk(&result.content).chars().count()
                );
            }
            CodingRuntimeEvent::Agent(KernelEvent::Usage(meta)) => {
                rounds += 1;
                total_tokens = total_tokens
                    .saturating_add((meta.tokens.prompt + meta.tokens.completion) as usize);
                if verbose {
                    eprintln!(
                        "[tokens] prompt={} completion={} cached={}",
                        meta.tokens.prompt, meta.tokens.completion, meta.tokens.cached
                    );
                }
            }
            CodingRuntimeEvent::Agent(KernelEvent::Error { message, .. }) => {
                close_native_thinking(&mut thinking_line_open);
                eprintln!("[error] {message}");
                exit_code = 1;
            }
            CodingRuntimeEvent::Agent(KernelEvent::Warning(message))
            | CodingRuntimeEvent::ControllerWarning(message)
            | CodingRuntimeEvent::PersistenceWarning(message) => {
                eprintln!("[warning] {message}")
            }
            CodingRuntimeEvent::Agent(KernelEvent::RateLimited {
                reset_at_display,
                reset_label,
                secs_until_reset,
                auto_resuming,
                server_message,
            }) => {
                let is_coding_plan = !reset_at_display.is_empty() || !reset_label.is_empty();
                if auto_resuming {
                    eprintln!(
                        "[rate-limited] auto-continuing in {}s…",
                        secs_until_reset.unwrap_or(0)
                    );
                } else if !is_coding_plan {
                    let reason = match server_message.as_deref() {
                        Some(message) if !message.trim().is_empty() => {
                            format!(" — {}", message.trim())
                        }
                        _ => String::new(),
                    };
                    match secs_until_reset {
                        Some(seconds) => eprintln!(
                            "[rate-limited] HTTP 429{reason} — retry later (in {seconds}s)"
                        ),
                        None => {
                            eprintln!("[rate-limited] HTTP 429{reason} — paused, retry later")
                        }
                    }
                } else if !reset_at_display.is_empty() {
                    eprintln!(
                        "[rate-limited] 5h window exhausted — resets around {reset_at_display}"
                    );
                } else if let Some(seconds) = secs_until_reset {
                    eprintln!(
                        "[rate-limited] 5h window exhausted — resets in {seconds}s, retry later"
                    );
                } else {
                    eprintln!("[rate-limited] 5h window exhausted — paused, retry later");
                }
            }
            CodingRuntimeEvent::Request(request) => {
                let response = if request.kind == APPROVAL_KIND {
                    serde_json::from_value::<ApprovalRequest>(request.payload)
                        .ok()
                        .map(|approval| {
                            if headless_auto_approve(
                                strict_unattended,
                                skip_permissions,
                                &approval.tool,
                            ) {
                                eprintln!("[headless] auto-approved {}", approval.tool);
                                ApprovalResponse::allow()
                            } else {
                                had_denial = true;
                                eprintln!(
                                    "[denied] {} requires interactive approval",
                                    approval.tool
                                );
                                ApprovalResponse::deny()
                            }
                        })
                } else {
                    None
                };
                let value = response
                    .and_then(|response| serde_json::to_value(response).ok())
                    .unwrap_or(serde_json::Value::Null);
                let _ = handle.respond(request.id, value).await;
            }
            CodingRuntimeEvent::CompactionFinished { completion } => {
                if let atomcode_coding::runtime::CompactionCompletion::Completed(outcome) =
                    completion
                {
                    if outcome.committed {
                        eprintln!(
                            "[compact] {}",
                            atomcode_config::i18n::format_compaction_mark(
                                outcome.removed_messages,
                                outcome.estimated_tokens_before,
                                outcome.estimated_tokens_after
                            )
                        );
                    }
                }
            }
            CodingRuntimeEvent::TurnFinished(completion) => {
                saw_turn_terminal = true;
                close_native_thinking(&mut thinking_line_open);
                let reason = match &completion {
                    TurnCompletion::Completed { reason, .. }
                    | TurnCompletion::SnapshotUnavailable { reason, .. } => *reason,
                };
                if !last_text_ended_with_newline {
                    println!();
                    io::stdout().flush()?;
                }
                atomcode_capabilities::notify::notify_turn_finished(
                    &notifications_cfg,
                    atomcode_capabilities::notify::TurnNotification {
                        duration: started.elapsed(),
                        turn_count: rounds,
                        tool_call_count: tool_calls,
                        total_tokens: Some(total_tokens),
                        stop_reason: headless_completion_notify_reason(&completion),
                        working_dir: Some(&working_dir),
                    },
                );
                if verbose {
                    eprintln!(
                        "[done] {:.1}s tokens={} turns={} tool_calls={}{}",
                        started.elapsed().as_secs_f64(),
                        atomcode_config::i18n::fmt_tokens(total_tokens),
                        rounds,
                        tool_calls,
                        match &completion {
                            TurnCompletion::Completed { .. } if reason == StopReason::Stopped =>
                                String::new(),
                            TurnCompletion::Completed { .. } => {
                                format!(" stopped={reason:?}")
                            }
                            TurnCompletion::SnapshotUnavailable { error, .. } => format!(
                                " completion=SnapshotUnavailable reason={reason:?} error={}",
                                error.message
                            ),
                        }
                    );
                }
                exit_code = headless_completion_exit_code(&completion, exit_code);
                break;
            }
            CodingRuntimeEvent::RuntimeStopped(_) => {
                exit_code = exit_code.max(1);
                break;
            }
            _ => {}
        }
    }
    if !saw_turn_terminal {
        exit_code = exit_code.max(1);
    }
    let _ = handle.shutdown().await;
    let _ = task.await;
    if exit_code == 0 && had_denial {
        exit_code = 2;
    }
    Ok((exit_code, captured))
}

/// Drive `atomcode_capabilities::setup::run` end-to-end and return the CLI exit code
/// (0 on success, 1 on any setup error). `setup::run` is synchronous; we
/// run it directly since `Commands::Setup` already runs outside the TUI loop.
fn run_setup_command(force: bool) -> i32 {
    use atomcode_capabilities::setup;

    let project_root = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("setup error: cannot read current directory: {e}");
            return 1;
        }
    };
    let mut opts = setup::RunOptions::new(project_root);
    opts.force = force;

    match setup::run(opts) {
        Ok(report) => {
            println!("{}", report.render_cli());
            0
        }
        Err(e) => {
            eprintln!("setup error: {e}");
            1
        }
    }
}

/// Handle subcommands (login, logout, status)
async fn handle_command(cmd: Commands, telemetry: &std::sync::Arc<Telemetry>) -> Result<()> {
    // Subcommands never enter TUI, so tell the panic hook to skip terminal
    // cleanup — otherwise `disable_raw_mode` panics on Windows with
    // "initial console mode not set" because raw mode was never enabled.
    HEADLESS_MODE.store(true, Ordering::Relaxed);

    match cmd {
        Commands::Login => {
            // `run()` intercepts Login (and its Codingplan alias) before
            // handle_command is called, running the full OAuth + setup
            // flow and falling through to the TUI. This arm is
            // unreachable in normal execution but kept defensive.
            unreachable!("Login is handled inline in run() before handle_command")
        }
        Commands::Logout => {
            auth::logout()?;
            telemetry.set_account_id(None);
            println!("  You have been logged out.");
            Ok(())
        }
        Commands::Status => {
            if let Some(auth) = auth::get_stored_auth() {
                println!(
                    "\n  Logged in as: {} ({})",
                    auth.user.username, auth.user.id
                );
                if let Some(name) = auth.user.name {
                    println!("  Name: {}", name);
                }
                if let Some(email) = auth.user.email {
                    println!("  Email: {}", email);
                }
                println!("  Auth file: {}\n", auth::auth_file_path().display());
            } else {
                println!("\n  Not logged in.");
                println!("  Run 'atomcode login' to authenticate.\n");
            }
            Ok(())
        }
        Commands::Upgrade { force } => run_upgrade_cli(force).await,
        Commands::Rollback => run_rollback_cli(),
        Commands::Uninstall {
            yes,
            purge,
            keep_data,
            dry_run,
        } => uninstall::run(uninstall::Args {
            yes,
            purge,
            keep_data,
            dry_run,
        }),
        Commands::Codingplan => {
            // Hidden alias for Login — `run()` intercepts both before
            // handle_command is called, so this arm is unreachable.
            unreachable!("Codingplan is handled inline in run() before handle_command")
        }
        Commands::Telemetry { .. } => {
            unreachable!("Telemetry is handled inline in run() before handle_command")
        }
        Commands::Daemon { .. } => {
            unreachable!("Daemon is handled inline in run() before handle_command")
        }
        Commands::Webui { .. } => {
            unreachable!("Webui is handled inline in run() before handle_command")
        }
        Commands::Setup { .. } => {
            unreachable!("Setup is handled inline in run() before handle_command")
        }
        Commands::Plugin(sub) => handle_plugin_cli(sub),
        Commands::Mcp(McpCli::Add {
            name,
            command,
            global,
            dir,
        }) => {
            let base = resolve_working_dir(dir);
            let path = if global {
                Config::config_dir().join("mcp.json")
            } else {
                base.join(".mcp.json")
            };
            let program = command
                .first()
                .expect("clap ensures at least one command token")
                .clone();
            let args: Vec<String> = command.into_iter().skip(1).collect();
            merge_stdio_mcp_server_into_json_file(&path, &name, &program, &args)?;
            println!(
                "  Added MCP server {:?} → {} (stdio: {} + {} arg(s))",
                name,
                path.display(),
                program,
                args.len()
            );
            Ok(())
        }
        Commands::Mcp(McpCli::AddGithubOauth { name, global, dir }) => {
            let base = resolve_working_dir(dir);
            let path = if global {
                Config::config_dir().join("mcp.json")
            } else {
                base.join(".mcp.json")
            };
            merge_http_oauth_mcp_server_into_json_file(
                &path,
                &name,
                "https://api.githubcopilot.com/mcp/",
                "github",
            )?;
            println!(
                "  Added GitHub OAuth MCP server {:?} → {}",
                name,
                path.display()
            );
            Ok(())
        }
        Commands::Mcp(McpCli::Login {
            name,
            provider,
            client_id,
            client_secret_env,
            scopes,
        }) => {
            let configs = load_mcp_config(&std::env::current_dir()?)?;
            let server = configs
                .into_iter()
                .find(|config| config.name == name)
                .ok_or_else(|| anyhow::anyhow!("MCP server {:?} not found in config", name))?;
            let is_github_server = matches!(
                &server.config,
                McpTransportConfig::Http {
                    auth: Some(McpHttpAuthConfig::OAuth(auth)),
                    ..
                } if auth.provider.as_deref() == Some("github")
            );
            let client_id = client_id.or_else(|| {
                if is_github_server && provider == "github" {
                    std::env::var("ATOMCODE_GITHUB_MCP_CLIENT_ID").ok()
                } else {
                    None
                }
            });
            let token = login_mcp_oauth(
                &server,
                McpOAuthLoginOptions {
                    client_id,
                    client_secret_env,
                    scopes,
                },
            )?;
            println!(
                "  Saved {} OAuth token for MCP server {:?} with {} scope(s)",
                token.provider,
                name,
                token.scopes.len()
            );
            Ok(())
        }
        Commands::Mcp(McpCli::Logout { name }) => {
            let removed = McpTokenStore::default().delete_token(&name)?;
            if removed {
                println!("  Removed saved OAuth token for MCP server {:?}", name);
            } else {
                println!("  No saved OAuth token found for MCP server {:?}", name);
            }
            Ok(())
        }
        Commands::Acp => {
            unreachable!("Acp is handled inline in run() before handle_command")
        }
        Commands::Completion(_) => {
            unreachable!("completion is handled before runtime startup")
        }
        Commands::Hooks(subcmd) => handle_hooks(subcmd).await,
        Commands::Schedule(_) => {
            unreachable!("Schedule is handled inline in run() so its exit code survives")
        }
        Commands::Askpass { .. } => {
            unreachable!("__askpass is handled early in run() before handle_command")
        }
    }
}

/// Handle hooks subcommands.
///
/// Reports and tests the CC-compatible external hooks the LIVE runtime actually
/// runs (`atomcode_capabilities::cc_hooks`: `$ATOMCODE_HOME/hooks.json` +
/// `<project>/.hooks.json`). The legacy v1 engine (TOML script / webhook / built-in
/// hooks) no longer fires at runtime, so it is intentionally not surfaced here.
async fn handle_hooks(cmd: HookCommands) -> Result<()> {
    use atomcode_capabilities::cc_hooks::{
        global_hooks_path, load_hooks_config, project_hooks_path, run_hook_for_test, HookEvent,
    };
    HEADLESS_MODE.store(true, Ordering::Relaxed);

    let cwd = std::env::current_dir().unwrap_or_default();

    // CC hook event → its display / payload name.
    fn event_name(e: HookEvent) -> &'static str {
        match e {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
        }
    }

    // Display the EXACT files cc_hooks loads — via cc_hooks' own resolver, not
    // `Config::config_dir()` (which is sudo-aware and would diverge from what the
    // hook loader actually reads under `sudo`, turning the diagnostic into a lie).
    let project_hooks = project_hooks_path(&cwd);
    let print_paths = || {
        match global_hooks_path() {
            Some(g) => {
                let mark = if g.exists() { "✓" } else { "✗" };
                println!("  {} Global:   {}", mark, g.display());
            }
            None => println!("  ✗ Global:   (no home directory)"),
        }
        let p = if project_hooks.exists() { "✓" } else { "✗" };
        println!("  {} Project:  {}", p, project_hooks.display());
    };

    match cmd {
        HookCommands::List => {
            let hooks = load_hooks_config(&cwd);
            println!("\nLoaded Hooks:");
            println!("─────────────────────────────────────────────");
            if hooks.is_empty() {
                println!("  (No hooks loaded)");
            } else {
                let mut by_event: std::collections::BTreeMap<&str, usize> =
                    std::collections::BTreeMap::new();
                for h in &hooks {
                    *by_event.entry(event_name(h.event)).or_insert(0) += 1;
                }
                println!("  {:<20} {:>5}", "Event", "Count");
                println!("  {:<20} {:>5}", "─".repeat(20), "─".repeat(5));
                for (ev, n) in &by_event {
                    println!("  {:<20} {:>5}", ev, n);
                }
                println!("  {:<20} {:>5}", "─".repeat(20), "─".repeat(5));
                println!("  {:<20} {:>5}", "Total", hooks.len());
            }
            println!("\nHook Config Files:");
            println!("─────────────────────────────────────────────");
            print_paths();
            println!();

            let untrusted: Vec<_> =
                atomcode_capabilities::plugin::installed_plugin_hook_trust_status()
                    .into_iter()
                    .filter(|s| !s.trusted)
                    .collect();
            if !untrusted.is_empty() {
                println!("Untrusted plugin hooks (not loaded):");
                for s in &untrusted {
                    println!(
                        "  {} — {} hook(s) [{}] · run: atomcode plugin trust {}",
                        s.plugin,
                        s.hook_count,
                        s.events.join(", "),
                        s.plugin
                    );
                }
                println!();
            }
            Ok(())
        }
        HookCommands::Test { name } => {
            let hooks = load_hooks_config(&cwd);
            // cc_hooks hooks carry no name — match by event name or a command substring.
            let found = hooks.iter().find(|h| {
                event_name(h.event).eq_ignore_ascii_case(&name) || h.command.contains(&name)
            });
            match found {
                None => {
                    println!("❌ No hook matching '{}' found.", name);
                    if hooks.is_empty() {
                        println!("\n  (No hooks loaded. Check hooks.json / .hooks.json.)");
                    } else {
                        println!("\nAvailable hooks (test by event name or a command substring):");
                        for h in &hooks {
                            println!("  🔹 {:<16} {}", event_name(h.event), h.command);
                        }
                    }
                }
                Some(hook) => {
                    println!("\n🔧 Testing Hook ({})", event_name(hook.event));
                    println!("  Command:   {}", hook.command);
                    println!("  Timeout:   {} ms", hook.timeout_ms);
                    if let Some(ref m) = hook.matcher {
                        println!("  Matcher:   {}", m);
                    }
                    println!();
                    // CC stdin payload — event-shaped to MATCH what the live runtime pipes
                    // (see cc_hooks lifecycle methods): only tool events carry tool fields,
                    // PostToolUse carries `tool_response`, UserPromptSubmit carries `prompt`.
                    let sid = "test-session-0000";
                    let cwd_s = cwd.display().to_string();
                    let payload = match hook.event {
                        HookEvent::PreToolUse => serde_json::json!({
                            "session_id": sid, "hook_event_name": "PreToolUse", "cwd": cwd_s,
                            "tool_name": "bash", "tool_input": { "command": "echo hello" },
                        }),
                        HookEvent::PostToolUse => serde_json::json!({
                            "session_id": sid, "hook_event_name": "PostToolUse", "cwd": cwd_s,
                            "tool_name": "bash", "tool_response": "hello\n",
                        }),
                        HookEvent::UserPromptSubmit => serde_json::json!({
                            "session_id": sid, "hook_event_name": "UserPromptSubmit",
                            "cwd": cwd_s, "prompt": "test prompt",
                        }),
                        HookEvent::SessionStart => serde_json::json!({
                            "session_id": sid, "hook_event_name": "SessionStart", "cwd": cwd_s,
                        }),
                        HookEvent::SessionEnd => serde_json::json!({
                            "session_id": sid, "hook_event_name": "SessionEnd", "cwd": cwd_s,
                        }),
                    };
                    let start = std::time::Instant::now();
                    match run_hook_for_test(hook, &payload).await {
                        Some(out) => {
                            println!("📋 Result:");
                            println!("  Duration:  {:?}", start.elapsed());
                            // CC exit-code contract: 0 = ok, 2 = DELIBERATE block (not a
                            // failure), other/signal = the hook broke.
                            let (label, detail) = match out.exit_code {
                                Some(0) => ("✅ SUCCESS", "exit code 0".to_string()),
                                Some(2) => (
                                    "⛔ BLOCK",
                                    "exit code 2 — hook requested a block (CC contract)"
                                        .to_string(),
                                ),
                                Some(c) => ("❌ FAILURE", format!("exit code {}", c)),
                                None => ("❌ FAILURE", "terminated by signal".to_string()),
                            };
                            println!("  Status:    {} ({})", label, detail);
                            if !out.stdout.is_empty() {
                                println!("  ── stdout ──");
                                for l in out.stdout.trim_end().lines() {
                                    println!("  │ {}", l);
                                }
                            }
                            if !out.stderr.is_empty() {
                                println!("  ── stderr ──");
                                for l in out.stderr.trim_end().lines() {
                                    println!("  │ {}", l);
                                }
                            }
                        }
                        None => {
                            println!("📋 Result:");
                            println!(
                                "  ❌ Hook did not complete: it timed out (>{} ms) or failed to spawn.",
                                hook.timeout_ms
                            );
                        }
                    }
                }
            }
            Ok(())
        }
        HookCommands::Paths => {
            println!("\nHook Configuration Files:");
            println!("─────────────────────────────────────────────");
            print_paths();
            println!("\nDocumentation:");
            println!("─────────────────────────────────────────────");
            println!("  docs/hooks.md - Hook usage guide");
            println!();
            Ok(())
        }
    }
}

/// Dispatch `atomcode plugin ...` subcommands. Each branch calls the same
/// `atomcode_capabilities::plugin::*` API the TUI's `/plugin` slash command uses, so
/// CLI installs and TUI installs share state under `$ATOMCODE_HOME/plugins/`.
fn handle_plugin_cli(sub: PluginCli) -> Result<()> {
    use atomcode_capabilities::plugin::{installer, marketplace};
    match sub {
        PluginCli::Marketplace(MarketplaceCli::Add { url }) => {
            let info = marketplace::add_marketplace(&url)
                .map_err(|e| anyhow::anyhow!("add marketplace: {:#}", e))?;
            println!(
                "  marketplace `{}` added at {} ({} plugins)",
                info.name,
                &info.git_commit[..7.min(info.git_commit.len())],
                info.plugins.len()
            );
            Ok(())
        }
        PluginCli::Marketplace(MarketplaceCli::Remove { name }) => {
            marketplace::remove_marketplace(&name)
                .map_err(|e| anyhow::anyhow!("remove marketplace: {:#}", e))?;
            println!("  marketplace `{}` removed", name);
            Ok(())
        }
        PluginCli::Marketplace(MarketplaceCli::Update { name }) => {
            let info = marketplace::update_marketplace(&name)
                .map_err(|e| anyhow::anyhow!("update marketplace: {:#}", e))?;
            println!(
                "  marketplace `{}` updated to {}",
                info.name,
                &info.git_commit[..7.min(info.git_commit.len())]
            );
            Ok(())
        }
        PluginCli::Marketplace(MarketplaceCli::List) => {
            let items = marketplace::list_marketplaces()?;
            if items.is_empty() {
                println!("  no marketplaces registered");
            } else {
                for m in items {
                    println!(
                        "  {}  {}  {}  ({} plugins)",
                        m.name,
                        m.source,
                        &m.git_commit[..7.min(m.git_commit.len())],
                        m.plugins.len()
                    );
                }
            }
            Ok(())
        }
        PluginCli::Install { spec } => {
            let installed_plugin_name: String;
            match parse_plugin_spec(&spec)? {
                PluginSpec::Qualified {
                    plugin,
                    marketplace: mp,
                } => {
                    let info = installer::install(
                        &plugin,
                        &mp,
                        atomcode_capabilities::plugin::InstallScope::User,
                    )
                    .map_err(|e| anyhow::anyhow!("install: {:#}", e))?;
                    println!("  installed `{}@{}`", info.plugin, info.marketplace);
                    installed_plugin_name = info.plugin;
                }
                PluginSpec::Bare { plugin } => {
                    match installer::resolve_plugin_marketplace(&plugin)
                        .map_err(|e| anyhow::anyhow!("resolve: {:#}", e))?
                    {
                        matches if matches.len() == 1 => {
                            let m = &matches[0];
                            let mp = m.marketplace.clone();
                            let resolved_plugin = m.plugin.clone();
                            let info = installer::install(
                                &resolved_plugin,
                                &mp,
                                atomcode_capabilities::plugin::InstallScope::User,
                            )
                            .map_err(|e| anyhow::anyhow!("install: {:#}", e))?;
                            println!("  installed `{}@{}`", info.plugin, info.marketplace);
                            installed_plugin_name = info.plugin;
                        }
                        matches if matches.len() > 1 => {
                            let mut msg = format!(
                                "plugin `{}` found in multiple marketplaces, please specify:\n",
                                plugin
                            );
                            for m in &matches {
                                msg.push_str(&format!(
                                    "  atomcode plugin install {}@{}\n",
                                    m.plugin, m.marketplace
                                ));
                            }
                            anyhow::bail!(msg.trim().to_string());
                        }
                        _ => {
                            anyhow::bail!("plugin `{}` not found in any marketplace", plugin);
                        }
                    }
                }
            }
            // Surface untrusted hooks for the freshly-installed plugin only — they
            // will NOT run until the user trusts them (loaded-code trust gate).
            // Filtered by `info.plugin` (the canonical plugin name returned by the
            // installer) so pre-existing untrusted plugins don't produce spurious output.
            for s in atomcode_capabilities::plugin::installed_plugin_hook_trust_status() {
                if !s.trusted && s.plugin == installed_plugin_name {
                    println!(
                        "Plugin `{}` ships {} hook(s) on [{}]. They will NOT run until trusted:\n  atomcode plugin trust {}",
                        s.plugin, s.hook_count, s.events.join(", "), s.plugin
                    );
                }
            }
            Ok(())
        }
        PluginCli::Uninstall { spec } => {
            match parse_plugin_spec(&spec)? {
                PluginSpec::Qualified {
                    plugin,
                    marketplace: mp,
                } => {
                    installer::uninstall(
                        &plugin,
                        &mp,
                        atomcode_capabilities::plugin::InstallScope::User,
                    )
                    .map_err(|e| anyhow::anyhow!("uninstall: {:#}", e))?;
                    println!("  uninstalled `{}@{}`", plugin, mp);
                }
                PluginSpec::Bare { plugin } => {
                    let installed = installer::list_installed().unwrap_or_default();
                    let matches: Vec<_> = installed
                        .into_iter()
                        .filter(|p| {
                            p.plugin == plugin
                                || p.plugin
                                    == atomcode_capabilities::plugin::marketplace::sanitize_name(
                                        &plugin,
                                    )
                        })
                        .collect();
                    match matches.len() {
                        0 => anyhow::bail!("plugin `{}` is not installed", plugin),
                        1 => {
                            let p = &matches[0];
                            installer::uninstall(&p.plugin, &p.marketplace, p.scope.clone())
                                .map_err(|e| anyhow::anyhow!("uninstall: {:#}", e))?;
                            println!("  uninstalled `{}@{}`", p.plugin, p.marketplace);
                        }
                        _ => {
                            let mut msg = format!(
                                "plugin `{}` installed from multiple marketplaces, please specify:\n",
                                plugin
                            );
                            for p in &matches {
                                msg.push_str(&format!(
                                    "  atomcode plugin uninstall {}@{}\n",
                                    p.plugin, p.marketplace
                                ));
                            }
                            anyhow::bail!(msg.trim().to_string());
                        }
                    }
                }
            }
            Ok(())
        }
        PluginCli::Trust { name } => {
            let status = atomcode_capabilities::plugin::installed_plugin_hook_trust_status();
            let matches: Vec<_> = if name.contains('@') {
                status.iter().filter(|s| s.plugin_id == name).collect()
            } else {
                status.iter().filter(|s| s.plugin == name).collect()
            };
            match matches.as_slice() {
                [] => anyhow::bail!("plugin `{name}` has no hooks (or is not installed)"),
                [s] => {
                    atomcode_capabilities::plugin::hook_trust::trust(&s.plugin_id, &s.hash)?;
                    println!(
                        "Trusted {} hook(s) from `{}` [{}].",
                        s.hook_count,
                        name,
                        s.events.join(", ")
                    );
                }
                many => {
                    let mut msg =
                        format!("plugin `{name}` installed from multiple marketplaces:\n");
                    for s in many {
                        msg.push_str(&format!(
                            "  atomcode plugin trust {}@{}\n",
                            s.plugin, s.marketplace
                        ));
                    }
                    anyhow::bail!(msg);
                }
            }
            Ok(())
        }
        PluginCli::Untrust { name } => {
            let status = atomcode_capabilities::plugin::installed_plugin_hook_trust_status();
            let matches: Vec<_> = if name.contains('@') {
                status.iter().filter(|s| s.plugin_id == name).collect()
            } else {
                status.iter().filter(|s| s.plugin == name).collect()
            };
            match matches.as_slice() {
                [] => anyhow::bail!("plugin `{name}` has no hooks (or is not installed)"),
                [s] => {
                    atomcode_capabilities::plugin::hook_trust::untrust(&s.plugin_id)?;
                    println!("Untrusted hooks from `{name}`.");
                }
                many => {
                    let mut msg =
                        format!("plugin `{name}` installed from multiple marketplaces:\n");
                    for s in many {
                        msg.push_str(&format!(
                            "  atomcode plugin untrust {}@{}\n",
                            s.plugin, s.marketplace
                        ));
                    }
                    anyhow::bail!(msg);
                }
            }
            Ok(())
        }
        PluginCli::List => {
            let items = installer::list_installed()?;
            if items.is_empty() {
                println!("  no installed plugins");
            } else {
                for p in items {
                    println!("  {}@{}  {}", p.plugin, p.marketplace, p.plugin_dir);
                }
            }
            Ok(())
        }
    }
}

/// Parsed argument for `atomcode plugin install/uninstall`.
/// Supports both `plugin@marketplace` (fully qualified) and bare
/// `plugin` (resolved across all marketplaces).
enum PluginSpec {
    /// Explicit `plugin@marketplace` — use as-is.
    Qualified { plugin: String, marketplace: String },
    /// Bare plugin name — needs marketplace resolution.
    Bare { plugin: String },
}

/// Parse a plugin spec string. Accepts both `plugin@marketplace` and
/// bare `plugin` (resolved across all registered marketplaces).
fn parse_plugin_spec(s: &str) -> Result<PluginSpec> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("expected <plugin> or <plugin>@<marketplace>, got empty string");
    }
    if let Some((plugin, mp)) = s.split_once('@') {
        if plugin.trim().is_empty() || mp.trim().is_empty() {
            anyhow::bail!("plugin/marketplace name must not be empty in `{}`", s);
        }
        Ok(PluginSpec::Qualified {
            plugin: plugin.trim().to_string(),
            marketplace: mp.trim().to_string(),
        })
    } else {
        Ok(PluginSpec::Bare {
            plugin: s.to_string(),
        })
    }
}

/// CLI (non-TUI) upgrade driver — prints progress to stdout and
/// success/error messages the same way `install.sh` does.
async fn run_upgrade_cli(force: bool) -> Result<()> {
    use atomcode_updater::{self as self_update, UpgradeEvent, ALREADY_LATEST};

    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<UpgradeEvent>();

    // Spawn the driver; consume events on the main task so stdout
    // writes don't interleave unpredictably with the upgrade work.
    let driver = tokio::spawn(self_update::run_upgrade(current.clone(), force, tx));

    let mut last_pct: i32 = -1;
    while let Some(ev) = rx.recv().await {
        match ev {
            UpgradeEvent::ManifestFetched { version } => {
                println!("==> Latest: {}", version);
            }
            UpgradeEvent::Downloading { bytes, total } => {
                // Debounce to whole percents so we don't spam stdout —
                // piping the CLI through `tee` with 10k updates is no
                // fun for anyone.
                let pct = if total == 0 {
                    0
                } else {
                    ((bytes * 100) / total) as i32
                };
                if pct != last_pct {
                    print!(
                        "\r    downloading {}% ({} / {} bytes)   ",
                        pct, bytes, total
                    );
                    io::stdout().flush().ok();
                    last_pct = pct;
                }
            }
            UpgradeEvent::Verifying => {
                println!("\n==> Verifying SHA256");
            }
            UpgradeEvent::Replacing => {
                println!("==> Replacing binary");
            }
            UpgradeEvent::Done {
                version,
                backup,
                exe: _,
            } => {
                println!(
                    "\n✓ Upgraded to {} (previous version kept at {})",
                    version,
                    backup.display()
                );
                println!("  Run `atomcode` to start the new version.");
            }
            // CLI path never spawns a rollback via this channel and the
            // driver below translates errors into the returned Result
            // (not a Failed event) — these arms exist only to keep the
            // match exhaustive if the TUI path ever reuses this code.
            UpgradeEvent::Failed(msg) => {
                if msg.contains(atomcode_updater::PACKAGE_MANAGED) {
                    println!(
                        "\n{}",
                        atomcode_config::i18n::t(atomcode_config::i18n::Msg::UpgradePackageManaged)
                    );
                } else {
                    eprintln!("\nupgrade failed: {}", msg);
                }
            }
            UpgradeEvent::RolledBack { exe, backup } => {
                println!(
                    "\n✓ Rolled back. exe={}, backup={}",
                    exe.display(),
                    backup.display()
                );
            }
        }
    }

    match driver.await {
        Ok(Ok(_summary)) => Ok(()),
        Ok(Err(e)) => {
            let msg = format!("{:#}", e);
            if msg.contains(atomcode_updater::PACKAGE_MANAGED) {
                println!(
                    "{}",
                    atomcode_config::i18n::t(atomcode_config::i18n::Msg::UpgradePackageManaged)
                );
                Ok(())
            } else if msg.contains(ALREADY_LATEST) {
                // Friendly path — not an error, just "nothing to do".
                println!("  {}", msg.replace(&format!("{}: ", ALREADY_LATEST), ""));
                Ok(())
            } else {
                Err(e)
            }
        }
        Err(e) => Err(anyhow::anyhow!("upgrade task panicked: {}", e)),
    }
}

fn run_rollback_cli() -> Result<()> {
    let summary = match atomcode_updater::run_rollback() {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("{:#}", e);
            if msg.contains(atomcode_updater::PACKAGE_MANAGED) {
                println!(
                    "{}",
                    atomcode_config::i18n::t(atomcode_config::i18n::Msg::UpgradePackageManaged)
                );
                return Ok(());
            }
            return Err(e);
        }
    };
    println!(
        "✓ Rolled back. Previous binary is now at {}, other version saved at {}",
        summary.exe.display(),
        summary.backup.display()
    );
    println!("  Run `atomcode` to start the rolled-back version.");
    Ok(())
}

/// Core CodingPlan flow shared by CLI-exit and CLI→TUI paths. Loads
/// the config (or starts from defaults if missing), runs the shared
/// `coding_plan::setup` orchestrator, persists the config on success,
/// and returns the rendered human-readable report — the caller decides
/// whether to print it to stdout or stash it for the TUI to surface.
fn run_codingplan_core(
    telemetry: Option<&std::sync::Arc<atomcode_telemetry::Telemetry>>,
) -> Result<String> {
    let path = Config::default_path();
    // Missing config is legitimate on first install — start from defaults
    // so the flow can still add AtomGit providers to a fresh config.toml.
    let mut config = match Config::load(&path) {
        Ok(c) => c,
        Err(_) => Config::default(),
    };
    atomcode_config::proxy::apply_process_proxy_config(&config.network.proxy);

    // If the stored token is locally valid (file present, expires_in
    // not yet past) but the server rejects it (revoked, refresh-token
    // dead, etc.), the orchestrator sets `report.auth_expired = true`.
    // Run OAuth *once* on that path — same flow `atomcode login` would
    // use — then re-run setup against the fresh token. Without this
    // the user sees the report ending in "claim failed — run `atomcode
    // login` again" and has to do manually what `codingplan` could
    // do itself.
    let mut report = atomcode_codingplan::run(&mut config, telemetry)?;
    if report.auth_expired {
        use atomcode_config::i18n::{t, Msg};
        print!("{}", t(Msg::CpReauthAfter401));
        match atomcode_auth::login(telemetry)
            .and_then(|auth| atomcode_auth::save_auth(&auth).map(|_| auth))
        {
            Ok(_) => {
                report = atomcode_codingplan::run(&mut config, telemetry)?;
            }
            Err(e) => {
                // Re-OAuth itself failed (user pressed Ctrl+C, network
                // dead, etc.). Print the *original* report so users
                // still see what triggered the retry, then bail.
                println!("{}", report.render());
                anyhow::bail!("re-authentication failed: {:#}", e);
            }
        }
    }

    if report.should_persist_config() {
        let persisted = match atomcode_config::ConfigStore::new(&path)
            .update(|latest| atomcode_codingplan::merge_successful_config(latest, &config, &report))
        {
            Ok(_) => true,
            Err(e) => {
                eprintln!("  ⚠ Failed to save config to {}: {:#}", path.display(), e);
                false
            }
        };
        // Stamp the sync marker alongside the config write. The drift
        // monitor on the TUI side reads this to decide whether to warn
        // about stale provider lists (> 24h + server drift). A failed
        // marker write is non-fatal — the config already landed; only
        // the 24h hint would be miscounted, which self-corrects on the
        // next successful run.
        if persisted {
            if let Err(e) = atomcode_codingplan::write_last_sync_now() {
                eprintln!("  ⚠ Failed to write codingplan sync marker: {:#}", e);
            }
        }
    }

    Ok(report.render())
}

/// Guard so the two-link panic-hook chain (pre-telemetry hook + telemetry-aware
/// hook that chains to it) writes the crash log exactly once.
static CRASH_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Synchronously append a panic's location + message + backtrace to
/// `$ATOMCODE_HOME/logs/panic.log`, then `flush` + `sync_all` so the bytes are
/// durable **before** the hook returns and the runtime calls `abort()`
/// (`panic = "abort"` in the release profile).
///
/// Why this exists: with `abort`, neither of the existing report paths
/// survives a crash — stderr is lost when the terminal window closes (Windows
/// resize crash), and `Telemetry::track` is async (mpsc → background tokio
/// writer), so abort kills the writer mid-segment and the `.partial` queue file
/// is discarded. A blocking, fsync'd file write is the only sink that survives.
/// Best-effort: every step swallows errors so the hook never re-panics.
fn write_crash_log(info: &std::panic::PanicHookInfo<'_>) {
    use std::io::Write;
    use std::sync::atomic::Ordering;
    if CRASH_LOGGED.swap(true, Ordering::SeqCst) {
        return;
    }
    // Same `logs/` dir as [`atomcode_log_path`], so it must resolve the same
    // way: that one goes through `Config::config_dir()`, this one used to hard-
    // code `~/.atomcode`, and with `$ATOMCODE_HOME` set the two split into
    // different trees — `atomcode.log` where the user configured it, the crash
    // report somewhere they never look.
    //
    // This also drops the old give-up-if-no-home arm: with nothing resolvable
    // the report now lands in a cwd-relative dir, matching every other path the
    // process writes, rather than being silently discarded.
    let dir = atomcode_config::config::Config::config_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("panic.log"))
    else {
        return;
    };
    let loc = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "unknown".into());
    let msg = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_default();
    let thread = std::thread::current()
        .name()
        .unwrap_or("unknown")
        .to_string();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // `force_capture` ignores RUST_BACKTRACE and always resolves frames.
    let bt = std::backtrace::Backtrace::force_capture();
    let _ = writeln!(f, "\n==== AtomCode panic @ unix:{ts} thread:{thread} ====");
    let _ = writeln!(f, "location: {loc}");
    let _ = writeln!(f, "message : {msg}");
    let _ = writeln!(f, "backtrace:\n{bt}");
    let _ = f.flush();
    let _ = f.sync_all();
}

/// Install the telemetry-aware panic hook. Replaces the minimal pre-init hook
/// set in `main()` so panics are both reported cleanly to the terminal AND
/// sent as a `Panic` telemetry event before the process exits.
fn install_panic_hook(telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Durable crash log FIRST — before restore_terminal / async telemetry /
        // abort, any of which can lose the panic on Windows (see write_crash_log).
        write_crash_log(info);
        restore_terminal_if_tui();
        let home = atomcode_config::util::real_home_dir();
        let cwd = std::env::current_dir().ok();
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown".into());
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let scrubbed_loc =
            atomcode_telemetry::scrub::scrub_path(&loc, home.as_deref(), cwd.as_deref());
        let scrubbed_msg = atomcode_telemetry::scrub::truncate_head(
            &atomcode_telemetry::scrub::scrub_path(&msg, home.as_deref(), cwd.as_deref()),
            atomcode_telemetry::scrub::HEAD_MAX,
        );
        let frames =
            atomcode_telemetry::scrub::backtrace_top_k(&bt, 5, home.as_deref(), cwd.as_deref());
        telemetry.track(atomcode_telemetry::Event::Panic {
            location: scrubbed_loc,
            message_head: scrubbed_msg,
            thread: std::thread::current().name().unwrap_or("unknown").into(),
            backtrace_top_5: frames,
            error_kind: Some("panic".to_string()),
            error_data: Some(
                serde_json::json!({
                    "session_duration_secs": telemetry.uptime().as_secs() as u32,
                    "turns_completed": null,
                    "last_tool_name": null,
                    "last_event": null,
                })
                .to_string(),
            ),
        });
        default_hook(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::{
        apply_cli_runtime_overrides, atomcode_log_path, close_thinking_chunk,
        format_thinking_chunk, format_verbose_tool_chunk, headless_completion_exit_code,
        headless_completion_notify_reason, is_completion_invocation, merge_startup_notices,
        interactive_provider_bootstrap, print_shell_completion, resolve_working_dir,
        runtime_config_from,
        should_fork_busy_continue, truncate_log_line, Cli, Commands, DEFAULT_LOG_DIRECTIVES,
    };
    use clap::Parser;
    use clap_complete::Shell;
    use std::path::PathBuf;

    #[test]
    fn completion_subcommand_defaults_to_bash_and_accepts_all_supported_shells() {
        let default = Cli::try_parse_from(["atomcode", "completion"]).unwrap();
        assert!(matches!(
            default.command,
            Some(Commands::Completion(command)) if command.shell == Shell::Bash
        ));

        for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
            let parsed = Cli::try_parse_from(["atomcode", "completion", shell]).unwrap();
            assert!(matches!(parsed.command, Some(Commands::Completion(_))));
        }
    }

    #[test]
    fn completion_scripts_cover_all_supported_shells() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let mut output = Vec::new();
            print_shell_completion(shell, &mut output);
            let script = String::from_utf8(output).unwrap();
            assert!(!script.is_empty(), "{shell:?} script should not be empty");
            assert!(
                script.contains("atomcode"),
                "{shell:?} script should target atomcode"
            );
            assert!(
                script.contains("completion"),
                "{shell:?} script should include the completion command"
            );
            assert!(
                !script.contains("__askpass"),
                "{shell:?} script should not expose the internal askpass helper"
            );
            assert!(
                !script.contains("codingplan"),
                "{shell:?} script should not expose deprecated hidden aliases"
            );
            assert!(
                !script.contains("acp"),
                "{shell:?} script should not expose internal protocol commands"
            );
        }
    }

    #[test]
    fn completion_early_exit_only_matches_the_root_subcommand() {
        let invocation =
            |args: &[&str]| is_completion_invocation(args.iter().map(std::ffi::OsString::from));

        assert!(invocation(&["completion", "zsh"]));
        assert!(invocation(&["--no-telemetry", "completion", "fish"]));
        assert!(invocation(&[
            "--config",
            "/tmp/config.toml",
            "completion",
            "bash"
        ]));
        assert!(!invocation(&["--provider", "completion"]));
        assert!(!invocation(&["-p", "completion"]));
        assert!(!invocation(&["mcp", "add", "completion", "server"]));
        assert!(!invocation(&["--", "completion"]));
    }

    #[test]
    fn log_path_is_logs_subdir_of_config_dir() {
        // The log lives at `<config_dir>/logs/atomcode.log`; ATOMCODE_HOME/sudo
        // resolution is `Config::config_dir()`'s job (covered by its own tests), so
        // this pins only the join rule.
        assert_eq!(
            atomcode_log_path(PathBuf::from("/Users/x/.atomcode")),
            PathBuf::from("/Users/x/.atomcode/logs/atomcode.log")
        );
    }

    #[test]
    fn default_log_directives_parse() {
        // A malformed default would silently disable ALL logging via the fallback
        // path in `init_file_logging`; pin that it is a valid EnvFilter directive.
        assert!(tracing_subscriber::EnvFilter::try_new(DEFAULT_LOG_DIRECTIVES).is_ok());
    }

    #[test]
    fn config_and_session_startup_notices_are_both_preserved() {
        assert_eq!(
            merge_startup_notices(
                Some("bad provider ignored".into()),
                Some("busy session forked".into())
            )
            .as_deref(),
            Some("bad provider ignored\nbusy session forked")
        );
    }

    #[test]
    fn busy_continue_forks_only_for_interactive_session_contention() {
        let busy = atomcode_capabilities::session::SessionStoreError::SessionInUse {
            id: "source".into(),
            path: PathBuf::from("/sessions/source.lease"),
        };
        let missing = atomcode_capabilities::session::SessionStoreError::NotFound {
            path: PathBuf::from("/sessions/source.meta"),
        };

        assert!(should_fork_busy_continue(true, &busy));
        assert!(!should_fork_busy_continue(false, &busy));
        assert!(!should_fork_busy_continue(true, &missing));
    }

    #[test]
    fn verbose_tool_chunk_strips_ephemeral_activity_marker() {
        assert_eq!(
            format_verbose_tool_chunk("\u{1e}review · round 2 · read_file"),
            "[progress] review · round 2 · read_file\n"
        );
    }

    #[test]
    fn snapshot_unavailable_is_headless_failure_even_when_reason_is_stopped() {
        let completion = atomcode_coding::TurnCompletion::SnapshotUnavailable {
            turn_id: 1,
            reason: atomcode_kernel::event::StopReason::Stopped,
            error: atomcode_coding::RuntimeSnapshotError {
                message: "snapshot failed".into(),
            },
            stats: Default::default(),
        };

        assert_eq!(headless_completion_exit_code(&completion, 0), 1);
        assert_eq!(
            headless_completion_notify_reason(&completion),
            atomcode_capabilities::notify::NotifyStopReason::Error
        );
    }

    #[test]
    fn runtime_config_honors_provider_override() {
        // Regression: engine-v2 headless `--provider X` was silently ignored —
        // runtime_config_from read `default_provider` directly instead of routing
        // through `active_provider`, so the runtime picked the config default
        // (e.g. an AtomGit gateway needing a signer this build lacks) and a
        // `--provider deepseek` run hit the wrong endpoint and failed.
        let toml_str = r#"
            default_provider = "gateway"

            [providers.gateway]
            type = "openai"
            model = "gw-model"
            base_url = "https://llm-api.atomgit.com/v1"

            [providers.direct]
            type = "openai"
            api_key = "sk-direct"
            model = "direct-model"
            base_url = "https://api.deepseek.com"
            reasoning_history = "exclude"
        "#;
        let config: atomcode_config::config::Config = toml::from_str(toml_str).unwrap();
        let wd = PathBuf::from("/tmp/x");

        // No override → the config default (gateway), no reasoning_history set.
        let def = runtime_config_from(&config, &wd, None, None, false, false);
        assert_eq!(def.base_url, "https://llm-api.atomgit.com/v1");
        assert_eq!(def.model, "gw-model");
        assert_eq!(def.provider_name, "gateway");
        assert_eq!(def.reasoning_history, None);

        // `--provider direct` → that provider's endpoint/model/key + its per-provider
        // reasoning_history override, NOT the default.
        let ov = runtime_config_from(&config, &wd, Some("direct"), None, false, false);
        assert_eq!(ov.base_url, "https://api.deepseek.com");
        assert_eq!(ov.model, "direct-model");
        assert_eq!(ov.provider_name, "direct");
        assert_eq!(ov.api_key, "sk-direct");
        assert_eq!(ov.reasoning_history.as_deref(), Some("exclude"));
    }

    #[test]
    fn interactive_restart_recovers_new_schema_provider() {
        let config: atomcode_config::config::Config = toml::from_str(
            r#"
                default_model = "taotoken/glm_for_coding"

                [provider_accounts.taotoken]
                provider = "openai"
                base_url = "https://example.test/v1"
                api_key = "test"

                [models."taotoken/glm_for_coding"]
                account = "taotoken"
                model = "glm_for_coding"
                context_window = 131072
            "#,
        )
        .unwrap();
        let runtime_cfg =
            runtime_config_from(&config, std::path::Path::new("/tmp"), None, None, false, true);

        assert!(config.providers.is_empty(), "legacy table stays empty");
        assert_eq!(
            interactive_provider_bootstrap(&runtime_cfg),
            atomcode_coding::ProviderBootstrap::RecoverAuthentication
        );
        let empty_runtime_cfg = runtime_config_from(
            &atomcode_config::config::Config::default(),
            std::path::Path::new("/tmp"),
            None,
            None,
            false,
            true,
        );
        assert_eq!(
            interactive_provider_bootstrap(&empty_runtime_cfg),
            atomcode_coding::ProviderBootstrap::Unavailable(
                atomcode_coding::ProviderUnavailableReason::NotConfigured
            )
        );
    }

    #[test]
    fn interactive_bootstrap_uses_runtime_fallback_resolution() {
        let config: atomcode_config::config::Config = toml::from_str(
            r#"
                default_model = "broken"

                [provider_accounts.missing]
                provider = "openai"

                [models.broken]
                account = "unknown"
                model = "broken-model"
                context_window = 131072

                [provider_accounts.usable]
                provider = "openai"
                base_url = "https://example.test/v1"
                api_key = "test"

                [models.usable]
                account = "usable"
                model = "fallback-model"
                context_window = 131072
            "#,
        )
        .unwrap();
        let runtime_cfg =
            runtime_config_from(&config, std::path::Path::new("/tmp"), None, None, false, true);

        assert_eq!(runtime_cfg.model, "fallback-model");
        assert_eq!(
            interactive_provider_bootstrap(&runtime_cfg),
            atomcode_coding::ProviderBootstrap::RecoverAuthentication
        );
    }

    #[test]
    fn round_cap_checkpoint_default_off_and_propagates_to_agent_config() {
        // Guard against C1 regression: the interactive checkpoint is TUI-only,
        // opted in at the TUI spawn sites by flipping `round_cap_checkpoint` on
        // the runtime config. A direct test of the async CLI factory
        // (`spawn_native_cli_runtime`/`spawn_deferred_tui_runtime`) isn't
        // feasible here — both need a live runtime + provider bootstrap — so we
        // pin the propagation seam those sites rely on: the field defaults off
        // and `agent_config()` copies it through to the kernel-facing config.
        let toml_str = r#"
            default_provider = "p"
            [providers.p]
            type = "openai"
            model = "m"
            api_key = "k"
            base_url = "https://example.test/v1"
        "#;
        let config: atomcode_config::config::Config = toml::from_str(toml_str).unwrap();
        let wd = PathBuf::from("/tmp/x");

        // Default (headless / ACP / daemon behavior): checkpoint stays off.
        let mut cfg = runtime_config_from(&config, &wd, None, None, false, true);
        assert!(!cfg.round_cap_checkpoint, "defaults off");
        assert!(
            !cfg.agent_config().round_cap_checkpoint,
            "off config yields off agent"
        );

        // TUI opt-in: the flag flows through agent_config() to the kernel.
        cfg.round_cap_checkpoint = true;
        assert!(
            cfg.agent_config().round_cap_checkpoint,
            "TUI opt-in reaches the agent config"
        );
    }

    #[test]
    fn interactive_provider_override_becomes_the_in_process_default() {
        let mut config: atomcode_config::config::Config = toml::from_str(
            r#"
                default_provider = "gateway"

                [providers.gateway]
                type = "openai"
                model = "gw-model"

                [providers.direct]
                type = "openai"
                model = "direct-model"
            "#,
        )
        .unwrap();

        apply_cli_runtime_overrides(&mut config, Some("direct"), None);

        assert_eq!(config.default_provider, "direct");
    }

    #[test]
    fn ascii_short_unchanged() {
        assert_eq!(truncate_log_line("hello", 10), "hello");
    }

    #[test]
    fn ascii_long_truncated_with_ellipsis() {
        assert_eq!(truncate_log_line("0123456789abcdef", 10), "0123456789...");
    }

    #[test]
    fn newlines_become_spaces() {
        assert_eq!(truncate_log_line("a\nb\nc", 10), "a b c");
    }

    #[test]
    fn mixed_ascii_cjk_truncates_at_char_boundary() {
        // 8 chars: ['a','b','c','计','算','d','e','f']; max 5 → "abc计算..."
        assert_eq!(truncate_log_line("abc计算def", 5), "abc计算...");
    }

    /// Regression test for panic at `crates/atomcode-cli/src/main.rs:272:42`:
    /// "byte index 500 is not a char boundary; it is inside '计' (bytes 498..501)".
    /// Triggered when ToolCallResult output was a CJK-heavy string > 500 bytes
    /// and the old code did `trimmed[..500]` (byte slice). Pure CJK at 3 bytes
    /// per char means almost any 500-byte cut lands inside a multi-byte char.
    #[test]
    fn cjk_truncation_does_not_panic() {
        let s: String = "计算".repeat(500); // 1000 chars, 3000 bytes
        let result = truncate_log_line(&s, 500);
        assert_eq!(result.chars().count(), 503); // 500 + "..."
        assert!(result.ends_with("..."));
    }

    /// Regression test for cwd-override bug: when no `-C` is given, working dir
    /// must equal `std::env::current_dir()`. Old code silently substituted the
    /// first line of `~/.atomcode/recent_dirs.txt`, breaking `atomcode -p` from
    /// any directory that wasn't the TUI's last-visited project.
    #[test]
    fn resolve_working_dir_uses_cwd_when_no_cli_dir() {
        let expected = std::env::current_dir().unwrap();
        assert_eq!(resolve_working_dir(None), expected);
    }

    #[test]
    fn resolve_working_dir_honors_cli_dir() {
        let temp = std::env::temp_dir();
        let canon = std::fs::canonicalize(&temp).unwrap_or(temp.clone());
        assert_eq!(resolve_working_dir(Some(temp)), canon);
    }

    #[test]
    fn resolve_working_dir_falls_back_to_input_when_canonicalize_fails() {
        // Use a non-existent path so canonicalize() returns Err and the
        // function falls back to the raw input rather than panicking.
        let bogus = PathBuf::from("/nonexistent/atomcode-test-path-xyzzy");
        assert_eq!(resolve_working_dir(Some(bogus.clone())), bogus);
    }

    /// Verify that std::fs::read_to_string reads a temp file correctly,
    /// which is the core of --prompt-file. This is a unit-level stand-in for
    /// the integration test (full CLI parse requires a running provider).
    #[test]
    fn prompt_file_read_preserves_trailing_newline() {
        use std::io::Write as _;
        let path = std::env::temp_dir().join("atomcode_test_prompt_file.txt");
        let content = "fix the bug\n";
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(content.as_bytes()).unwrap();
        }
        let read_back = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(
            read_back, content,
            "--prompt-file must preserve trailing newline (unlike bash $(...))"
        );
    }

    // ---- format_thinking_chunk / close_thinking_chunk ----
    //
    // Regression suite for the "one word per line" bug in headless verbose
    // output. The old `eprintln!("[thinking] {}", text)` printed a fresh
    // prefix + trailing newline for every streaming chunk, so a streaming
    // reasoning model produced output like:
    //
    //   [thinking] are
    //   [thinking] already
    //   [thinking] configured
    //
    // The new formatter must keep a single line open across many tiny
    // chunks until something else (text, tool call, turn complete, etc.)
    // explicitly closes it.

    /// Streaming many single-token chunks must produce ONE line, not N.
    #[test]
    fn thinking_chunks_stream_onto_single_line() {
        let mut buf = String::new();
        let mut open = false;
        for tok in ["are", " already", " configured", " and", " what"] {
            format_thinking_chunk(&mut buf, &mut open, tok);
        }
        assert_eq!(buf, "[thinking] are already configured and what");
        assert!(open, "line should remain open until something closes it");
    }

    /// A non-reasoning event must close the line with a single newline.
    #[test]
    fn close_appends_newline_and_clears_open() {
        let mut buf = String::new();
        let mut open = false;
        format_thinking_chunk(&mut buf, &mut open, "hello");
        close_thinking_chunk(&mut buf, &mut open);
        assert_eq!(buf, "[thinking] hello\n");
        assert!(!open);
        // Closing again is a no-op (idempotent).
        close_thinking_chunk(&mut buf, &mut open);
        assert_eq!(buf, "[thinking] hello\n");
    }

    /// Embedded newlines inside a chunk must produce a re-prefixed next line.
    #[test]
    fn embedded_newline_reprefixes_next_line() {
        let mut buf = String::new();
        let mut open = false;
        format_thinking_chunk(&mut buf, &mut open, "first line\nsecond line");
        assert_eq!(buf, "[thinking] first line\n[thinking] second line");
        assert!(open);
    }

    /// A chunk ending with `\n` closes the line; the next chunk must
    /// re-introduce the `[thinking] ` prefix.
    #[test]
    fn trailing_newline_closes_and_next_chunk_reprefixes() {
        let mut buf = String::new();
        let mut open = false;
        format_thinking_chunk(&mut buf, &mut open, "para1\n");
        assert!(!open, "trailing newline should close the line");
        format_thinking_chunk(&mut buf, &mut open, "para2");
        assert_eq!(buf, "[thinking] para1\n[thinking] para2");
        assert!(open);
    }

    /// Empty chunks must be skipped without emitting a stray prefix.
    #[test]
    fn empty_chunk_is_noop() {
        let mut buf = String::new();
        let mut open = false;
        format_thinking_chunk(&mut buf, &mut open, "");
        assert_eq!(buf, "");
        assert!(!open);
        // Still no prefix after empty input.
        format_thinking_chunk(&mut buf, &mut open, "x");
        assert_eq!(buf, "[thinking] x");
    }

    /// CJK content (common in Chinese reasoning models) must not break the
    /// single-line invariant — every char-level chunk just appends.
    #[test]
    fn cjk_chunks_stream_correctly() {
        let mut buf = String::new();
        let mut open = false;
        for tok in ["先", "看", "看", "你", "当前的", "环境"] {
            format_thinking_chunk(&mut buf, &mut open, tok);
        }
        assert_eq!(buf, "[thinking] 先看看你当前的环境");
    }

    /// Simulated end-to-end event sequence: thinking deltas, then a tool
    /// call. The tool call must appear on its OWN line, not mashed onto
    /// the tail of the thinking text.
    #[test]
    fn thinking_followed_by_tool_call_is_separated() {
        let mut buf = String::new();
        let mut open = false;
        format_thinking_chunk(&mut buf, &mut open, "I should");
        format_thinking_chunk(&mut buf, &mut open, " check");
        format_thinking_chunk(&mut buf, &mut open, " the file");
        // Now a non-reasoning event arrives → close, then emit it.
        close_thinking_chunk(&mut buf, &mut open);
        buf.push_str("[tool→ read_file]\n");
        assert_eq!(
            buf,
            "[thinking] I should check the file\n[tool→ read_file]\n"
        );
    }
}
