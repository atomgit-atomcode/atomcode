//! JSONC comment tolerance for the hand-edited JSON config files (`.mcp.json`,
//! `.hooks.json`). Dependency-free, so it is available regardless of capability
//! features.

/// Blank out `//` and `/* … */` comments so a JSONC-flavoured config parses.
///
/// Editors (VS Code, Cursor) treat `.mcp.json` and `.hooks.json` as JSONC, and our own
/// `.mcp.json.example` is commented, so people paste commented configs. Both files go
/// through this one function so they agree on what a comment is.
///
/// Comment bytes are replaced with spaces rather than removed, and newlines inside
/// block comments are kept, so every surviving byte stays at its original offset — a `serde_json` parse error still reports the
/// line/column the user sees in their editor.
///
/// String literals are never touched: `"https://mcp.example.com/mcp"` must survive the
/// `//` in its scheme, and `"a\"//b"` must not end the string at the escaped quote.
///
/// Trailing commas remain invalid — this is comment tolerance, not full JSON5.
pub fn strip_comments(input: &str) -> String {
    normalize(input, false)
}

/// Blank comments and trailing commas in files that opt in to JSONC.
#[cfg(feature = "migration")]
pub(crate) fn strip_comments_and_trailing_commas(input: &str) -> String {
    normalize(input, true)
}

fn normalize(input: &str, allow_trailing_commas: bool) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut in_string = false;
    let mut previous = None;
    let mut trailing_comma = None;

    while i < bytes.len() {
        let b = bytes[i];

        if in_string {
            out.push(b);
            // A backslash escapes the next byte, including `\"` — consume both so an
            // escaped quote does not look like the end of the string.
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => {
                previous = Some(b);
                trailing_comma = None;
                in_string = true;
                out.push(b);
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    out.push(b' ');
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.extend_from_slice(b"  ");
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        out.extend_from_slice(b"  ");
                        i += 2;
                        break;
                    }
                    // Keep newlines so line numbers in parse errors stay truthful.
                    out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                    i += 1;
                }
            }
            _ => {
                if allow_trailing_commas && !b.is_ascii_whitespace() {
                    if matches!(b, b']' | b'}') {
                        if let Some(comma) = trailing_comma {
                            out[comma] = b' ';
                        }
                    }
                    // Let the parser reject commas without a preceding value.
                    trailing_comma = (b == b','
                        && previous.is_some_and(|b| !matches!(b, b'[' | b'{' | b':' | b',')))
                    .then_some(i);
                    previous = Some(b);
                }
                out.push(b);
                i += 1;
            }
        }
    }

    // Every emitted byte is either copied verbatim or an ASCII space/newline, and a
    // multi-byte UTF-8 sequence inside a comment has ALL of its bytes replaced, so the
    // result is still valid UTF-8. The fallback keeps this infallible rather than
    // panicking: the caller's parse then fails on the original text.
    String::from_utf8(out).unwrap_or_else(|_| input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[cfg(feature = "migration")]
    #[test]
    fn trailing_commas_are_blanked_outside_strings_only() {
        let input = r#"{"array": ["quote\",]", 2, /* comment */], "text": ",}",}"#;
        let normalized = strip_comments_and_trailing_commas(input);
        assert_eq!(normalized.len(), input.len());
        let value: Value = serde_json::from_str(&normalized).unwrap();
        assert_eq!(value["array"][0], "quote\",]");
        assert_eq!(value["array"][1], 2);
        assert_eq!(value["text"], ",}");
        assert!(serde_json::from_str::<Value>(&strip_comments(input)).is_err());
    }

    #[cfg(feature = "migration")]
    #[test]
    fn commas_that_do_not_follow_a_value_still_fail() {
        for input in [r#"{,}"#, r#"[ , ]"#, r#"[1,,]"#, r#"{"value": ,}"#] {
            assert!(
                serde_json::from_str::<Value>(&strip_comments_and_trailing_commas(input)).is_err(),
                "{input}"
            );
        }
    }

    /// The trap: a URL's `//` sits inside a string and must survive untouched.
    #[test]
    fn double_slash_inside_a_string_is_not_a_comment() {
        let src = r#"{"url": "https://mcp.example.com/mcp"}"#;
        assert_eq!(
            strip_comments(src),
            src,
            "stripping must not touch string contents"
        );
        let parsed: Value = serde_json::from_str(&strip_comments(src)).unwrap();
        assert_eq!(parsed["url"], "https://mcp.example.com/mcp");
    }

    #[test]
    fn escaped_quote_does_not_end_the_string_early() {
        // The `//` here is still inside the string: the `\"` must not close it.
        let src = r#"{"a": "x\"//y", "b": 1}"#;
        assert_eq!(strip_comments(src), src);
        let parsed: Value = serde_json::from_str(&strip_comments(src)).unwrap();
        assert_eq!(parsed["a"], "x\"//y");
        assert_eq!(parsed["b"], 1);
    }

    #[test]
    fn line_and_block_comments_are_blanked_not_removed() {
        let src = "{\n  // 说明\n  \"a\": 1, /* 尾注 */\n  \"b\": 2\n}";
        let out = strip_comments(src);
        assert_eq!(
            out.len(),
            src.len(),
            "byte offsets must be preserved so parse errors keep pointing at the right column"
        );
        assert_eq!(
            out.lines().count(),
            src.lines().count(),
            "line count must be preserved"
        );
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 2);
    }

    #[test]
    fn multi_line_block_comment_keeps_line_numbers() {
        let src = "{\n/*\n\n*/\n  \"a\": 1\n}";
        let out = strip_comments(src);
        assert_eq!(out.matches('\n').count(), src.matches('\n').count());
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["a"], 1);
    }
}
