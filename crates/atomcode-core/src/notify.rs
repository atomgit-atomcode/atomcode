use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalProtocol {
    Kitty,
    WezTerm,
    ITerm2,
}

pub fn notify_turn_finished(cfg: &NotificationConfig, turn: TurnNotification<'_>) {
    if !cfg.enabled || turn.duration < Duration::from_secs(cfg.min_duration_secs) {
        return;
    }

    let (title, body) = build_system_notification_text(&turn);
    let emitted_terminal = if cfg.terminal {
        emit_terminal_notification(cfg, &turn).unwrap_or(false)
    } else {
        false
    };

    if cfg.bell {
        let _ = emit_bell();
    }

    if cfg.system && !emitted_terminal {
        spawn_system_notification(title.into_owned(), body);
    }
}

fn build_system_notification_text(turn: &TurnNotification<'_>) -> (Cow<'static, str>, String) {
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

fn build_terminal_notification_text(
    protocol: TerminalProtocol,
    turn: &TurnNotification<'_>,
) -> (Cow<'static, str>, String) {
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
    if matches!(protocol, TerminalProtocol::Kitty | TerminalProtocol::WezTerm) {
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

fn fmt_duration(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1000 {
        format!("{}ms", ms)
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

fn emit_terminal_notification(cfg: &NotificationConfig, turn: &TurnNotification<'_>) -> io::Result<bool> {
    let Some(protocol) = detect_terminal_protocol() else {
        return Ok(false);
    };
    let (title, body) = build_terminal_notification_text(protocol, turn);
    let mut stdout = io::stdout();
    if stdout.is_terminal() {
        write_terminal_notification(&mut stdout, protocol, cfg, &title, &body)?;
        stdout.flush()?;
        return Ok(true);
    }
    let mut stderr = io::stderr();
    if stderr.is_terminal() {
        write_terminal_notification(&mut stderr, protocol, cfg, &title, &body)?;
        stderr.flush()?;
        return Ok(true);
    }
    Ok(false)
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
    protocol: TerminalProtocol,
    cfg: &NotificationConfig,
    title: &str,
    body: &str,
) -> io::Result<()> {
    match protocol {
        TerminalProtocol::Kitty => {
            let id = "atomcode-task";
            let title = sanitize_plain_text(title);
            let body = sanitize_plain_text(body);
            let visibility = if cfg.background_only { "unfocused" } else { "always" };
            write!(out, "\x1b]99;i={id}:o={visibility}:d=0;{title}\x1b\\")?;
            write!(out, "\x1b]99;i={id}:p=body;{body}\x1b\\")?;
        }
        TerminalProtocol::WezTerm => {
            let title = sanitize_plain_text(title).replace(';', ":");
            let body = sanitize_plain_text(body).replace(';', ":");
            write!(out, "\x1b]777;notify;{title};{body}\x1b\\")?;
        }
        TerminalProtocol::ITerm2 => {
            let msg = sanitize_plain_text(&format!("{title} - {body}"));
            write!(out, "\x1b]9;{msg}\x07")?;
        }
    }
    Ok(())
}

fn detect_terminal_protocol() -> Option<TerminalProtocol> {
    let term = std::env::var("TERM").unwrap_or_default();
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let lc_terminal = std::env::var("LC_TERMINAL").unwrap_or_default();
    if std::env::var_os("KITTY_WINDOW_ID").is_some() || term.contains("kitty") {
        return Some(TerminalProtocol::Kitty);
    }
    if std::env::var_os("WEZTERM_PANE").is_some() || term_program.eq_ignore_ascii_case("wezterm") {
        return Some(TerminalProtocol::WezTerm);
    }
    if term_program == "iTerm.app" || lc_terminal.eq_ignore_ascii_case("iTerm2") {
        return Some(TerminalProtocol::ITerm2);
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

fn spawn_system_notification(title: String, body: String) {
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        {
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
            let _ = (title, body);
        }
    });
}

#[cfg(target_os = "macos")]
fn apple_script_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (title, body) = build_terminal_notification_text(
            TerminalProtocol::ITerm2,
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
        let (_title, body) = build_terminal_notification_text(
            TerminalProtocol::WezTerm,
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
    fn control_chars_are_removed_from_payloads() {
        let s = sanitize_plain_text("hi\x07 there\nnext\x1b");
        assert_eq!(s, "hi there next");
    }
}
