// crates/atomcode-tuix/src/event_loop/loop_parse.rs
//
// Parser for the `/loop` command argument string.
// Handles interval mode (e.g. "5m /diff"), self-paced mode (bare prompt),
// stop aliases, and status queries.

#[derive(Debug, PartialEq)]
pub enum LoopArg {
    Interval { secs: u64, payload: String },
    SelfPaced { prompt: String },
    Stop,
    Status,
    Error(String),
}

/// Parse the text after `/loop `. Empty → Status.
pub fn parse_loop_arg(arg: &str) -> LoopArg {
    let t = arg.trim();
    if t.is_empty() || t == "status" {
        return LoopArg::Status;
    }
    if matches!(t, "stop" | "off" | "clear" | "cancel" | "reset" | "none") {
        return LoopArg::Stop;
    }
    let (head, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
    if let Some(secs) = parse_interval(head) {
        let payload = rest.trim();
        if payload.is_empty() {
            return LoopArg::Error(
                "用法：/loop <间隔> <prompt 或 /命令>，例 /loop 5m /diff".into(),
            );
        }
        if payload.split_whitespace().next() == Some("/loop") {
            return LoopArg::Error("不能对 /loop 自身循环".into());
        }
        if !(10..=86_400).contains(&secs) {
            return LoopArg::Error("间隔需在 10s–24h 之间".into());
        }
        return LoopArg::Interval { secs, payload: payload.to_string() };
    }
    if t.split_whitespace().next() == Some("/loop") {
        return LoopArg::Error("不能对 /loop 自身循环".into());
    }
    LoopArg::SelfPaced { prompt: t.to_string() }
}

/// `30s` / `5m` / `1h` → seconds. None if not an interval token.
fn parse_interval(tok: &str) -> Option<u64> {
    let pos = tok.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = tok.split_at(pos);
    let n: u64 = num.parse().ok()?;
    match unit {
        "s" => Some(n),
        "m" => Some(n * 60),
        "h" => Some(n * 3600),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interval_and_slash() {
        assert_eq!(
            parse_loop_arg("5m /diff"),
            LoopArg::Interval { secs: 300, payload: "/diff".into() }
        );
    }

    #[test]
    fn parses_interval_and_prompt() {
        assert_eq!(
            parse_loop_arg("30s check build"),
            LoopArg::Interval { secs: 30, payload: "check build".into() }
        );
    }

    #[test]
    fn bare_prompt_is_self_paced() {
        assert_eq!(
            parse_loop_arg("watch CI until green"),
            LoopArg::SelfPaced { prompt: "watch CI until green".into() }
        );
    }

    #[test]
    fn interval_without_payload_is_error() {
        assert!(matches!(parse_loop_arg("5m"), LoopArg::Error(_)));
    }

    #[test]
    fn out_of_range_interval_is_error() {
        assert!(matches!(parse_loop_arg("5s x"), LoopArg::Error(_)));
        assert!(matches!(parse_loop_arg("25h x"), LoopArg::Error(_)));
    }

    #[test]
    fn stop_aliases() {
        for a in ["stop", "off", "clear", "cancel", "reset", "none"] {
            assert_eq!(parse_loop_arg(a), LoopArg::Stop);
        }
    }

    #[test]
    fn self_referential_rejected() {
        assert!(matches!(parse_loop_arg("5m /loop x"), LoopArg::Error(_)));
    }

    #[test]
    fn empty_is_status() {
        assert_eq!(parse_loop_arg(""), LoopArg::Status);
        assert_eq!(parse_loop_arg("status"), LoopArg::Status);
    }
}
