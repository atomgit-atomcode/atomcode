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
    // Defense-in-depth bound. Repair is best-effort salvage of weak-model
    // output: a multi-hundred-KB argument is either already-valid (the tool
    // parses it directly) or hopeless — don't run the structural passes over a
    // giant blob. The passes below are O(N), but this caps total work and
    // allocation on pathological input and is a hard ceiling for the middleware
    // (which runs synchronously on the host thread under panic=abort).
    const MAX_REPAIR_BYTES: usize = 512 * 1024;
    if args.len() > MAX_REPAIR_BYTES {
        return args.to_string();
    }

    // Pre-pass: rescue ambiguous Windows paths BEFORE any JSON parsing
    // touches them. `{"file_path": "D:\test\foo.py"}` is *valid* JSON
    // (`\t` and `\f` are spec-legal escapes), so the fast path would
    // hand it straight to `serde_json::from_str`, which would decode
    // `D:<TAB>est<FF>oo.py` and write to the wrong file. The pre-pass
    // detects drive-letter strings and double-escapes their ambiguous
    // `\X` sequences so the resulting JSON encodes the path the model
    // actually meant. Idempotent on already-correctly-escaped input.
    let pre = pre_escape_windows_paths_in_json(args);

    // Fast path: already valid JSON.
    if serde_json::from_str::<serde_json::Value>(&pre).is_ok() {
        return pre;
    }
    // Generic JSON repair (trailing commas, unquoted keys, fence strip, etc.).
    let repaired = repair_json(&pre);
    if serde_json::from_str::<serde_json::Value>(&repaired).is_ok() {
        return repaired;
    }
    // Specialized: edit_file often ships source code with unescaped quotes/newlines.
    // Case-insensitive so a model that emits `Edit_File`/`EDIT_FILE` still gets the
    // extractor (the kernel resolves tools strictly today, but the middleware now
    // passes the resolved tool's canonical name — see `RepairToolArgsMiddleware`).
    if tool_name.eq_ignore_ascii_case("edit_file") {
        if let Some(v) = extract_edit_file_args(&pre) {
            if let Ok(s) = serde_json::to_string(&v) {
                return s;
            }
        }
    }
    // Last resort: key-value field extraction. Only return this if it actually
    // recovered something — an empty object is no better than the original garbage.
    let extracted = extract_json_fields(&pre);
    if let Some(obj) = extracted.as_object() {
        if !obj.is_empty() {
            if let Ok(s) = serde_json::to_string(&extracted) {
                return s;
            }
        }
    }
    args.to_string()
}

/// Pre-escape ambiguous backslash sequences inside JSON string literals
/// that look like Windows paths.
///
/// Why: `{"file_path": "D:\test\foo.py"}` parses as valid JSON, but
/// `serde_json` decodes `\t`→TAB and `\f`→FF, corrupting the path. The
/// model almost certainly meant literal backslashes. KEY-SCOPED: this only
/// applies to the VALUE of a path-typed key (`file_path`/`path`) that contains
/// a drive-letter prefix (`[A-Za-z]:[\\/]`), where it treats the bare `\X`
/// (X ∈ {t,n,r,b,f,u}) as a literal backslash and doubles it. A `\n`/`\t` in a
/// `content`/`old_string` value is left as the model's intended JSON escape —
/// rewriting there silently corrupted valid code/text.
///
/// Idempotent: already-correctly-escaped `\\` is preserved (the second
/// backslash is consumed as part of the escape pair, not a fresh one).
/// JSON-legal `\"` and `\/` are also passed through verbatim.
///
/// Heuristic precision: the drive-letter detector requires the alpha
/// char to be a *single* letter (not the tail of a longer word), so
/// strings like `"category:\nimportant"` don't trip it — the byte
/// preceding the alpha must not itself be alphabetic.
fn pre_escape_windows_paths_in_json(s: &str) -> String {
    // KEY-SCOPED: only the VALUE of a path-typed key is eligible for the rewrite.
    // A `\n`/`\t` inside `content`/`old_string`/`new_string` (or any non-path
    // value) is almost always an intended JSON escape, not a path separator;
    // rewriting there silently corrupted valid model output (a code blob with a
    // drive-label + newline like `print('C:\ndone')` became literal `\n`).
    // `file_path`/`path` are the file-targeting keys where a lone-backslash drive
    // path is genuinely ambiguous and worth disambiguating.
    const PATH_KEYS: &[&str] = &["file_path", "path"];

    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(n + 16);
    let mut i = 0;
    // The most recently seen object key. A string is a KEY when the next
    // non-whitespace char after it is `:`; otherwise it is a VALUE whose key is
    // `current_key` (carries across array elements, reset by the next real key).
    let mut current_key: Option<String> = None;
    while i < n {
        if chars[i] != '"' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // Opening quote — find the matching close, honoring JSON
        // backslash escapes so `\"` doesn't terminate.
        let body_start = i + 1;
        let mut j = body_start;
        while j < n {
            if chars[j] == '\\' && j + 1 < n {
                j += 2;
                continue;
            }
            if chars[j] == '"' {
                break;
            }
            j += 1;
        }
        let body_end = j.min(n);
        let body: String = chars[body_start..body_end].iter().collect();
        let after = if body_end < n { body_end + 1 } else { body_end };

        // Key vs value: peek past the close quote and any whitespace for `:`.
        let mut k = after;
        while k < n && chars[k].is_whitespace() {
            k += 1;
        }
        let is_key = k < n && chars[k] == ':';

        out.push('"');
        if is_key {
            out.push_str(&body);
            current_key = Some(body);
        } else {
            let is_path_value = current_key
                .as_deref()
                .is_some_and(|kk| PATH_KEYS.iter().any(|p| kk.eq_ignore_ascii_case(p)));
            if is_path_value && looks_like_windows_path(&body) {
                rewrite_windows_path_body(&body, &mut out);
            } else {
                out.push_str(&body);
            }
        }
        if body_end < n {
            out.push('"');
            i = body_end + 1;
        } else {
            i = body_end;
        }
    }
    out
}

/// True iff `s` contains an **under-escaped** Windows drive-letter path
/// prefix (`[A-Za-z]:\` with a *single* backslash) in a path-shaped context.
///
/// Required context: the drive letter is at the start of the body,
/// or the byte before it is `\` (UNC long-path `\\?\D:\…`), `'`,
/// or `"` (quoted path literal embedded in code). Without this
/// guard, natural-language strings whose contents happen to match
/// the alpha-colon-backslash shape — e.g. `class A:\n`, `case X:\n`,
/// `Section B:\nContent` — would be misread as Windows paths and
/// every `\n`/`\t` in the body would be doubled to a literal
/// backslash+letter, corrupting the file. The earlier
/// "preceded-by-alphabetic" guard only ruled out multi-letter
/// words like `category:\n`; single-letter labels slipped through
/// and broke `write_file` on common Python sources.
///
/// **Single-backslash requirement (the 审核 / Windows-desktop bug).**
/// Only a *lone* `\` after the colon can mis-decode under `serde_json`
/// (`D:\test` → `D:<TAB>est`). Two cases must NOT trigger the body
/// rewrite, because rewriting then doubles every real `\n`/`\t`
/// elsewhere in the same string:
/// * `X:/…` forward-slash paths — never escape-ambiguous.
/// * `X:\\…` already-escaped paths — valid JSON that decodes
///   correctly. A multi-line `content`/`old_string` blob frequently
///   *contains* such a path (`excel_path = r'C:\\Users\\…\\文章.xlsx'`)
///   right next to real `\n` newlines; firing here doubled all of
///   them and landed a 30-line script on disk as ONE line of literal
///   backslash-n → broken Python → the agent looped forever trying
///   to "fix the encoding". Gate on the lone backslash so the
///   correctly-escaped path leaves the surrounding newlines intact.
fn looks_like_windows_path(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 3 {
        return false;
    }
    for i in 0..bytes.len().saturating_sub(2) {
        if !bytes[i].is_ascii_alphabetic() {
            continue;
        }
        if bytes[i + 1] != b':' {
            continue;
        }
        // Only a single backslash is ambiguous. `/` and `\\` decode
        // correctly already — see the doc note above.
        if bytes[i + 2] != b'\\' {
            continue;
        }
        if bytes.get(i + 3) == Some(&b'\\') {
            continue;
        }
        // Path-context guard: only accept at start of body, or
        // immediately after a path-shaped delimiter. Everything
        // else (whitespace, alpha, punctuation, JSON escapes) is
        // a false-positive surface for prose content.
        if i > 0 {
            let prev = bytes[i - 1];
            if !matches!(prev, b'\\' | b'"' | b'\'') {
                continue;
            }
        }
        return true;
    }
    false
}

/// Walk an already-extracted JSON string body (between but not
/// including the surrounding quotes) and double any bare `\X` that
/// `looks_like_windows_path` flagged as ambiguous, while leaving
/// already-escaped sequences alone.
fn rewrite_windows_path_body(body: &str, out: &mut String) {
    let chars: Vec<char> = body.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        if chars[k] != '\\' {
            out.push(chars[k]);
            k += 1;
            continue;
        }
        match chars.get(k + 1).copied() {
            Some('\\') => {
                // Already escaped — preserve both bytes.
                out.push_str("\\\\");
                k += 2;
            }
            Some(c @ ('"' | '/' | 'u')) => {
                // JSON-legal escape unrelated to single-char ambiguity
                // — preserve verbatim.
                //
                // `\u` is the JSON Unicode escape `\uXXXX` (always 6
                // chars total, 4 hex digits follow). Unlike `\t`/`\n`/
                // `\r`/`\b`/`\f` — single-letter shortcuts that a
                // Windows path could naturally produce as
                // backslash+letter — `\u` is unambiguous: a Windows
                // path containing literal `\u` is impossible (drive
                // letter + `:` + `\` then directory char; no shell or
                // model would normalise a directory called "u…" to a
                // `\u` glyph). Treating `\u` as ambiguous corrupted
                // legitimate Unicode escapes inside drive-letter
                // strings: `"D:A\foo"` → `"D:\\u0041\\foo"`
                // decoded to literal `D:A\foo` instead of `D:A\foo`.
                out.push('\\');
                out.push(c);
                k += 2;
            }
            Some(c @ ('t' | 'n' | 'r' | 'b' | 'f')) => {
                // Ambiguous in Windows-path context: model meant a
                // literal backslash, not a JSON escape. Double the
                // backslash so the JSON parser decodes `\X` back to
                // the two chars `\` and X.
                out.push_str("\\\\");
                out.push(c);
                k += 2;
            }
            Some(other) => {
                // Invalid JSON escape — leave for repair_json to fix.
                out.push('\\');
                out.push(other);
                k += 2;
            }
            None => {
                out.push('\\');
                k += 1;
            }
        }
    }
}

/// For each position in `chars`, true iff that char is structural
/// JSON (outside any string body). The surrounding `"` chars themselves
/// are considered structural; everything between them — including
/// escape pairs like `\"` and `\n` — is non-structural so structural
/// passes don't mistake string content for grammar.
///
/// Used by the unquoted-key fix, trailing-comma removal, and brace
/// balance to skip work that would otherwise corrupt strings whose
/// contents look like JSON fragments (source code with `{`/`,}`/
/// `class:`/etc.).
fn structural_mask(chars: &[char]) -> Vec<bool> {
    let mut mask = vec![true; chars.len()];
    let mut in_string = false;
    let mut i = 0;
    while i < chars.len() {
        if !in_string {
            if chars[i] == '"' {
                in_string = true;
            }
            i += 1;
            continue;
        }
        if chars[i] == '\\' && i + 1 < chars.len() {
            mask[i] = false;
            mask[i + 1] = false;
            i += 2;
            continue;
        }
        if chars[i] == '"' {
            in_string = false;
            i += 1;
            continue;
        }
        mask[i] = false;
        i += 1;
    }
    mask
}

fn escape_unescaped_control_chars_in_strings(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    let chars: Vec<char> = s.chars().collect();
    let mask = structural_mask(&chars);
    let mut escaped = String::with_capacity(s.len());

    for (i, c) in chars.into_iter().enumerate() {
        if mask[i] {
            escaped.push(c);
            continue;
        }

        match c {
            '\u{0008}' => escaped.push_str("\\b"),
            '\t' => escaped.push_str("\\t"),
            '\n' => escaped.push_str("\\n"),
            '\u{000c}' => escaped.push_str("\\f"),
            '\r' => escaped.push_str("\\r"),
            '\u{0000}'..='\u{001f}' => {
                let byte = c as usize;
                escaped.push_str("\\u00");
                escaped.push(HEX[byte >> 4] as char);
                escaped.push(HEX[byte & 0x0f] as char);
            }
            _ => escaped.push(c),
        }
    }

    escaped
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

    // JSON forbids literal control characters inside quoted values. Preserve
    // structural whitespace while escaping only characters inside strings, so
    // nested arrays and objects remain intact for the normal parser.
    result = escape_unescaped_control_chars_in_strings(&result);

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
    let chars: Vec<char> = result.chars().collect();
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
    // Insert the queued commas in a single O(N) pass. Replaying with
    // `Vec::insert` (each O(N), shifting the tail) over O(N) insertions was
    // O(N^2) — a long run of comma-less fields from a weak model would pin a
    // core. `insertions` is ascending (collected in a forward scan), so a single
    // ordered rebuild is byte-identical to the reverse `insert` replay.
    if insertions.is_empty() {
        result = chars.into_iter().collect();
    } else {
        let mut rebuilt = Vec::with_capacity(chars.len() + insertions.len());
        let mut ins = insertions.into_iter().peekable();
        for (idx, c) in chars.into_iter().enumerate() {
            // `chars.insert(pos, ',')` inserts BEFORE chars[pos], so emit a comma
            // before each char whose index is queued (a while-loop tolerates
            // duplicate positions, though the forward scan can't produce them).
            while ins.peek() == Some(&idx) {
                rebuilt.push(',');
                ins.next();
            }
            rebuilt.push(c);
        }
        // Any insertion queued at end-of-string (pos == chars.len()).
        for _ in ins {
            rebuilt.push(',');
        }
        result = rebuilt.into_iter().collect();
    }

    // Fix unquoted keys: {path: "src"} → {"path": "src"}
    // Guarded by `structural_mask` so a `{`/`,` INSIDE a string value
    // doesn't trigger the rewrite — otherwise source code like
    // `"snippet { class: foo }"` would have `"class"` injected into
    // the string body, corrupting both content and JSON validity.
    let mut fixed = String::with_capacity(result.len() + 20);
    let rchars: Vec<char> = result.chars().collect();
    let mask = structural_mask(&rchars);
    let mut ri = 0;
    while ri < rchars.len() {
        if mask[ri] && (rchars[ri] == '{' || rchars[ri] == ',') {
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

    // Remove trailing commas before } or ]. Both the `,` and the
    // closing brace must be structural — a literal `,}` inside a
    // string value (e.g. `"tail,}"`) must survive unchanged.
    //
    // Single right-to-left pass. The previous fixpoint loop removed only ONE
    // comma per closing brace per pass and recomputed `structural_mask` each
    // pass → O(N^2), a host-freeze on a long `,,,,]` run from a weak model.
    // `structural_mask` is invariant under structural-comma removal (it depends
    // only on quote positions), so one pass suffices: a structural comma is
    // dropped iff its nearest kept right-neighbor — reachable through a run of
    // already-dropped structural commas — is a structural `}`/`]`. Whitespace or
    // any other char breaks the run (matching the old loop, which required the
    // IMMEDIATE right neighbor to be the brace). Output is identical.
    {
        let rchars: Vec<char> = result.chars().collect();
        let mask = structural_mask(&rchars);
        let mut keep = vec![true; rchars.len()];
        let mut right_is_close = false;
        for i in (0..rchars.len()).rev() {
            if mask[i] && rchars[i] == ',' {
                if right_is_close {
                    keep[i] = false; // drop; a removed comma is transparent
                } else {
                    right_is_close = false;
                }
            } else if mask[i] && (rchars[i] == '}' || rchars[i] == ']') {
                right_is_close = true;
            } else {
                right_is_close = false;
            }
        }
        result = rchars
            .into_iter()
            .enumerate()
            .filter_map(|(i, c)| if keep[i] { Some(c) } else { None })
            .collect();
    }

    // If it doesn't start with { or [, wrap it
    if !result.starts_with('{') && !result.starts_with('[') {
        result = format!("{{{}}}", result);
    }

    // Count braces and add missing closing ones. Only structural
    // `{`/`}` count — a string value containing source code with
    // `{ … }` is balanced from the JSON envelope's perspective and
    // must not provoke extra `}` appends.
    let rchars: Vec<char> = result.chars().collect();
    let mask = structural_mask(&rchars);
    let mut open_braces = 0usize;
    let mut close_braces = 0usize;
    for (i, &c) in rchars.iter().enumerate() {
        if !mask[i] {
            continue;
        }
        if c == '{' {
            open_braces += 1;
        } else if c == '}' {
            close_braces += 1;
        }
    }
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

    #[test]
    fn repair_json_escapes_unescaped_newline_inside_string() {
        let input = "{\n\"instruction\":\"line 1\nline 2\"\n}";
        let repaired = repair_json(input);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("should escape the literal newline");
        assert_eq!(parsed["instruction"], "line 1\nline 2");
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
    fn repair_tool_args_parallel_edit_preserves_files_with_unescaped_newline() {
        let input = concat!(
            r#"{"files":[{"path":"a.rs","instruction":"line 1"#,
            "\n",
            r#"line 2"},{"path":"b.rs","instruction":"change b"}]}"#
        );
        let repaired = repair_tool_args("parallel_edit_files", input);
        let parsed: serde_json::Value =
            serde_json::from_str(&repaired).expect("should preserve the files array");
        assert_eq!(
            parsed["files"],
            serde_json::json!([
                {"path": "a.rs", "instruction": "line 1\nline 2"},
                {"path": "b.rs", "instruction": "change b"}
            ])
        );
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

    // --- pre_escape_windows_paths_in_json regression tests ---
    //
    // The fast path in `repair_tool_args` would otherwise hand
    // `{"file_path": "D:\test\foo.py"}` (spec-valid JSON) straight to
    // `serde_json::from_str`, which decodes `\t`→TAB and `\f`→FF.
    // c1f33e62 only fixed `extract_json_fields` (last-resort); the main
    // path needed its own guard.

    #[test]
    fn repair_tool_args_rescues_windows_path_in_valid_json() {
        // Model emits valid JSON with a single-backslash Windows path —
        // this would silently decode to "D:<TAB>est<FF>oo.py" without
        // the pre-pass. Raw bytes: `D` `:` `\` `t` `e` `s` `t` `\` `f`...
        let input = "{\"file_path\": \"D:\\test\\foo.py\"}";
        let out = repair_tool_args("read_file", input);
        let v: serde_json::Value =
            serde_json::from_str(&out).expect("should be valid JSON after pre-pass");
        assert_eq!(v["file_path"], "D:\\test\\foo.py");
        let s = v["file_path"].as_str().unwrap();
        assert!(!s.contains('\t'), "tab must not appear: got {:?}", s);
        assert!(
            !s.contains('\u{000C}'),
            "form feed must not appear: got {:?}",
            s
        );
    }

    #[test]
    fn repair_tool_args_idempotent_on_correctly_escaped_path() {
        // Properly escaped Windows path — pre-pass must not double again.
        // Raw bytes: `D` `:` `\` `\` `w` `o` `r` `k` `\` `\` `a` ...
        let input = r#"{"file_path": "D:\\work\\app.py"}"#;
        let out = repair_tool_args("read_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["file_path"], "D:\\work\\app.py");
    }

    #[test]
    fn repair_tool_args_preserves_unc_long_path_prefix() {
        // \\?\D:\... long-path prefix, fully escaped. The leading
        // \\\\?\\ region must survive pre-pass and parse correctly.
        let input = r#"{"file_path": "\\\\?\\D:\\test-wsd\\run.py"}"#;
        let out = repair_tool_args("read_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["file_path"], "\\\\?\\D:\\test-wsd\\run.py");
    }

    #[test]
    fn repair_tool_args_non_path_string_with_tab_preserved() {
        // Drive-letter heuristic must NOT fire on plain text containing
        // a `\t` escape — `category:\n…` looks superficially similar
        // (alpha-then-`:`-then-`\`) but `y` is the tail of a word, not
        // a single-letter drive. The tab MUST be decoded as a tab.
        let input = r#"{"category": "fast\ttab\nnewline"}"#;
        let out = repair_tool_args("write_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let s = v["category"].as_str().unwrap();
        assert!(s.contains('\t'), "real \\t should remain a tab: {:?}", s);
        assert!(
            s.contains('\n'),
            "real \\n should remain a newline: {:?}",
            s
        );
    }

    #[test]
    fn repair_tool_args_word_ending_with_colon_then_backslash_is_not_path() {
        // `category:\nimportant` — drive letter must be SINGLE alpha,
        // not the tail of a longer word. False-positive would corrupt
        // the intended newline into the two chars `\` + `n`.
        let input = r#"{"label": "category:\nimportant"}"#;
        let out = repair_tool_args("write_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let s = v["label"].as_str().unwrap();
        assert!(s.contains('\n'), "newline should survive: got {:?}", s);
        assert!(
            !s.contains('\\'),
            "no literal backslash should remain: got {:?}",
            s
        );
    }

    /// Reverted-fix regression pin. A previous attempt added a
    /// "skip if body contains `\n` or `\r` escape" guard to
    /// `looks_like_windows_path` to defend content-with-embedded-
    /// path bodies. It broke Windows paths whose own filenames
    /// start with `n` or `r` — `D:\new`, `D:\node_modules`,
    /// `D:\readme.txt`, `\nightly\foo`, etc. — because those
    /// contain a `\` + `n` (or `\r`) byte pair that the guard
    /// misread as a newline escape. Eval matrix went 14 → 27
    /// before the revert.
    ///
    /// Pin the loose-path case so any future "body shape" guard
    /// has to keep it working.
    #[test]
    fn repair_tool_args_loose_windows_path_with_n_dir_name_still_rewrites() {
        // Raw JSON: `{"file_path": "D:\new\foo.py"}` — model emits
        // single-backslash Windows path with a directory called
        // `new`. The bytes between the inner quotes are `D` `:`
        // `\` `n` `e` `w` `\` `f` `o` `o` `.` `p` `y`. The pre-
        // escape pass MUST double the `\n` and `\f` so the path
        // round-trips, otherwise serde decodes `\n` → newline and
        // the path turns into `D:<newline>ew<formfeed>oo.py`.
        let input = "{\"file_path\": \"D:\\new\\foo.py\"}";
        let out = repair_tool_args("read_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let p = v["file_path"].as_str().unwrap();
        assert_eq!(
            p, "D:\\new\\foo.py",
            "loose Windows path with `\\n` substring must round-trip; got {:?}",
            p
        );
        assert!(
            !p.contains('\n'),
            "no real newline must leak through: {:?}",
            p
        );
        assert!(
            !p.contains('\u{000C}'),
            "no form feed must leak through: {:?}",
            p
        );
    }

    /// Python source like `class A:\n    pass\n` has a single
    /// uppercase letter preceded by whitespace, then `:`, then
    /// `\` from the JSON `\n` escape. The old tail-of-word guard
    /// only rejected multi-letter words, so single-letter "names"
    /// (class names, match arms, switch labels) slipped through
    /// and every `\n`/`\t` in the file body got doubled, writing
    /// the file as one line of literal `\n` characters. This is
    /// the v4.23.2 tool-error regression — `notify.py` rewrites
    /// turned into 1 line of garbage.
    #[test]
    fn repair_tool_args_single_letter_label_before_newline_is_not_path() {
        let input = r#"{"file_path": "/tmp/notify.py", "content": "class A:\n    pass\n"}"#;
        let out = repair_tool_args("write_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let content = v["content"].as_str().unwrap();
        assert!(
            content.contains('\n'),
            "newline must survive — file becomes 1-line garbage otherwise: got {:?}",
            content
        );
        assert!(
            !content.contains("\\n"),
            "literal backslash-n must not appear: got {:?}",
            content
        );
        assert_eq!(content, "class A:\n    pass\n");
    }

    #[test]
    fn repair_tool_args_content_with_escaped_windows_path_keeps_newlines() {
        // The Windows "审核" screenshot bug: a write_file whose CONTENT is a
        // multi-line Python script that *references* a correctly-escaped
        // Windows path (`C:\\Users\\…`). The path made looks_like_windows_path
        // fire on the WHOLE content body, and rewrite_windows_path_body then
        // doubled every real `\n` newline into a literal backslash-n — landing
        // the 4-line script on disk as ONE line of broken Python (the
        // `(813 bytes, 1 lines)` in the report), after which `python` exits 1
        // and the agent loops forever "fixing the encoding".
        //
        // Now the gate only fires on UNDER-escaped (single-backslash) drive
        // paths, so the already-`\\`-escaped path is left alone and the real
        // newlines survive.
        let input = r#"{"file_path":"D:\\atomcode\\read_excel.py","content":"import openpyxl\nimport os\nexcel_path = r'C:\\Users\\Administrator\\Desktop\\文章.xlsx'\nprint(os.path.exists(excel_path))\n"}"#;
        let out = repair_tool_args("write_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let content = v["content"].as_str().unwrap();
        assert!(
            content.contains('\n'),
            "real newlines must survive — file becomes 1-line garbage otherwise: got {:?}",
            content
        );
        assert!(
            !content.contains("\\n"),
            "no literal backslash-n must appear: got {:?}",
            content
        );
        // The escaped path must still decode to single backslashes.
        assert!(
            content.contains(r"C:\Users\Administrator\Desktop\文章.xlsx"),
            "embedded Windows path must round-trip: got {:?}",
            content
        );
        assert_eq!(
            content.lines().count(),
            4,
            "should be a 4-line script, not collapsed to 1: got {:?}",
            content
        );
    }

    #[test]
    fn repair_tool_args_lowercase_drive_letter_recognized() {
        // Lowercase `c:\` is also a valid Windows drive prefix.
        let input = "{\"file_path\": \"c:\\users\\me\\file.txt\"}";
        let out = repair_tool_args("read_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["file_path"], "c:\\users\\me\\file.txt");
    }

    #[test]
    fn repair_tool_args_windows_path_in_malformed_json_recovered() {
        // Pre-pass + repair_json combined: trailing comma (parses-fail)
        // AND single-backslash Windows path. Pre-pass fixes the path
        // first, then repair_json strips the trailing comma.
        let input = "{\"file_path\": \"D:\\test\\foo.py\",}";
        let out = repair_tool_args("read_file", input);
        let v: serde_json::Value =
            serde_json::from_str(&out).expect("should recover via repair_json");
        assert_eq!(v["file_path"], "D:\\test\\foo.py");
    }

    #[test]
    fn repair_tool_args_windows_path_rescue_scoped_to_path_keys() {
        // KEY-SCOPED (option 1): a lone-backslash drive path is disambiguated in
        // `file_path` (genuinely a path), but a `\n`/`\t` inside `old_string` is
        // taken as the JSON escape the model wrote — NOT doubled into a literal
        // backslash — so valid code/text isn't corrupted. (Models must escape
        // paths they embed in code, e.g. `D:\\test`.)
        let input = "{\"file_path\": \"D:\\test\\foo.py\", \"old_string\": \"x = 'C:\\nfoo'\"}";
        let out = repair_tool_args("edit_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        // file_path: the lone-backslash path rescued to literal backslashes.
        assert_eq!(v["file_path"], "D:\\test\\foo.py");
        // old_string: the `\n` stays the model's intended NEWLINE, not `\\n`.
        assert_eq!(v["old_string"], "x = 'C:\nfoo'");
    }

    #[test]
    fn repair_tool_args_drive_shape_in_non_path_value_not_rewritten() {
        // KEY-SCOPED: a drive-letter shape in a NON-path value (`cmd`) is left
        // exactly as the model wrote it — only file_path/path get the path pass.
        // A properly-escaped path round-trips unchanged; the pre-pass walker still
        // honours `\"` when locating the string close.
        let input = "{\"cmd\": \"run \\\"D:\\\\foo.exe\\\"\"}";
        let out = repair_tool_args("bash", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["cmd"], "run \"D:\\foo.exe\"");
    }

    #[test]
    fn pre_escape_idempotent_under_double_application() {
        // Belt-and-suspenders: pre-pass must be a fixed point so a
        // future refactor that accidentally applies it twice doesn't
        // double-escape paths.
        let once = pre_escape_windows_paths_in_json(r#"{"p": "D:\\a\\b"}"#);
        let twice = pre_escape_windows_paths_in_json(&once);
        assert_eq!(once, twice, "pre_escape should be idempotent");
    }

    /// `\u` Unicode escapes inside a drive-letter string must survive
    /// the Windows pre-pass intact. `\u` is the 6-char `\uXXXX` JSON
    /// escape — not a single-char ambiguity like `\t`/`\n` that could
    /// arise from a literal Windows path. Treating it as ambiguous
    /// would corrupt legitimate Unicode escapes (`张` for "张" in
    /// `C:\Users\张三\…`) by doubling the backslash and turning the
    /// Chinese name into the literal text `张`.
    #[test]
    fn pre_escape_preserves_unicode_escape_in_windows_path() {
        // Raw bytes: `C` `:` `\` `\` `U` `s` `e` `r` `s` `\` `\` `\` `u` `5` `f` `2` `0` …
        // After JSON decode that's `C:\Users\张三\file.txt`.
        let input = r#"{"file_path": "C:\\Users\\张三\\file.txt"}"#;
        let out = repair_tool_args("read_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(v["file_path"], "C:\\Users\\张三\\file.txt");
        // Negative: bytes after pre-pass MUST NOT contain `\\u` —
        // that would mean we doubled the backslash and broke the
        // Unicode escape. Check via the round-trip: if the decoded
        // string contains a literal `\u` substring, the pre-pass
        // corrupted it.
        let s = v["file_path"].as_str().unwrap();
        assert!(
            !s.contains("\\u"),
            "Unicode escape must not survive as literal `\\u`; got {s:?}",
        );
    }

    /// Mixed: `\u` preserved (legit escape) AND `\t`/`\f` doubled
    /// (Windows path chars). The path-context heuristic applies per
    /// `\X` pair independently. JSON body `D:\testA\foo` should
    /// decode to `D:\testA\foo` — the `A` from `A` snaps directly
    /// onto `test` because no backslash separates them in the source.
    #[test]
    fn pre_escape_mixes_unicode_escape_with_ambiguous_letter() {
        let input = "{\"file_path\": \"D:\\test\\u0041\\foo\"}";
        let out = repair_tool_args("read_file", input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        // \t → literal `\t`; A → "A"; \f → literal `\f`.
        assert_eq!(v["file_path"], "D:\\testA\\foo");
    }

    // --- repair_json in_string awareness regression tests ---
    //
    // These call `repair_json` DIRECTLY rather than through
    // `repair_tool_args` because the end-to-end `extract_json_fields`
    // fallback otherwise rescues these scenarios and masks the
    // structural-pass bugs. The contract under test is "repair_json's
    // output should be parseable when the input was already mostly
    // valid, even if string contents look JSON-shaped".

    #[test]
    fn repair_json_brace_balance_ignores_braces_in_strings() {
        // Pre-fix brace balance counted `{` inside the string value,
        // saw 2 `{` vs 1 `}`, and appended a spurious closing brace,
        // producing `{"k":"v{"}}` which fails to parse.
        let input = r#"{"old_string": "fn main() {"}"#;
        let repaired = repair_json(input);
        let v: serde_json::Value = serde_json::from_str(&repaired).unwrap_or_else(|e| {
            panic!("brace balance should not over-close; got {repaired:?}: {e}")
        });
        assert_eq!(v["old_string"], "fn main() {");
    }

    #[test]
    fn repair_json_unquoted_key_does_not_quote_inside_string() {
        // String value contains `{ class: foo }` which looks like an
        // unquoted-key pattern. Pre-fix the walker happily inserted
        // `"class"` INSIDE the string, mutating the model's content
        // and corrupting the JSON to boot.
        let input = r#"{"outer": "snippet { class: foo }", "n": 1}"#;
        let repaired = repair_json(input);
        let v: serde_json::Value = serde_json::from_str(&repaired).unwrap_or_else(|e| {
            panic!("unquoted-key fix must not touch string content; got {repaired:?}: {e}")
        });
        assert_eq!(v["outer"], "snippet { class: foo }");
    }

    #[test]
    fn repair_json_trailing_comma_skips_literal_inside_string() {
        // Pre-fix used `result.replace(",}", "}")` globally, so a
        // string value containing `,}` literal got its content
        // rewritten to `}`.
        let input = r#"{"outer": "tail,}", "n": 1}"#;
        let repaired = repair_json(input);
        let v: serde_json::Value = serde_json::from_str(&repaired).unwrap_or_else(|e| {
            panic!("trailing-comma replace must not touch strings; got {repaired:?}: {e}")
        });
        assert_eq!(v["outer"], "tail,}");
    }

    #[test]
    fn repair_json_handles_multiple_braces_in_source_string() {
        // edit_file old_string with nested `{ }` in source — common
        // for Rust/JS code. With unquoted-key + brace-balance both
        // fixed, the walker leaves the string alone and brace count
        // nets to zero from the envelope's perspective.
        let input = r#"{"old_string": "fn x() { if y { return z; } }", "k": 1}"#;
        let repaired = repair_json(input);
        let v: serde_json::Value = serde_json::from_str(&repaired).unwrap_or_else(|e| {
            panic!("nested braces in string must not break repair; got {repaired:?}: {e}")
        });
        assert_eq!(v["old_string"], "fn x() { if y { return z; } }");
    }

    #[test]
    fn repair_json_unquoted_key_outside_string_still_works() {
        // Make sure the in_string guard doesn't disable the normal
        // unquoted-key fix on real unquoted keys.
        let input = r#"{path: "src/main.rs", depth: 2}"#;
        let repaired = repair_json(input);
        let v: serde_json::Value =
            serde_json::from_str(&repaired).expect("legit unquoted keys must still be wrapped");
        assert_eq!(v["path"], "src/main.rs");
        assert_eq!(v["depth"], 2);
    }

    #[test]
    fn repair_json_trailing_comma_outside_string_still_removed() {
        // Make sure the in_string guard doesn't disable the normal
        // trailing-comma removal on real trailing commas.
        let input = r#"{"k": "v",}"#;
        let repaired = repair_json(input);
        let v: serde_json::Value =
            serde_json::from_str(&repaired).expect("legit trailing comma must still be stripped");
        assert_eq!(v["k"], "v");
    }
}

// ---------------------------------------------------------------------------
// Middleware: normalize tool-call argument JSON before execution.
// ---------------------------------------------------------------------------

use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall};
use std::sync::Arc;

/// Repairs a tool call's JSON arguments in place before the call executes.
///
/// Kernel tools deserialize their arguments directly
/// (`serde_json::from_str(&call.arguments)`), so any non-conforming JSON the
/// model emits — trailing commas, single quotes, unescaped source-code quotes /
/// newlines, markdown code fences, ambiguous Windows backslash paths — fails the
/// *entire* tool call. Weaker models trip this constantly when writing files or
/// editing code, which surfaces to the user as "write failed / nothing happens".
///
/// The former core dispatch ran `repair_tool_args` over arguments before execution;
/// the layered kernel has no built-in equivalent. This middleware preserves that
/// tolerance by rewriting `call.arguments` in [`before`](ToolMiddleware::before).
///
/// **Register it FIRST** (ahead of any approval gate) so the bytes an approval
/// gate sees are exactly the bytes that execute — the repaired, valid JSON. The
/// repair chain leaves already-valid JSON untouched EXCEPT an under-escaped
/// Windows drive path in a `file_path`/`path` VALUE, which it intentionally
/// rewrites (`{"file_path":"D:\test"}` is valid JSON that mis-decodes to
/// `D:<TAB>est`) — `content`/`old_string` values are never rewritten. It returns
/// hopelessly broken input unchanged, so rewriting unconditionally is safe and
/// never blocks: a
/// non-repairable payload still reaches the tool, which surfaces the real parse
/// error to the model.
pub struct RepairToolArgsMiddleware;

impl RepairToolArgsMiddleware {
    /// Normalize the call's arguments to valid JSON in place, selecting the
    /// `edit_file` specialized extractor by `tool_name`. Extracted from `before`
    /// so it can be unit-tested without a `Tool`/`RequestCtx`.
    fn repair_call(&self, tool_name: &str, call: &mut ToolCall) {
        call.arguments = repair_tool_args(tool_name, &call.arguments);
    }
}

#[async_trait]
impl ToolMiddleware for RepairToolArgsMiddleware {
    /// Repair arguments before execution; never blocks (non-repairable input is
    /// passed through untouched, letting the tool report the real parse error).
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        // Use the RESOLVED tool's canonical name, not the raw `call.name`, so the
        // edit_file extractor selection survives any future alias / case-insensitive
        // tool resolution in the kernel — matching v1, which repaired with the
        // corrected name.
        self.repair_call(tool.name(), call);
        BeforeOutcome::Proceed
    }
}

#[cfg(test)]
mod middleware_tests {
    use super::*;

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: args.into(),
        }
    }

    #[test]
    fn repairs_trailing_comma_in_write_file_args() {
        // Common weak-model output: a trailing comma in write_file args, which
        // the kernel's `from_str` rejects outright.
        let mw = RepairToolArgsMiddleware;
        let mut c = call(
            "write_file",
            r#"{"file_path":"game.html","content":"<html>",}"#,
        );
        mw.repair_call("write_file", &mut c);
        let v: serde_json::Value = serde_json::from_str(&c.arguments)
            .expect("arguments should be valid JSON after repair");
        assert_eq!(v["file_path"], "game.html");
    }

    #[test]
    fn passes_valid_json_through_unchanged() {
        let mw = RepairToolArgsMiddleware;
        let valid = r#"{"file_path":"a.html","content":"x"}"#;
        let mut c = call("write_file", valid);
        mw.repair_call("write_file", &mut c);
        assert_eq!(c.arguments, valid, "valid JSON must not be altered");
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;

    // --- #1: the two former O(N^2) repair_json passes are now single-pass O(N).
    //     These inputs froze the host (seconds-to-minutes) on the old code; assert
    //     the output is still correct (the test would never finish if it hung). ---

    #[test]
    fn trailing_comma_run_collapses_correctly() {
        // `{"k":[,,,...]}` (a botched list) used to remove one comma per pass → O(N^2).
        let input = format!("{{\"k\":[{}]}}", ",".repeat(50_000));
        let out = repair_tool_args("write_file", &input);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON after repair");
        assert!(
            v["k"].as_array().map(|a| a.is_empty()).unwrap_or(false),
            "comma run must collapse to []: {out}"
        );
    }

    #[test]
    fn missing_comma_run_inserts_correctly() {
        // `{"k0":"v" "k1":"v" ...}` used to replay N× O(N) Vec::insert → O(N^2).
        let n = 20_000;
        let mut s = String::from("{");
        for i in 0..n {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(&format!("\"k{i}\":\"v\""));
        }
        s.push('}');
        let out = repair_tool_args("write_file", &s);
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON after repair");
        assert_eq!(v["k0"], "v");
        assert_eq!(v[format!("k{}", n - 1)], "v");
    }

    #[test]
    fn oversized_input_is_returned_unchanged() {
        // Past the MAX_REPAIR_BYTES ceiling, repair is skipped and the original is
        // returned verbatim (the tool surfaces its own parse error).
        let big = format!("{{\"content\":\"{}\",}}", "x".repeat(600_000));
        assert_eq!(repair_tool_args("write_file", &big), big);
    }

    // --- #3: edit_file extractor selection is case-insensitive (canonical name). ---

    #[test]
    fn edit_file_extractor_is_case_insensitive() {
        // old_string carries an UNESCAPED double-quote — only the edit_file
        // specialized extractor recovers this; repair_json cannot. A model that
        // emits the name as `Edit_File` must route identically to `edit_file`.
        let input = r#"{"file_path": "a.py", "old_string": "say "hi" now", "new_string": "x"}"#;
        let lower = repair_tool_args("edit_file", input);
        let mixed = repair_tool_args("Edit_File", input);
        assert_eq!(
            lower, mixed,
            "tool-name casing must not change repair routing"
        );
        let v: serde_json::Value = serde_json::from_str(&mixed).expect("recovered to valid JSON");
        assert_eq!(v["file_path"], "a.py");
    }

    // --- #2 (key-scoping): valid non-path content with a drive-letter shape is
    //     identity — the Windows-path pass no longer corrupts it. ---

    #[test]
    fn valid_content_with_drive_shape_is_identity() {
        // A valid `content` value containing a drive-label + newline (`C:\n`) must
        // NOT be rewritten — `\n` is the model's intended newline, not a path
        // separator. Pre-key-scoping this doubled it to literal `\\n`, landing
        // broken code on disk.
        let input = r#"{"content":"print('C:\ndone')"}"#;
        assert_eq!(repair_tool_args("write_file", input), input);
    }
}
