//! Scrub paths and truncate strings before events leave the process.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

pub const HEAD_MAX: usize = 200;

/// Redact common secret shapes (API tokens, keys, auth-header credentials) from
/// a string before it leaves the process in telemetry. Best-effort and
/// conservative: pattern-based, biased toward redacting known secret formats
/// while leaving ordinary tool arguments intact. This is a BACKSTOP for when a
/// tool call embeds a secret (e.g. `curl -H "Authorization: token ghp_…"` or a
/// `powershell $token = 'ghp_…'`) — truncation-by-length alone does not hide a
/// token that sits near the start of an argument.
///
/// Two passes:
/// 1. key/value credentials — `authorization:`, `api_key=`, `password:` … keep
///    the key, redact the value.
/// 2. known standalone token shapes anywhere in the string — GitHub/GitLab/AWS/
///    Google/Slack/OpenAI-style keys and JWTs — redact the whole token.
pub fn redact_secrets(s: &str) -> String {
    static KV: OnceLock<Regex> = OnceLock::new();
    static TOKENS: OnceLock<Regex> = OnceLock::new();
    // Keep the key + separator (+ optional `bearer`/`token` scheme word) in
    // group 1; the credential in group 2 is dropped for `<REDACTED>`.
    let kv = KV.get_or_init(|| {
        Regex::new(
            r#"(?i)(\b(?:authorization|x-api-key|api[-_]?key|access[-_]?token|private[-_]?token|auth[-_]?token|password|passwd|secret)\b\s*[:=]\s*"?(?:bearer\s+|token\s+)?)([^"'\s,;)]{6,})"#,
        )
        .expect("valid kv secret regex")
    });
    // Each branch starts at a word boundary so we don't clip an internal
    // substring of an ordinary hyphenated word (e.g. `sk-` inside `task-mgmt`).
    let tokens = TOKENS.get_or_init(|| {
        Regex::new(
            r#"\b(?:gh[opsru]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9\-_]{16,}|xox[baprs]-[A-Za-z0-9\-]{10,}|sk-[A-Za-z0-9\-_]{16,}|AKIA[0-9A-Z]{16}|ASIA[0-9A-Z]{16}|AIza[0-9A-Za-z\-_]{35}|eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{6,})"#,
        )
        .expect("valid token secret regex")
    });
    let step1 = kv.replace_all(s, "${1}<REDACTED>");
    tokens.replace_all(&step1, "<REDACTED>").into_owned()
}

pub fn scrub_path(s: &str, home: Option<&Path>, cwd: Option<&Path>) -> String {
    let mut out = s.to_string();
    if let Some(h) = home.and_then(|p| p.to_str()) {
        if !h.is_empty() {
            out = out.replace(h, "<HOME>");
        }
    }
    if let Some(c) = cwd.and_then(|p| p.to_str()) {
        if !c.is_empty() {
            out = out.replace(c, "<CWD>");
        }
    }
    out
}

pub fn truncate_head(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

pub fn backtrace_top_k(bt: &str, k: usize, home: Option<&Path>, cwd: Option<&Path>) -> Vec<String> {
    bt.lines()
        .take(k)
        .map(|line| {
            let scrubbed = scrub_path(line, home, cwd);
            if let Some(idx) = scrubbed.find(" at ") {
                let (head, rest) = scrubbed.split_at(idx + 4);
                let short = match rest.rsplit_once('/') {
                    Some((_, tail)) => tail.to_string(),
                    None => rest.to_string(),
                };
                format!("{}{}", head, short)
            } else {
                scrubbed
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn scrubs_home_path() {
        let home = PathBuf::from("/Users/lichao");
        let s = "panic at /Users/lichao/project/foo.rs:10";
        assert_eq!(
            scrub_path(s, Some(&home), None),
            "panic at <HOME>/project/foo.rs:10"
        );
    }

    #[test]
    fn scrubs_cwd_path() {
        let cwd = PathBuf::from("/tmp/proj");
        let s = "error in /tmp/proj/src/main.rs:3";
        assert_eq!(
            scrub_path(s, None, Some(&cwd)),
            "error in <CWD>/src/main.rs:3"
        );
    }

    #[test]
    fn truncate_head_respects_char_boundary() {
        let s = "中文字串";
        let out = truncate_head(s, 3);
        assert_eq!(out, "中文字");
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn truncate_head_returns_full_when_short() {
        assert_eq!(truncate_head("hi", 200), "hi");
    }

    #[test]
    fn redacts_github_token_in_curl_auth_header() {
        // The reported leak: a GitHub PAT inside a bash curl command.
        let s = r#"bash(command=curl -s -H "Authorization: token ghp_Qq1234567890abcdefghijKLMNOP0987654321")"#;
        let out = redact_secrets(s);
        assert!(
            !out.contains("ghp_Qq1234567890"),
            "token must be gone: {out}"
        );
        assert!(
            out.contains("<REDACTED>"),
            "expected redaction marker: {out}"
        );
    }

    #[test]
    fn redacts_github_token_in_powershell_single_quotes() {
        // Second reported form: powershell `$token = 'ghp_…'`. The token is
        // caught by the standalone-shape pass even though the KV pass can't
        // reach past the single quote.
        let s =
            "bash(command=powershell -Command \"$token = 'ghp_Qq1234567890abcdefghijKLMNOP0987'\")";
        let out = redact_secrets(s);
        assert!(
            !out.contains("ghp_Qq1234567890"),
            "token must be gone: {out}"
        );
    }

    #[test]
    fn redacts_various_known_token_shapes() {
        for secret in [
            "sk-ant-api03-abcdef_ghijklmnopqrstuvwx-YZ0123456789",
            "AKIAIOSFODNN7EXAMPLE",
            "glpat-abcdefghij1234567890",
            "xoxb-1234567890-abcdefghij",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
        ] {
            let out = redact_secrets(&format!("run with {secret} now"));
            assert!(!out.contains(secret), "should redact {secret}: {out}");
            assert!(out.contains("<REDACTED>"));
        }
    }

    #[test]
    fn redacts_key_value_credentials() {
        assert!(!redact_secrets("api_key=supersecretvalue123").contains("supersecretvalue123"));
        assert!(!redact_secrets("password: hunter2hunter2").contains("hunter2hunter2"));
        assert!(redact_secrets("api_key=supersecretvalue123").contains("api_key="));
    }

    #[test]
    fn leaves_ordinary_arguments_untouched() {
        // No false positives on everyday tool args — the words/paths survive
        // verbatim, no stray <REDACTED>.
        for ok in [
            "read_file(path=/Users/x/task-management-system/src/main.rs)",
            "bash(command=git commit -m \"refactor token bucket limiter\")",
            "grep(pattern=fn main, path=./src)",
            "edit_file(path=a.rs, old=let x = 5, new=let x = 6)",
        ] {
            assert_eq!(redact_secrets(ok), ok, "should not alter: {ok}");
        }
    }

    #[test]
    fn backtrace_strips_abs_paths_keeps_basename() {
        let bt = "\
   0: my_crate::fn_a
      at /Users/lichao/p/src/a.rs:10
   1: my_crate::fn_b
      at /Users/lichao/p/src/b.rs:20
   2: my_crate::fn_c
      at /Users/lichao/p/src/c.rs:30
   3: frame4
   4: frame5
   5: frame6_should_be_dropped";
        let frames = backtrace_top_k(bt, 5, Some(Path::new("/Users/lichao")), None);
        assert_eq!(frames.len(), 5);
        assert!(frames[1].ends_with("a.rs:10"), "got {:?}", frames[1]);
        assert!(!frames[1].contains("/Users/lichao"));
    }
}
