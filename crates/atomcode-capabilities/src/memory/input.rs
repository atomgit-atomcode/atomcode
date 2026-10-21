//! Slash-command syntax only. Storage, validation and mutations stay in MemoryTool.
use atomcode_i18n::product::{t, Msg};
use serde_json::{json, Value};

pub struct MemoryInput {
    pub call: Value,
    pub json: bool,
}
impl MemoryInput {
    pub fn is_listing(&self) -> bool {
        self.call["action"] == "list"
    }
}
fn word(input: &str) -> Result<(String, &str), String> {
    let input = input.trim_start();
    if let Some(quote) = input.chars().next().filter(|c| *c == '\'' || *c == '"') {
        let end = input[1..]
            .find(quote)
            .ok_or_else(|| t(Msg::MemoryArgUnclosedQuote).into_owned())?
            + 1;
        let tail = &input[end + 1..];
        if !tail.is_empty() && !tail.starts_with(char::is_whitespace) {
            return Err(t(Msg::MemoryArgQuoteSpacing).into_owned());
        }
        return Ok((input[1..end].into(), tail.trim_start()));
    }
    let (head, tail) = input.split_once(char::is_whitespace).unwrap_or((input, ""));
    Ok((head.into(), tail.trim_start()))
}
pub fn parse(base: &str, input: &str) -> Result<MemoryInput, String> {
    let mut call = json!({ "action": base });
    let mut json_output = false;
    let mut action = base.to_string();
    let mut subcommand = base != "list";
    let mut rest = input.trim();
    while !rest.is_empty() {
        let (token, tail) = word(rest)?;
        if token == "--" {
            rest = tail;
            break;
        }
        if token.starts_with("--") || token == "-g" {
            rest = tail;
            match token.as_str() {
                "--global" | "-g" => call["scope"] = json!("global"),
                "--project" => call["scope"] = json!("project"),
                "--local" => call["scope"] = json!("local"),
                "--pin" => call["pinned"] = json!(true),
                "--unpin" => call["pinned"] = json!(false),
                "--json" => json_output = true,
                "--expired" => call["expired"] = json!(true),
                "--clear-expiry" => call["clear_expiry"] = json!(true),
                "--ttl-days" | "--expiring-within" => {
                    let (value, tail) = word(rest)?;
                    let days = value
                        .parse::<u32>()
                        .map_err(|_| t(Msg::MemoryInvalidDays).into_owned())?;
                    super::entry::validate_days(days)
                        .map_err(|_| t(Msg::MemoryInvalidDays).into_owned())?;
                    let key = if token == "--ttl-days" {
                        "ttl_days"
                    } else {
                        "expiring_within_days"
                    };
                    call[key] = json!(days);
                    rest = tail;
                }
                "--clear-topic" => call["topic"] = json!(""),
                "--clear-source" => call["source"] = json!(""),
                "--topic" | "--source" | "--status" | "--id" => {
                    let (value, tail) = word(rest)?;
                    if value.is_empty() || value.starts_with("--") {
                        return Err(t(Msg::MemoryArgNeedsValue { arg: &token }).into_owned());
                    }
                    call[token.trim_start_matches("--")] = json!(value);
                    rest = tail;
                }
                _ => return Err(t(Msg::MemoryArgUnknown { arg: &token }).into_owned()),
            }
            continue;
        }
        if !subcommand {
            if !matches!(
                token.as_str(),
                "list" | "update" | "pin" | "unpin" | "deprecate"
            ) {
                return Err(t(Msg::MemoryCommandSyntax).into_owned());
            }
            action = if token == "unpin" {
                "pin".into()
            } else {
                token.clone()
            };
            if token == "unpin" {
                call["pinned"] = json!(false);
            }
            subcommand = true;
            rest = tail;
            continue;
        }
        if matches!(action.as_str(), "update" | "pin" | "deprecate") && call.get("id").is_none() {
            call["id"] = json!(token);
            rest = tail;
            continue;
        }
        break;
    }
    if base == "forget" && call.get("id").is_some() {
        if !rest.is_empty() {
            return Err(t(Msg::MemoryIdKeywordConflict).into_owned());
        }
        action = "deprecate".into();
    }
    if !rest.is_empty() {
        match action.as_str() {
            "remember" | "update" => call["content"] = json!(rest.trim()),
            "forget" => call["keyword"] = json!(rest.trim()),
            _ => return Err(t(Msg::MemoryUnexpectedContent).into_owned()),
        }
    }
    if call.get("status").is_some_and(|status| {
        !matches!(status.as_str(), Some("active" | "outdated" | "deprecated"))
    }) {
        return Err(t(Msg::MemoryInvalidStatus).into_owned());
    }
    if json_output && action != "list" {
        return Err(t(Msg::MemoryJsonListOnly).into_owned());
    }
    if action == "list"
        && (call.get("source").is_some()
            || call.get("pinned").is_some()
            || call.get("id").is_some())
    {
        return Err(t(Msg::MemoryListFilters).into_owned());
    }
    if base != "forget" && action != "list" && call.get("id").is_none() && action != "remember" {
        return Err(t(Msg::MemoryIdRequired).into_owned());
    }
    if action == "remember" && call.get("content").is_none() {
        return Err(t(Msg::MemoryRememberRequired).into_owned());
    }
    if action == "forget" && call.get("keyword").is_none() {
        return Err(t(Msg::MemoryForgetRequired).into_owned());
    }
    if ((!matches!(action.as_str(), "remember" | "update"))
        && (call.get("ttl_days").is_some() || call.get("clear_expiry").is_some()))
        || (action != "list"
            && (call.get("expired").is_some() || call.get("expiring_within_days").is_some()))
        || (call.get("expired").is_some() && call.get("expiring_within_days").is_some())
        || (call.get("ttl_days").is_some() && call.get("clear_expiry").is_some())
        || (action == "remember" && call.get("clear_expiry").is_some())
    {
        return Err(t(Msg::MemoryExpiryConflict).into_owned());
    }
    call["action"] = json!(action);
    Ok(MemoryInput {
        call,
        json: json_output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_flags_and_content_remain_separate() {
        let input = parse(
            "remember",
            "--global --topic 'runtime ownership' --pin uses one owner --literal",
        )
        .unwrap();
        assert_eq!(input.call["scope"], "global");
        assert_eq!(input.call["topic"], "runtime ownership");
        assert_eq!(input.call["pinned"], true);
        assert_eq!(input.call["content"], "uses one owner --literal");
        let input = parse("remember", "-- --pin is literal").unwrap();
        assert_eq!(input.call["content"], "--pin is literal");
    }
    #[test]
    fn exact_commands_are_scoped_and_reversible() {
        let input = parse(
            "list",
            "--local update m-123 --status active corrected fact",
        )
        .unwrap();
        assert_eq!(
            input.call,
            json!({"action":"update","scope":"local","id":"m-123","status":"active","content":"corrected fact"})
        );
        assert_eq!(parse("list", "unpin m-123").unwrap().call["pinned"], false);
        assert_eq!(
            parse("forget", "--id m-123").unwrap().call["action"],
            "deprecate"
        );
    }
    #[test]
    fn invalid_or_ambiguous_operations_fail_before_writing() {
        for args in [
            "pin",
            "--status future",
            "--unknown",
            "--topic",
            "deprecate m-123 content",
            "--json pin m-123",
        ] {
            assert!(parse("list", args).is_err(), "{args}");
        }
        assert!(parse("forget", "--id m-123 keyword").is_err());
        assert!(parse("remember", "--pin").is_err());
    }
}

#[cfg(test)]
mod expiry_tests {
    use super::*;
    #[test]
    fn expiry_commands_share_validation_and_preserve_action_classification() {
        assert_eq!(
            parse("remember", "--ttl-days 7 temporary fact")
                .unwrap()
                .call["ttl_days"],
            7
        );
        assert!(!parse("list", "update m-one --clear-expiry")
            .unwrap()
            .is_listing());
        assert_eq!(
            parse("list", "--expired --json").unwrap().call["expired"],
            true
        );
        assert_eq!(
            parse("list", "--expiring-within 7 --topic runtime")
                .unwrap()
                .call["expiring_within_days"],
            7
        );
        for input in [
            "--ttl-days 7",
            "--expiring-within 0",
            "--expiring-within -1",
            "--expiring-within 36501",
            "--expired --expiring-within 7",
            "pin m-one --ttl-days 7",
            "update m-one --ttl-days 7 --clear-expiry",
        ] {
            assert!(parse("list", input).is_err(), "{input}");
        }
    }
}
