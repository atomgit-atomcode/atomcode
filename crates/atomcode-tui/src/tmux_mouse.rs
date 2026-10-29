//! Respect the current tmux pane's mouse policy with a bounded helper and no
//! terminal-input reader. Only an explicit off answer disables capture.

pub(crate) fn disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| disabled_for(|key| std::env::var(key).ok(), query))
}

fn disabled_for(
    env: impl Fn(&str) -> Option<String>,
    mut query: impl FnMut(&[&str]) -> Option<String>,
) -> bool {
    let pane = env("TMUX_PANE").filter(|value| !value.is_empty());
    if pane.is_none() && env("TMUX").is_none_or(|value| value.is_empty()) {
        return false;
    }
    let mut args = vec!["display-message", "-p"];
    if let Some(pane) = pane.as_deref() {
        args.extend(["-t", pane]);
    }
    args.push("#{mouse}");
    query(&args).is_some_and(|answer| matches!(answer.trim(), "0" | "off"))
}

fn query(args: &[&str]) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut output = tempfile::tempfile().ok()?;
    let mut child = Command::new("tmux")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone().ok()?))
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(150);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    if output.metadata().ok()?.len() > 32 {
        return None;
    }
    output.seek(SeekFrom::Start(0)).ok()?;
    let mut answer = String::new();
    output.take(32).read_to_string(&mut answer).ok()?;
    Some(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_terminals_never_spawn_a_tmux_query() {
        assert!(!disabled_for(|_| None, |_| panic!("not in tmux")));
        assert!(!disabled_for(
            |_| Some(String::new()),
            |_| panic!("empty markers")
        ));
    }

    #[test]
    fn only_a_confirmed_off_answer_blocks_mouse_capture() {
        for answer in [
            Some("0\n"),
            Some("off\n"),
            Some("1\n"),
            Some("on\n"),
            Some(""),
            Some("unknown"),
            None,
        ] {
            let blocked = disabled_for(
                |key| (key == "TMUX").then(|| "/socket,123,0".into()),
                |args| {
                    assert_eq!(args, &["display-message", "-p", "#{mouse}"]);
                    answer.map(String::from)
                },
            );
            assert_eq!(blocked, matches!(answer, Some("0\n" | "off\n")));
        }
    }

    #[test]
    fn queries_the_current_pane_instead_of_only_global_defaults() {
        assert!(disabled_for(
            |key| (key == "TMUX_PANE").then(|| "%17".into()),
            |args| {
                assert_eq!(args, &["display-message", "-p", "-t", "%17", "#{mouse}"]);
                Some("off".into())
            }
        ));
    }
}
