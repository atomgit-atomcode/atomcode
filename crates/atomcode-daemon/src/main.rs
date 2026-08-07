//! AtomCode API Service — standalone binary entrypoint.
//!
//! This is a thin shell around [`atomcode_daemon::run_server`]: it parses CLI
//! arguments, performs process-global bootstrap (Windows console attach, legacy
//! session migration) and then delegates to the shared server logic in the
//! `atomcode_daemon` library crate.

// On Windows, mark this binary as a GUI-subsystem application so that
// launching it from a GUI parent (e.g. VSCode extension host) does NOT
// allocate a visible console window. When launched from a terminal the
// daemon will attempt to re-attach to the parent console for stderr output.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use atomcode_daemon::{run_server, ServerOpts};
use atomcode_telemetry::{CliOverride, SessionMode};

/// Default idle timeout in seconds (30 minutes) before the daemon self-shuts
/// down when no client activity is observed.
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 30 * 60;

fn parse_daemon_args() -> (String, u16, CliOverride, u64, SessionMode) {
    const DEFAULT_HOST: &str = "127.0.0.1";
    const DEFAULT_PORT: u16 = 13456;

    let mut host: Option<String> = None;
    let mut port: Option<u16> = None;
    let mut no_telemetry = false;
    let mut idle_timeout: Option<u64> = None;
    let mut client_mode: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--host" {
            if let Some(value) = args.next() {
                host = Some(value);
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--host=") {
            host = Some(value.to_string());
            continue;
        }

        if arg == "--port" {
            if let Some(value) = args.next() {
                port = value.parse().ok();
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--port=") {
            port = value.parse().ok();
            continue;
        }

        if arg == "--no-telemetry" {
            no_telemetry = true;
            continue;
        }

        if arg == "--idle-timeout" {
            if let Some(value) = args.next() {
                idle_timeout = value.parse().ok();
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--idle-timeout=") {
            idle_timeout = value.parse().ok();
            continue;
        }

        if arg == "--client" {
            if let Some(value) = args.next() {
                client_mode = Some(value);
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--client=") {
            client_mode = Some(value.to_string());
            continue;
        }
    }

    let cli_override = if no_telemetry {
        CliOverride { disabled: true }
    } else {
        CliOverride::default()
    };

    // Allow env var override: ATOMCODE_DAEMON_IDLE_TIMEOUT=<seconds>
    // 0 = disabled; non-zero values are clamped to a minimum of 60s to prevent
    // accidental rapid cycling from misconfigured environments.
    let raw_timeout = idle_timeout
        .or_else(|| {
            std::env::var("ATOMCODE_DAEMON_IDLE_TIMEOUT")
                .ok()?
                .parse()
                .ok()
        })
        .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
    let timeout = if raw_timeout == 0 {
        0
    } else {
        raw_timeout.max(60)
    };

    let mode = match client_mode.as_deref() {
        Some("vscode") => SessionMode::Vscode,
        Some("jetbrains") => SessionMode::Jetbrains,
        Some("webui") => SessionMode::Webui,
        Some("atomcode-air") => SessionMode::AtomcodeAir,
        _ => SessionMode::Ide,
    };

    (
        host.unwrap_or_else(|| DEFAULT_HOST.to_string()),
        port.unwrap_or(DEFAULT_PORT),
        cli_override,
        timeout,
        mode,
    )
}

#[tokio::main]
async fn main() {
    // On Windows, when built as a GUI-subsystem binary (windows_subsystem = "windows"),
    // there is no default console. If launched from a terminal (cmd.exe / PowerShell),
    // re-attach to the parent's console so eprintln!/tracing output is visible.
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
        unsafe {
            AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }

    // Used by IDE packaging to reject a daemon compiled after the private
    // signer overlay was removed. Keep this check side-effect free.
    if std::env::args().any(|arg| arg == "--check-official-build") {
        std::process::exit(if atomcode_capabilities::provider::signer_available() {
            0
        } else {
            1
        });
    }

    // Ensure legacy sessions (macOS pre-v4.16 ~/Library/Application Support/atomcode/sessions)
    // are migrated to the canonical location ($ATOMCODE_HOME/sessions) before any handler reads it.
    if let Err(error) = atomcode_capabilities::session::SessionManager::migrate_from_legacy() {
        tracing::warn!("[session] Failed to migrate legacy sessions: {error}");
    }

    let (host, port, cli_override, idle_timeout_secs, startup_mode) = parse_daemon_args();

    let token_store = atomcode_daemon::auth_token::WebuiTokenStore::new();
    let daemon_token = atomcode_daemon::resolve_daemon_token(
        std::env::var("ATOMCODE_DAEMON_TOKEN").ok(),
        &token_store,
    );

    if let Err(e) = run_server(ServerOpts {
        host,
        port,
        cli_override,
        idle_timeout_secs,
        startup_mode,
        webui_tokens: Some(token_store),
        // 独立二进制：保留完整启动横幅。
        quiet: false,
        // 独立二进制 / VSCode：沿用 config 的 default_workdir，不覆盖。
        working_dir_override: None,
        // 独立二进制自行 bind host:port，不预绑定。
        prebound_listener: None,
        // 独立 daemon 模式不需要 app user_id 校验。
        app_user_id: None,
        daemon_token_file: Some(daemon_token),
    })
    .await
    {
        eprintln!("Fatal: daemon server error: {e:#}");
        std::process::exit(1);
    }
}
