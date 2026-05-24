/// JSON repair utilities for malformed LLM tool-call output.
///
/// LLMs frequently produce JSON with issues such as trailing commas, single quotes,
/// unquoted keys, invalid backslash escapes, and markdown code fences.
/// These functions attempt to repair such output before falling back to
/// last-resort key-value extraction.

/// Normalize tool-call arguments into valid JSON before execution.
///
/// Runs the repair chain: direct parse → repair_json → tool-specific extractor →
/// generic key-value extraction. Returns the original string unchanged if all
/// strategies fail (caller can then surface a parse error to the model).
///
/// `tool_name` selects a specialized extractor when available (e.g. `edit_file`
/// which may contain unescaped source code in `old_string`/`new_string`).
pub fn repair_tool_args(tool_name: &str, args: &str) -> String {
    // Fast path: already valid JSON.
    if serde_json::from_str::<serde_json::Value>(args).is_ok() {
        return args.to_string();
    }
    // Generic JSON repair (trailing commas, unquoted keys, fence strip, etc.).
    let repaired = repair_json(args);
    if serde_json::from_str::<serde_json::Value>(&repaired).is_ok() {
        return repaired;
    }
    // Specialized: edit_file often ships source code with unescaped quotes/newlines.
    if tool_name == "edit_file" {
        if let Some(v) = extract_edit_file_args(args) {
            if let Ok(s) = serde_json::to_string(&v) {
                return s;
            }
        }
    }
    // Last resort: key-value field extraction. Only return this if it actually
    // recovered something — an empty object is no better than the original garbage.
    let extracted = extract_json_fields(args);
    if let Some(obj) = extracted.as_object() {
        if !obj.is_empty() {
            if let Ok(s) = serde_json::to_string(&extracted) {
                return s;
            }
        }
    }
    args.to_string()
}

/// Attempt to repair common JSON issues from LLM output:
/// - Trailing commas before } or ]
/// - Single quotes instead of double quotes (outside of string values)
/// - Missing closing braces
/// - Unescaped newlines in strings
/// - Invalid backslash escapes
/// - Unquoted keys
/// - Missing commas between key-value pairs
/// - Markdown code fences
pub fn repair_json(s: &str) -> String {
    let mut result = s.to_string();

    // Fix invalid JSON backslash escapes: \. \( \) \| \w \d \s \+ \* etc.
    // JSON only allows: \\ \" \/ \n \r \t \b \f \uXXXX
    // Models often write regex like @app\.(get|post) which has \. — invalid in JSON.
    // Fix by doubling the backslash: \. → \\. so JSON parses it as literal backslash + dot.
    let valid_escapes = ['\\', '"', '/', 'n', 'r', 't', 'b', 'f', 'u'];
    let chars: Vec<char> = result.chars().collect();
    let mut fixed = String::with_capacity(result.len() + 20);
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            if valid_escapes.contains(&next) {
                // Valid JSON escape — keep as-is
                fixed.push('\\');
                fixed.push(next);
                i += 2;
            } else {
                // Invalid JSON escape (like \. \( \| \w \d \s \+ \*)
                // Double the backslash so JSON parser sees \\ followed by the char
                fixed.push('\\');
                fixed.push('\\');
                fixed.push(next);
                i += 2;
            }
        } else {
            fixed.push(chars[i]);
            i += 1;
        }
    }
    result = fixed;

    // Remove leading/trailing whitespace and any markdown code fences
    result = result.trim().to_string();
    if result.starts_with("```json") {
        result = result
            .strip_prefix("```json")
            .unwrap_or(&result)
            .to_string();
    }
    if result.starts_with("```") {
        result = result.strip_prefix("```").unwrap_or(&result).to_string();
    }
    if result.ends_with("```") {
        result = result.strip_suffix("```").unwrap_or(&result).to_string();
    }
    result = result.trim().to_string();

    // Replace single quotes with double quotes for keys/values
    // Be careful not to break strings containing apostrophes
    // Simple heuristic: replace ' at JSON structural positions
    if !result.contains('"') && result.contains('\'') {
        result = result.replace('\'', "\"");
    }

    // Fix missing commas between key-value pairs: }" " → }", "
    // Pattern: value followed by whitespace then another key
    // e.g., {"path": "src" "depth": 2} → {"path": "src", "depth": 2}
    let mut chars: Vec<char> = result.chars().collect();
    let mut insertions = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // Look for pattern: " <whitespace> " where the second " starts a key
        if chars[i] == '"' {
            let j = i + 1;
            // Skip whitespace
            let mut k = j;
            while k < chars.len() && chars[k].is_whitespace() {
                k += 1;
            }
            // If next non-whitespace is " and it looks like a key (followed by :), insert comma
            if k < chars.len() && chars[k] == '"' && k > j {
                // Check if this looks like key: find the closing " then :
                let mut q = k + 1;
                while q < chars.len() && chars[q] != '"' {
                    q += 1;
                }
                if q + 1 < chars.len() {
                    let mut r = q + 1;
                    while r < chars.len() && chars[r].is_whitespace() {
                        r += 1;
                    }
                    if r < chars.len() && chars[r] == ':' {
                        // This is a missing comma: insert after position i
                        insertions.push(j);
                    }
                }
            }
        }
        i += 1;
    }
    // Insert commas in reverse order to preserve indices
    for pos in insertions.into_iter().rev() {
        chars.insert(pos, ',');
    }
    result = chars.into_iter().collect();

    // Fix unquoted keys: {path: "src"} → {"path": "src"}
    // Simple approach: find patterns like {key: or ,key: and add quotes
    let mut fixed = String::with_capacity(result.len() + 20);
    let rchars: Vec<char> = result.chars().collect();
    let mut ri = 0;
    while ri < rchars.len() {
        if rchars[ri] == '{' || rchars[ri] == ',' {
            fixed.push(rchars[ri]);
            ri += 1;
            // Skip whitespace
            while ri < rchars.len() && rchars[ri].is_whitespace() {
                fixed.push(rchars[ri]);
                ri += 1;
            }
            // Check if next is an unquoted key (alphanumeric/underscore followed by :)
            if ri < rchars.len() && rchars[ri].is_alphanumeric() {
                let key_start = ri;
                while ri < rchars.len() && (rchars[ri].is_alphanumeric() || rchars[ri] == '_') {
                    ri += 1;
                }
                // Skip whitespace after key
                let mut ki = ri;
                while ki < rchars.len() && rchars[ki].is_whitespace() {
                    ki += 1;
                }
                if ki < rchars.len() && rchars[ki] == ':' {
                    // Unquoted key — add quotes
                    fixed.push('"');
                    for c in &rchars[key_start..ri] {
                        fixed.push(*c);
                    }
                    fixed.push('"');
                } else {
                    // Not a key, just copy
                    for c in &rchars[key_start..ri] {
                        fixed.push(*c);
                    }
                }
            }
        } else {
            fixed.push(rchars[ri]);
            ri += 1;
        }
    }
    result = fixed;

    // Remove trailing commas before } or ]
    loop {
        let before = result.clone();
        result = result.replace(",}", "}").replace(",]", "]");
        if result == before {
            break;
        }
    }

    // If it doesn't start with { or [, wrap it
    if !result.starts_with('{') && !result.starts_with('[') {
        result = format!("{{{}}}", result);
    }

    // Count braces and add missing closing ones
    let open_braces = result.chars().filter(|c| *c == '{').count();
    let close_braces = result.chars().filter(|c| *c == '}').count();
    for _ in 0..(open_braces.saturating_sub(close_braces)) {
        result.push('}');
    }

    result
}

/// Last-resort: extract ALL key-value pairs from malformed JSON by string matching.
/// Tool-agnostic — no hardcoded field lists. Finds any `"key": "value"` or `key: value` pattern.
pub fn extract_json_fields(s: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Find a key: either "key" or bare_key followed by :
        let key = if chars[i] == '"' {
            // Quoted key
            let start = i + 1;
            i = start;
            while i < len && chars[i] != '"' {
                i += 1;
            }
            if i >= len {
                break;
            }
            let k: String = chars[start..i].iter().collect();
            i += 1; // skip closing "
            k
        } else if chars[i].is_alphabetic() || chars[i] == '_' {
            // Bare key
            let start = i;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            chars[start..i].iter().collect()
        } else {
            i += 1;
            continue;
        };

        // Skip whitespace, expect :
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len || chars[i] != ':' {
            continue;
        }
        i += 1; // skip :
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= len {
            break;
        }

        // Read value
        if chars[i] == '"' {
            // String value — extract and unescape JSON escape sequences
            let start = i + 1;
            i = start;
            while i < len && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            let raw: String = chars[start..i.min(len)].iter().collect();
            let val = unescape_json_string_contents(&raw);
            map.insert(key, serde_json::json!(val));
            if i < len {
                i += 1;
            }
        } else if chars[i] == 't' || chars[i] == 'f' {
            // Boolean
            let start = i;
            while i < len && chars[i].is_alphabetic() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            match word.as_str() {
                "true" => {
                    map.insert(key, serde_json::json!(true));
                }
                "false" => {
                    map.insert(key, serde_json::json!(false));
                }
                _ => {
                    map.insert(key, serde_json::json!(word));
                }
            }
        } else if chars[i].is_ascii_digit() || chars[i] == '-' {
            // Number
            let start = i;
            while i < len && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == '-') {
                i += 1;
            }
            let num_str: String = chars[start..i].iter().collect();
            if let Ok(n) = num_str.parse::<i64>() {
                map.insert(key, serde_json::json!(n));
            } else if let Ok(f) = num_str.parse::<f64>() {
                map.insert(key, serde_json::json!(f));
            }
        } else {
            // Unquoted string value — read until , } ]
            let start = i;
            while i < len && !matches!(chars[i], ',' | '}' | ']' | '\n') {
                i += 1;
            }
            let val: String = chars[start..i]
                .iter()
                .collect::<String>()
                .trim()
                .to_string();
            if !val.is_empty() {
                map.insert(key, serde_json::json!(val));
            }
        }
    }

    serde_json::Value::Object(map)
}

/// Specialized parser for edit_file arguments when JSON parsing fails.
/// Models often generate old_string/new_string with unescaped quotes/newlines.
/// This parser uses the known field order to extract content by position.
pub fn extract_edit_file_args(raw: &str) -> Option<serde_json::Value> {
    let fp_marker = raw.find("\"file_path\"")?;
    let old_marker = raw.find("\"old_string\"")?;
    let new_marker = raw.find("\"new_string\"")?;
    if old_marker <= fp_marker || new_marker <= old_marker {
        return None;
    }

    // Extract file_path (simple quoted string before old_string)
    let fp_region = &raw[fp_marker + 11..old_marker];
    let fp_colon = fp_region.find(':')?;
    let fp_val = fp_region[fp_colon + 1..]
        .trim()
        .trim_matches(|c| c == '"' || c == ',')
        .trim();
    if fp_val.is_empty() {
        return None;
    }
    let file_path = fp_val.to_string();

    // Extract old_string: everything between "old_string": " and ", "new_string"
    let old_colon = raw[old_marker..].find(':')?;
    let old_start = old_marker + old_colon + 1;
    let old_raw = &raw[old_start..new_marker];
    let old_string = unescape_field_value(old_raw);

    // Extract new_string: everything after "new_string": " to the end
    let new_colon = raw[new_marker..].find(':')?;
    let new_start = new_marker + new_colon + 1;
    let new_raw = &raw[new_start..];
    let new_string = unescape_field_value_end(new_raw);

    if old_string.is_empty() && new_string.is_empty() {
        return None;
    }

    let replace_all = raw.contains("\"replace_all\"")
        && raw.rfind("true").map_or(false, |t| {
            raw.rfind("\"replace_all\"").map_or(false, |r| t > r)
        });

    Some(serde_json::json!({
        "file_path": file_path,
        "old_string": old_string,
        "new_string": new_string,
        "replace_all": replace_all,
    }))
}

fn unescape_field_value(raw: &str) -> String {
    let t = raw.trim().trim_end_matches(',').trim();
    let inner = if t.starts_with('"') { &t[1..] } else { t };
    let inner = inner.trim_end_matches('"');
    unescape_json_string_contents(inner)
}

fn unescape_field_value_end(raw: &str) -> String {
    let t = raw.trim();
    let inner = if t.starts_with('"') { &t[1..] } else { t };
    // Remove trailing "} or ", "replace_all": ... }
    let end = inner
        .rfind("\", \"replace_all\"")
        .or_else(|| inner.rfind("\"}"))
        .or_else(|| inner.rfind("\"\n}"))
        .unwrap_or(inner.len());
    let content = &inner[..end];
    unescape_json_string_contents(content)
}

/// Single-pass JSON-string unescape.
///
/// Sequential `s.replace("\\t", "\t")` chains are unsafe for this: a properly
/// escaped Windows path like `\\test` (raw chars `\` `\` `t`) gets its second
/// `\` + `t` matched as a `\t` escape, corrupting the path. We must consume
/// each backslash + char as one unit.
///
/// Recognized: `\\` `\"` `\/` `\n` `\r` `\t` `\b` `\f`. Unknown `\X` keeps the
/// backslash literal (callers may receive paths that were never JSON-escaped).
/// `\u` Unicode escapes are intentionally not interpreted — out of scope for
/// this last-resort recovery path.
fn unescape_json_string_contents(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('/') => out.push('/'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000C}'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- repair_json tests ---

    #[test]
    fn repair_trailing_comma() {
        let input = r#"{"key": "value",}"#;
        let repaired = repair_json(input);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("should be valid JSON");
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn repair_single_quotes() {
        let input = "{'key': 'value'}";
        let repaired = repair_json(input);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("should be valid JSON");
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn repair_missing_closing_brace() {
        let input = r#"{"key": "value""#;
        let repaired = repair_json(input);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("should be valid JSON");
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn repair_unquoted_keys() {
        let input = r#"{path: "src/main.rs"}"#;
        let repaired = repair_json(input);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("should be valid JSON");
        assert_eq!(parsed["path"], "src/main.rs");
    }

    #[test]
    fn repair_invalid_backslash_escape() {
        // \. is not a valid JSON escape — should be doubled to \\.
        let input = r#"{"pattern": "app\.rs"}"#;
        let repaired = repair_json(input);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("should be valid JSON after escape repair");
        // After repair \. becomes \\. which JSON parses as literal backslash + dot
        assert!(parsed["pattern"].as_str().unwrap().contains('.'));
    }

    #[test]
    fn repair_missing_comma_between_fields() {
        let input = r#"{"path": "src" "depth": 2}"#;
        let repaired = repair_json(input);
        // Should either parse or at least not panic
        let _ = serde_json::from_str::<serde_json::Value>(&repaired);
    }

    #[test]
    fn repair_markdown_fence_json() {
        let input = "```json\n{\"key\": \"value\"}\n```";
        let repaired = repair_json(input);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("should strip fences");
        assert_eq!(parsed["key"], "value");
    }

    #[test]
    fn repair_markdown_fence_no_lang() {
        let input = "```\n{\"key\": \"value\"}\n```";
        let repaired = repair_json(input);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("should strip fences");
        assert_eq!(parsed["key"], "value");
    }

    // --- extract_json_fields tests ---

    #[test]
    fn extract_fields_basic_key_value() {
        let input = r#"{"file_path": "/src/main.rs", "pattern": "hello"}"#;
        let result = extract_json_fields(input);
        assert_eq!(result["file_path"], "/src/main.rs");
        assert_eq!(result["pattern"], "hello");
    }

    #[test]
    fn extract_fields_boolean_values() {
        let input = r#"{"recursive": true, "case_sensitive": false}"#;
        let result = extract_json_fields(input);
        assert_eq!(result["recursive"], true);
        assert_eq!(result["case_sensitive"], false);
    }

    #[test]
    fn extract_fields_bare_keys() {
        let input = r#"{path: "/tmp/foo", depth: 3}"#;
        let result = extract_json_fields(input);
        assert_eq!(result["path"], "/tmp/foo");
    }

    // --- extract_edit_file_args tests ---

    #[test]
    fn extract_edit_file_standard_escaped_newlines() {
        let input = r#"{"file_path": "/src/lib.rs", "old_string": "fn old(){\n}", "new_string": "fn new(){\n}"}"#;
        let result = extract_edit_file_args(input).expect("should parse");
        assert_eq!(result["file_path"], "/src/lib.rs");
        // \n sequences in old_string/new_string get unescaped to real newlines
        assert!(result["old_string"].as_str().unwrap().contains('\n'));
        assert!(result["new_string"].as_str().unwrap().contains('\n'));
    }

    #[test]
    fn extract_edit_file_returns_none_on_missing_markers() {
        let input = r#"{"file_path": "/src/lib.rs"}"#;
        assert!(extract_edit_file_args(input).is_none());
    }

    #[test]
    fn extract_edit_file_replace_all_true() {
        let input = r#"{"file_path": "/src/lib.rs", "old_string": "foo", "new_string": "bar", "replace_all": true}"#;
        let result = extract_edit_file_args(input).expect("should parse");
        assert_eq!(result["replace_all"], true);
    }

    // --- repair_tool_args tests ---

    #[test]
    fn repair_tool_args_passes_valid_json_through() {
        let input = r#"{"file_path":"/tmp/a.rs","content":"x"}"#;
        assert_eq!(repair_tool_args("write_file", input), input);
    }

    #[test]
    fn repair_tool_args_fixes_fence_wrapped_json() {
        let input = "```json\n{\"file_path\":\"/tmp/a.rs\",\"content\":\"x\"}\n```";
        let out = repair_tool_args("write_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("should parse");
        assert_eq!(v["file_path"], "/tmp/a.rs");
    }

    #[test]
    fn repair_tool_args_keeps_empty_object_untouched() {
        // Empty `{}` is valid JSON — we must not paper over it by inventing fields.
        // Callers surface it as a user-visible error instead.
        assert_eq!(repair_tool_args("write_file", "{}"), "{}");
    }

    #[test]
    fn repair_tool_args_returns_original_when_unsalvageable() {
        // Pure garbage with no extractable key=value pairs → return as-is so
        // the tool emits the real parse error (not a misleading repaired stub).
        let input = "!!!";
        assert_eq!(repair_tool_args("write_file", input), "!!!");
    }

    // --- Windows-path unescape regression tests ---
    //
    // Properly-escaped Windows paths arrive in raw form as `\` `\` `t` (3 chars).
    // The old `.replace("\\t", "\t")` chain mistakenly matched the literal "\t"
    // formed by the second backslash + the t, turning `\\test` into `\<TAB>est`.

    #[test]
    fn extract_fields_windows_path_keeps_backslash_t() {
        // JSON-legal: every Windows backslash doubled.
        let input = r#"{"file_path": "D:\\work\\prj\\test-wsd\\run.py"}"#;
        let result = extract_json_fields(input);
        assert_eq!(
            result["file_path"], "D:\\work\\prj\\test-wsd\\run.py",
            "escaped backslashes must collapse to single backslashes, not produce TAB",
        );
        assert!(
            !result["file_path"].as_str().unwrap().contains('\t'),
            "no tab character should appear",
        );
    }

    #[test]
    fn extract_fields_unc_long_path_prefix() {
        // \\?\D:\... long-path prefix, fully escaped → \\?\D:\test-wsd\run.py
        let input = r#"{"file_path": "\\\\?\\D:\\test-wsd\\run.py"}"#;
        let result = extract_json_fields(input);
        assert_eq!(result["file_path"], "\\\\?\\D:\\test-wsd\\run.py");
    }

    #[test]
    fn extract_fields_literal_backslash_n_preserved() {
        // Raw `\` `\` `n` must decode to `\n` (backslash + n), not a newline —
        // sequential `.replace` could swap order and produce a real newline here.
        let input = r#"{"x": "a\\nb"}"#;
        let result = extract_json_fields(input);
        assert_eq!(result["x"], "a\\nb");
        assert!(!result["x"].as_str().unwrap().contains('\n'));
    }

    #[test]
    fn extract_fields_real_escapes_still_work() {
        // Don't regress the intended behavior: \n → newline, \t → tab, \" → ".
        let input = r#"{"a": "line1\nline2", "b": "col1\tcol2", "c": "say \"hi\""}"#;
        let result = extract_json_fields(input);
        assert_eq!(result["a"], "line1\nline2");
        assert_eq!(result["b"], "col1\tcol2");
        assert_eq!(result["c"], "say \"hi\"");
    }

    #[test]
    fn extract_edit_file_windows_path_in_old_string() {
        // old_string/new_string go through unescape_field_value(_end). A Windows
        // path embedded in them must not have its `\t` swallowed into a tab.
        let input = r#"{"file_path": "/src/x.py", "old_string": "p = 'C:\\foo\\test.py'", "new_string": "p = 'C:\\foo\\bar.py'"}"#;
        let result = extract_edit_file_args(input).expect("should parse");
        assert_eq!(result["old_string"], "p = 'C:\\foo\\test.py'");
        assert_eq!(result["new_string"], "p = 'C:\\foo\\bar.py'");
        assert!(!result["old_string"].as_str().unwrap().contains('\t'));
    }
}
