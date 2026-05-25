use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use crate::agent::TurnStopReason;
use crate::config::NotificationConfig;

#[derive(Debug, Clone)]
pub struct TurnNotification<'a> {
    pub duration: Duration,
    pub turn_count: usize,
    pub tool_call_count: usize,
    pub total_tokens: Option<usize>,
    pub stop_reason: TurnStopReason,
    pub working_dir: Option<&'a Path>,
}

#[derive(Debug, Clone)]
pub struct ApprovalNotification<'a> {
    pub tool_name: &'a str,
    pub detail: Option<&'a str>,
    pub working_dir: Option<&'a Path>,
}

#[derive(Debug, Clone)]
pub enum NotificationEvent<'a> {
    ApprovalNeeded(ApprovalNotification<'a>),
    TurnFinished(TurnNotification<'a>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalApp {
    Kitty,
    WezTerm,
    Ghostty,
    ITerm2,
    AppleTerminal,
    WindowsTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisibilityPolicy {
    BackgroundOnlyBestEffort,
}

#[derive(Debug, Clone)]
struct NotificationPlan {
    title: Cow<'static, str>,
    body: String,
    terminal_id: &'static str,
    visibility: VisibilityPolicy,
    emit_terminal: bool,
    emit_system: bool,
    emit_bell: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryResult {
    Delivered,
    Unsupported,
    Failed,
}

const FOCUS_UNKNOWN: u8 = 0;
const FOCUS_TRUE: u8 = 1;
const FOCUS_FALSE: u8 = 2;

static TERMINAL_FOCUS_STATE: AtomicU8 = AtomicU8::new(FOCUS_UNKNOWN);

pub fn set_terminal_focus_state(focused: Option<bool>) {
    let encoded = match focused {
        Some(true) => FOCUS_TRUE,
        Some(false) => FOCUS_FALSE,
        None => FOCUS_UNKNOWN,
    };
    TERMINAL_FOCUS_STATE.store(encoded, Ordering::Relaxed);
}

fn terminal_focus_state() -> Option<bool> {
    match TERMINAL_FOCUS_STATE.load(Ordering::Relaxed) {
        FOCUS_TRUE => Some(true),
        FOCUS_FALSE => Some(false),
        _ => None,
    }
}

pub fn notify(cfg: &NotificationConfig, event: NotificationEvent<'_>) {
    let Some(plan) = build_notification_plan(cfg, event) else {
        return;
    };
    dispatch_notification(plan);
}

pub fn notify_turn_finished(cfg: &NotificationConfig, turn: TurnNotification<'_>) {
    notify(cfg, NotificationEvent::TurnFinished(turn));
}

fn build_notification_plan(
    cfg: &NotificationConfig,
    event: NotificationEvent<'_>,
) -> Option<NotificationPlan> {
    if !cfg.enabled {
        return None;
    }
    if cfg.background_only && terminal_focus_state() == Some(true) {
        return None;
    }

    let (title, body, terminal_id, visibility) = match event {
        NotificationEvent::ApprovalNeeded(approval) => {
            let (title, body) = build_approval_notification_text(&approval);
            (
                title,
                body,
                "atomcode-approval",
                VisibilityPolicy::BackgroundOnlyBestEffort,
            )
        }
        NotificationEvent::TurnFinished(turn) => {
            if turn.duration < Duration::from_secs(cfg.min_duration_secs) {
                return None;
            }
            let (title, body) = build_system_notification_text(&turn);
            (
                title,
                body,
                "atomcode-task",
                VisibilityPolicy::BackgroundOnlyBestEffort,
            )
        }
    };

    // Windows 上系统通知走 PowerShell NotifyIcon，实测会让 TUI 闪退，整条通道关掉。
    //
    // For background-only notifications, only use OS-native fallbacks when we
    // know the terminal is actually unfocused. macOS Terminal.app does not feed
    // focus events into our current reader, so its state stays Unknown while the
    // user may still be reading scrollback in the foreground. BEL / terminal
    // protocols can still let the terminal decide how much attention to request.
    let emit_system = cfg.system
        && !cfg!(target_os = "windows")
        && (!cfg.background_only || terminal_focus_state() == Some(false));

    Some(NotificationPlan {
        title,
        body,
        terminal_id,
        visibility,
        emit_terminal: cfg.terminal,
        emit_system,
        emit_bell: cfg.bell,
    })
}

fn dispatch_notification(plan: NotificationPlan) {
    let terminal_result = if plan.emit_terminal {
        deliver_terminal_notification(&plan)
    } else {
        DeliveryResult::Unsupported
    };

    if plan.emit_bell {
        let _ = emit_bell();
    }

    if plan.emit_system && terminal_result != DeliveryResult::Delivered {
        spawn_system_notification(plan.title.into_owned(), plan.body);
    }
}

fn deliver_terminal_notification(plan: &NotificationPlan) -> DeliveryResult {
    match emit_terminal_notification(plan) {
        Ok(true) => DeliveryResult::Delivered,
        Ok(false) => DeliveryResult::Unsupported,
        Err(_) => DeliveryResult::Failed,
    }
}

fn emit_terminal_notification(plan: &NotificationPlan) -> io::Result<bool> {
    let Some(app) = detect_terminal_app() else {
        return Ok(false);
    };
    let mut stdout = io::stdout();
    if stdout.is_terminal() {
        if !write_terminal_notification(&mut stdout, app, plan)? {
            return Ok(false);
        }
        stdout.flush()?;
        return Ok(true);
    }
    let mut stderr = io::stderr();
    if stderr.is_terminal() {
        if !write_terminal_notification(&mut stderr, app, plan)? {
            return Ok(false);
        }
        stderr.flush()?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
fn build_turn_terminal_notification_text(
    app: TerminalApp,
    turn: &TurnNotification<'_>,
) -> (Cow<'static, str>, String) {
    let (title, mut body) = build_system_notification_text(turn);
    if matches!(app, TerminalApp::Kitty | TerminalApp::WezTerm | TerminalApp::Ghostty) {
        if let Some(scope) = turn
            .working_dir
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
        {
            body = format!("{} · {}", scope, body);
        }
    }
    (title, body)
}

fn build_turn_system_notification_text(turn: &TurnNotification<'_>) -> (Cow<'static, str>, String) {
    let title = match turn.stop_reason {
        TurnStopReason::Natural => Cow::Borrowed("AtomCode done"),
        TurnStopReason::Cancelled => Cow::Borrowed("AtomCode cancelled"),
        TurnStopReason::Error => Cow::Borrowed("AtomCode failed"),
        TurnStopReason::TurnLimit => Cow::Borrowed("AtomCode stopped"),
        TurnStopReason::StepLimit => Cow::Borrowed("AtomCode stopped"),
    };
    let status = match turn.stop_reason {
        TurnStopReason::Natural => "Done",
        TurnStopReason::Cancelled => "Cancelled",
        TurnStopReason::Error => "Failed",
        TurnStopReason::TurnLimit => "Stopped",
        TurnStopReason::StepLimit => "Stopped",
    };
    let mut body = format!("{} · {}", status, fmt_duration(turn.duration));
    if turn.turn_count > 0 {
        body.push_str(&format!(" · {} rounds", turn.turn_count));
    }
    if turn.tool_call_count > 0 {
        body.push_str(&format!(" · {} tools", turn.tool_call_count));
    }
    (title, body)
}

fn build_system_notification_text(turn: &TurnNotification<'_>) -> (Cow<'static, str>, String) {
    build_turn_system_notification_text(turn)
}

fn build_approval_notification_text(
    approval: &ApprovalNotification<'_>,
) -> (Cow<'static, str>, String) {
    let title = Cow::Borrowed("AtomCode approval needed");
    let mut body = format!("{} is waiting for Y/A/N", approval.tool_name);
    if let Some(scope) = approval
        .working_dir
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
    {
        body.push_str(&format!(" · {}", scope));
    }
    if let Some(detail) = approval.detail.filter(|s| !s.trim().is_empty()) {
        body.push_str(&format!(" · {}", detail.trim()));
    }
    (title, body)
}

fn fmt_duration(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

fn emit_bell() -> io::Result<bool> {
    let mut stdout = io::stdout();
    if stdout.is_terminal() {
        stdout.write_all(b"\x07")?;
        stdout.flush()?;
        return Ok(true);
    }
    let mut stderr = io::stderr();
    if stderr.is_terminal() {
        stderr.write_all(b"\x07")?;
        stderr.flush()?;
        return Ok(true);
    }
    Ok(false)
}

fn write_terminal_notification(
    out: &mut dyn Write,
    app: TerminalApp,
    plan: &NotificationPlan,
) -> io::Result<bool> {
    match app {
        TerminalApp::Kitty => {
            let title = &plan.title;
            let body = &plan.body;
            write_kitty_notification(out, plan.terminal_id, plan.visibility, title, body)?;
            Ok(true)
        }
        TerminalApp::WezTerm | TerminalApp::Ghostty => {
            let title = &plan.title;
            let body = &plan.body;
            write_osc777_notification(out, title, body)?;
            Ok(true)
        }
        TerminalApp::ITerm2 => {
            let title = &plan.title;
            let body = &plan.body;
            write_iterm2_notification(out, title, body)?;
            Ok(true)
        }
        TerminalApp::AppleTerminal | TerminalApp::WindowsTerminal => Ok(false),
    }
}

fn write_kitty_notification(
    out: &mut dyn Write,
    id: &str,
    visibility: VisibilityPolicy,
    title: &str,
    body: &str,
) -> io::Result<()> {
    let title = sanitize_plain_text(title);
    let body = sanitize_plain_text(body);
    let visibility = match visibility {
        VisibilityPolicy::BackgroundOnlyBestEffort => "unfocused",
    };
    write!(out, "\x1b]99;i={id}:o={visibility}:d=0;{title}\x1b\\")?;
    write!(out, "\x1b]99;i={id}:p=body;{body}\x1b\\")?;
    Ok(())
}

fn write_osc777_notification(out: &mut dyn Write, title: &str, body: &str) -> io::Result<()> {
    let title = sanitize_plain_text(title).replace(';', ":");
    let body = sanitize_plain_text(body).replace(';', ":");
    write!(out, "\x1b]777;notify;{title};{body}\x1b\\")?;
    Ok(())
}

fn write_iterm2_notification(out: &mut dyn Write, title: &str, body: &str) -> io::Result<()> {
    let payload = match (title.trim().is_empty(), body.trim().is_empty()) {
        (false, false) => sanitize_plain_text(&format!("{title}: {body}")),
        (false, true) => sanitize_plain_text(title),
        (true, false) => sanitize_plain_text(body),
        (true, true) => String::from("AtomCode"),
    };
    write!(out, "\x1b]9;{payload}\x1b\\")?;
    Ok(())
}

fn detect_terminal_app() -> Option<TerminalApp> {
    if std::env::var_os("KITTY_WINDOW_ID").is_some() {
        return Some(TerminalApp::Kitty);
    }
    if std::env::var_os("WEZTERM_PANE").is_some() {
        return Some(TerminalApp::WezTerm);
    }
    if std::env::var_os("WT_SESSION").is_some() {
        return Some(TerminalApp::WindowsTerminal);
    }

    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    if term_program.eq_ignore_ascii_case("wezterm") {
        return Some(TerminalApp::WezTerm);
    }
    if term_program.eq_ignore_ascii_case("ghostty") {
        return Some(TerminalApp::Ghostty);
    }
    if term_program == "iTerm.app" || term_program.eq_ignore_ascii_case("iTerm2") {
        return Some(TerminalApp::ITerm2);
    }
    if term_program.eq_ignore_ascii_case("apple_terminal")
        || term_program.eq_ignore_ascii_case("terminal.app")
        || term_program.eq_ignore_ascii_case("terminal")
    {
        return Some(TerminalApp::AppleTerminal);
    }
    if term_program.eq_ignore_ascii_case("windows_terminal") {
        return Some(TerminalApp::WindowsTerminal);
    }

    let lc_terminal = std::env::var("LC_TERMINAL").unwrap_or_default();
    if lc_terminal.eq_ignore_ascii_case("iTerm2") {
        return Some(TerminalApp::ITerm2);
    }
    if lc_terminal.eq_ignore_ascii_case("Terminal") {
        return Some(TerminalApp::AppleTerminal);
    }

    let term = std::env::var("TERM").unwrap_or_default();
    if term.contains("kitty") {
        return Some(TerminalApp::Kitty);
    }

    None
}

fn sanitize_plain_text(s: &str) -> String {
    s.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(target_os = "macos")]
fn macos_terminal_bundle_id(app: Option<TerminalApp>) -> Option<&'static str> {
    match app {
        Some(TerminalApp::AppleTerminal) => Some("com.apple.Terminal"),
        Some(TerminalApp::ITerm2) => Some("com.googlecode.iterm2"),
        Some(TerminalApp::WezTerm) => Some("com.github.wez.wezterm"),
        Some(TerminalApp::Ghostty) => Some("com.mitchellh.ghostty"),
        Some(TerminalApp::Kitty) => Some("net.kovidgoyal.kitty"),
        _ => None,
    }
}

// Only the macOS branch of `spawn_system_notification` calls this (to
// find `terminal-notifier`). Linux uses notify-send unconditionally and
// Windows shells out to powershell.exe — neither needs PATH lookup.
// Kept callable on every platform because `missing_executable_lookup_
// returns_none` is a portable unit test of PATH-iteration semantics.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn find_executable_on_path(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn spawn_system_notification(title: String, body: String) {
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        {
            if let Some(bin) = find_executable_on_path("terminal-notifier") {
                let mut cmd = Command::new(bin);
                cmd.arg("-title")
                    .arg(&title)
                    .arg("-message")
                    .arg(&body)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                if let Some(bundle_id) = macos_terminal_bundle_id(detect_terminal_app()) {
                    cmd.arg("-activate").arg(bundle_id);
                }
                if cmd.spawn().is_ok() {
                    return;
                }
            }

            let script = format!(
                "display notification {} with title {}",
                apple_script_string(&body),
                apple_script_string(&title)
            );
            let _ = Command::new("osascript")
                .arg("-e")
                .arg(script)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }

        #[cfg(target_os = "linux")]
        {
            let _ = Command::new("notify-send")
                .arg(&title)
                .arg(&body)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }

        #[cfg(target_os = "windows")]
        {
            let script = format!(
                "Add-Type -AssemblyName System.Windows.Forms; \
                 Add-Type -AssemblyName System.Drawing; \
                 $n = New-Object System.Windows.Forms.NotifyIcon; \
                 $n.Icon = [System.Drawing.SystemIcons]::Information; \
                 $n.BalloonTipTitle = '{}'; \
                 $n.BalloonTipText = '{}'; \
                 $n.Visible = $true; \
                 $n.ShowBalloonTip(5000); \
                 Start-Sleep -Milliseconds 5500; \
                 $n.Dispose();",
                powershell_string_literal(&title),
                powershell_string_literal(&body),
            );
            let _ = Command::new("powershell.exe")
                .arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-WindowStyle")
                .arg("Hidden")
                .arg("-Command")
                .arg(script)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    });
}

#[cfg(target_os = "macos")]
fn apple_script_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "windows")]
fn powershell_string_literal(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn focus_state_test_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    #[test]
    fn builds_human_readable_notification_text() {
        let (title, body) = build_system_notification_text(&TurnNotification {
            duration: Duration::from_secs(12),
            turn_count: 3,
            tool_call_count: 5,
            total_tokens: Some(4321),
            stop_reason: TurnStopReason::Natural,
            working_dir: Some(Path::new("/tmp/demo")),
        });
        assert_eq!(title, "AtomCode done");
        assert_eq!(body, "Done · 12.0s · 3 rounds · 5 tools");
    }

    #[test]
    fn terminal_text_is_compact_for_iterm() {
        let (title, body) = build_turn_terminal_notification_text(
            TerminalApp::ITerm2,
            &TurnNotification {
                duration: Duration::from_secs(49),
                turn_count: 4,
                tool_call_count: 9,
                total_tokens: Some(1209),
                stop_reason: TurnStopReason::Natural,
                working_dir: Some(Path::new("/tmp/atomcode")),
            },
        );
        assert_eq!(title, "AtomCode done");
        assert_eq!(body, "Done · 49.0s · 4 rounds · 9 tools");
    }

    #[test]
    fn terminal_text_keeps_scope_for_split_title_body_protocols() {
        let (_title, body) = build_turn_terminal_notification_text(
            TerminalApp::WezTerm,
            &TurnNotification {
                duration: Duration::from_secs(12),
                turn_count: 3,
                tool_call_count: 5,
                total_tokens: None,
                stop_reason: TurnStopReason::Natural,
                working_dir: Some(Path::new("/tmp/demo")),
            },
        );
        assert!(body.contains("3 rounds"));
        assert!(body.contains("5 tools"));
        assert!(body.starts_with("demo · Done"));
    }

    #[test]
    fn approval_notification_is_action_oriented() {
        let (title, body) = build_approval_notification_text(&ApprovalNotification {
            tool_name: "Bash",
            detail: Some("ls -la ~/.ssh/"),
            working_dir: Some(Path::new("/tmp/demo")),
        });
        assert_eq!(title, "AtomCode approval needed");
        assert!(body.contains("Bash is waiting for Y/A/N"));
        assert!(body.contains("demo"));
        assert!(body.contains("ls -la ~/.ssh/"));
    }

    #[test]
    fn background_only_is_preserved_for_approval_notifications() {
        let plan = NotificationPlan {
            title: Cow::Borrowed("AtomCode approval needed"),
            body: "Bash is waiting for Y/A/N".into(),
            terminal_id: "atomcode-approval",
            visibility: VisibilityPolicy::BackgroundOnlyBestEffort,
            emit_terminal: true,
            emit_system: true,
            emit_bell: true,
        };
        let mut out = Vec::new();
        assert!(write_terminal_notification(&mut out, TerminalApp::Kitty, &plan).unwrap());
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.contains(":o=unfocused:"));
    }

    #[test]
    fn iterm2_uses_osc9_notification_sequence() {
        let plan = NotificationPlan {
            title: Cow::Borrowed("AtomCode approval needed"),
            body: "Bash is waiting for Y/A/N".into(),
            terminal_id: "atomcode-approval",
            visibility: VisibilityPolicy::BackgroundOnlyBestEffort,
            emit_terminal: true,
            emit_system: true,
            emit_bell: true,
        };
        let mut out = Vec::new();
        assert!(write_terminal_notification(&mut out, TerminalApp::ITerm2, &plan).unwrap());
        let rendered = String::from_utf8(out).unwrap();
        assert!(rendered.starts_with("\u{1b}]9;"));
        assert!(rendered.ends_with("\u{1b}\\"));
    }

    #[test]
    fn apple_terminal_has_no_native_terminal_notification_path() {
        let plan = NotificationPlan {
            title: Cow::Borrowed("AtomCode done"),
            body: "Done · 12.0s".into(),
            terminal_id: "atomcode-task",
            visibility: VisibilityPolicy::BackgroundOnlyBestEffort,
            emit_terminal: true,
            emit_system: true,
            emit_bell: true,
        };
        let mut out = Vec::new();
        assert!(!write_terminal_notification(&mut out, TerminalApp::AppleTerminal, &plan).unwrap());
        assert!(out.is_empty());
    }

    #[test]
    fn turn_finished_below_threshold_is_suppressed_by_policy() {
        let cfg = NotificationConfig::default();
        let plan = build_notification_plan(
            &cfg,
            NotificationEvent::TurnFinished(TurnNotification {
                duration: Duration::from_secs(2),
                turn_count: 1,
                tool_call_count: 1,
                total_tokens: None,
                stop_reason: TurnStopReason::Natural,
                working_dir: Some(Path::new("/tmp/demo")),
            }),
        );
        assert!(plan.is_none());
    }

    #[test]
    fn approval_event_ignores_duration_threshold() {
        let cfg = NotificationConfig::default();
        let plan = build_notification_plan(
            &cfg,
            NotificationEvent::ApprovalNeeded(ApprovalNotification {
                tool_name: "Bash",
                detail: Some("ls -la ~/.ssh/"),
                working_dir: Some(Path::new("/tmp/demo")),
            }),
        )
        .unwrap();
        assert_eq!(plan.terminal_id, "atomcode-approval");
        assert_eq!(plan.visibility, VisibilityPolicy::BackgroundOnlyBestEffort);
    }

    #[test]
    fn focused_terminal_suppresses_background_only_notifications() {
        let _guard = focus_state_test_lock();
        let cfg = NotificationConfig::default();
        set_terminal_focus_state(Some(true));
        let plan = build_notification_plan(
            &cfg,
            NotificationEvent::ApprovalNeeded(ApprovalNotification {
                tool_name: "Bash",
                detail: Some("ls -la ~/.ssh/"),
                working_dir: Some(Path::new("/tmp/demo")),
            }),
        );
        set_terminal_focus_state(None);
        assert!(plan.is_none());
    }

    #[test]
    fn background_only_unknown_focus_suppresses_system_fallback() {
        let _guard = focus_state_test_lock();
        let cfg = NotificationConfig::default();
        set_terminal_focus_state(None);
        let plan = build_notification_plan(
            &cfg,
            NotificationEvent::TurnFinished(TurnNotification {
                duration: Duration::from_secs(12),
                turn_count: 1,
                tool_call_count: 1,
                total_tokens: None,
                stop_reason: TurnStopReason::Natural,
                working_dir: Some(Path::new("/tmp/demo")),
            }),
        )
        .unwrap();

        assert!(plan.emit_terminal);
        assert!(plan.emit_bell);
        assert!(!plan.emit_system);
    }

    #[test]
    fn background_only_unfocused_allows_system_fallback() {
        let _guard = focus_state_test_lock();
        let cfg = NotificationConfig::default();
        set_terminal_focus_state(Some(false));
        let plan = build_notification_plan(
            &cfg,
            NotificationEvent::TurnFinished(TurnNotification {
                duration: Duration::from_secs(12),
                turn_count: 1,
                tool_call_count: 1,
                total_tokens: None,
                stop_reason: TurnStopReason::Natural,
                working_dir: Some(Path::new("/tmp/demo")),
            }),
        )
        .unwrap();
        set_terminal_focus_state(None);

        assert_eq!(plan.emit_system, !cfg!(target_os = "windows"));
    }

    #[test]
    fn non_background_only_keeps_system_fallback_for_unknown_focus() {
        let _guard = focus_state_test_lock();
        let mut cfg = NotificationConfig::default();
        cfg.background_only = false;
        set_terminal_focus_state(None);
        let plan = build_notification_plan(
            &cfg,
            NotificationEvent::TurnFinished(TurnNotification {
                duration: Duration::from_secs(12),
                turn_count: 1,
                tool_call_count: 1,
                total_tokens: None,
                stop_reason: TurnStopReason::Natural,
                working_dir: Some(Path::new("/tmp/demo")),
            }),
        )
        .unwrap();

        assert_eq!(plan.emit_system, !cfg!(target_os = "windows"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_terminal_bundle_ids_match_supported_terminals() {
        assert_eq!(
            macos_terminal_bundle_id(Some(TerminalApp::AppleTerminal)),
            Some("com.apple.Terminal")
        );
        assert_eq!(
            macos_terminal_bundle_id(Some(TerminalApp::ITerm2)),
            Some("com.googlecode.iterm2")
        );
        assert_eq!(
            macos_terminal_bundle_id(Some(TerminalApp::WezTerm)),
            Some("com.github.wez.wezterm")
        );
        assert_eq!(
            macos_terminal_bundle_id(Some(TerminalApp::Ghostty)),
            Some("com.mitchellh.ghostty")
        );
        assert_eq!(
            macos_terminal_bundle_id(Some(TerminalApp::Kitty)),
            Some("net.kovidgoyal.kitty")
        );
    }

    #[test]
    fn missing_executable_lookup_returns_none() {
        assert!(find_executable_on_path("__atomcode_missing_notifier__").is_none());
    }

    #[test]
    fn control_chars_are_removed_from_payloads() {
        let s = sanitize_plain_text("hi\x07 there\nnext\x1b");
        assert_eq!(s, "hi there next");
    }
}
