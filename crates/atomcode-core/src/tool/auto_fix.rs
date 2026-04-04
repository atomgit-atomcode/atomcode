//! Pre-write validation and auto-fix for edited content.
//!
//! All checks operate on in-memory content — nothing is written to disk.
//! The caller is responsible for writing the validated/fixed content.
//!
//! Post-write syntax check (`post_edit_syntax_check`) needs the file on disk
//! (runs external commands like `node --check`) and is called separately after write.

/// Result of `validate_and_fix`: the (possibly fixed) content, any warnings,
/// and whether any auto-fix was applied.
pub struct ValidateResult {
    pub fixed_content: String,
    pub warnings: Vec<String>,
    pub was_fixed: bool,
}

/// Run all pre-write validations on the content in memory:
/// 1. Duplicate block detection
/// 2. Brace auto-fix
/// 3. HTML tag auto-fix
///
/// Returns a `ValidateResult` with the (possibly fixed) content.
/// The caller should write `fixed_content` to disk, then optionally call
/// `post_edit_syntax_check` for on-disk syntax validation.
pub async fn validate_and_fix(content: &str, file_path: &str, new_string: &str) -> ValidateResult {
    let mut warnings: Vec<String> = Vec::new();
    let mut was_fixed = false;
    let mut current = content.to_string();

    // 1. Duplicate detection (warning only, no fix)
    let dup_warn = detect_duplicate_blocks(&current, new_string);
    if !dup_warn.is_empty() {
        warnings.push(dup_warn);
    }

    // 2. Brace auto-fix
    match fix_braces(&current, file_path) {
        BraceFixResult::Balanced => {}
        BraceFixResult::AutoFixed(fixed, msg) => {
            current = fixed;
            warnings.push(msg);
            was_fixed = true;
        }
        BraceFixResult::CannotFix(msg) => {
            warnings.push(msg);
        }
    }

    // 3. HTML tag auto-fix
    match fix_html_tags(&current, file_path) {
        HtmlFixResult::Balanced => {}
        HtmlFixResult::AutoFixed(fixed, msg) => {
            current = fixed;
            warnings.push(msg);
            was_fixed = true;
        }
        HtmlFixResult::CannotFix(msg) => {
            warnings.push(msg);
        }
    }

    ValidateResult {
        fixed_content: current,
        warnings,
        was_fixed,
    }
}

/// Post-edit syntax check for common file types.
/// Runs a fast, non-destructive check and returns a warning if syntax is broken.
/// This needs the file to be on disk — call AFTER writing.
pub async fn post_edit_syntax_check(file_path: &str) -> String {
    let ext = file_path.rsplit('.').next().unwrap_or("");
    let cmd = match ext {
        "js" | "mjs" | "cjs" => Some(("node", vec!["--check".to_string(), file_path.to_string()])),
        "json" => {
            // Validate JSON by attempting parse
            return match tokio::fs::read_to_string(file_path).await {
                Ok(content) => {
                    if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                        format!("\n\u{26a0} SYNTAX ERROR: {} is not valid JSON. Fix before proceeding.", file_path)
                    } else {
                        String::new()
                    }
                }
                Err(_) => String::new(),
            };
        }
        "ts" | "tsx" => {
            return String::new();
        }
        "vue" | "svelte" => {
            // Quick checks for common Vue SFC errors:
            // 1. Nested backticks in <script> (template strings containing `)
            // 2. Fast build check if no dev server is running
            let mut warnings = Vec::new();

            if let Ok(content) = tokio::fs::read_to_string(file_path).await {
                // Check for nested backticks in <script> section
                if let Some(script_start) = content.find("<script") {
                    let script_end = content.find("</script>").unwrap_or(content.len());
                    let script = &content[script_start..script_end];
                    // Count backticks — odd number means unclosed template string
                    let backtick_count = script.chars().filter(|c| *c == '`').count();
                    if backtick_count % 2 != 0 {
                        warnings.push(format!(
                            "Unclosed template string (`) in <script> — {} backticks found (odd). \
                             Use regular strings ('') for data containing backticks.",
                            backtick_count
                        ));
                    }
                }
            }

            // If no dev server log exists, run a quick build to catch errors
            let file_dir = std::path::Path::new(file_path);
            let project_root = file_dir.ancestors()
                .find(|p| p.join("package.json").exists());
            if let Some(root) = project_root {
                let log_exists = root.join("frontend.log").exists()
                    || root.join("backend.log").exists();
                if !log_exists {
                    // No dev server running — do a quick build check
                    if let Ok(output) = tokio::process::Command::new("sh")
                        .args(["-c", "npm run build 2>&1 | head -20"])
                        .current_dir(root)
                        .output()
                        .await
                    {
                        let out = String::from_utf8_lossy(&output.stdout);
                        let err = String::from_utf8_lossy(&output.stderr);
                        let combined = format!("{}{}", out, err);
                        if !output.status.success() || combined.to_lowercase().contains("error") {
                            let err_lines: String = combined.lines()
                                .filter(|l| l.to_lowercase().contains("error"))
                                .take(3)
                                .collect::<Vec<_>>()
                                .join("\n");
                            if !err_lines.is_empty() {
                                warnings.push(format!("Build error:\n{}", err_lines));
                            }
                        }
                    }
                }
            }

            if warnings.is_empty() {
                return String::new();
            }
            return format!("\n⚠ VUE SYNTAX: {}", warnings.join("; "));
        }
        "py" => Some(("python3", vec!["-m".to_string(), "py_compile".to_string(), file_path.to_string()])),
        _ => None,
    };

    if let Some((program, args)) = cmd {
        match tokio::process::Command::new(program)
            .args(&args)
            .output()
            .await
        {
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let first_lines: String = stderr.lines().take(3).collect::<Vec<_>>().join("\n");
                format!("\n\u{26a0} SYNTAX ERROR in {}:\n{}", file_path, first_lines)
            }
            _ => String::new(),
        }
    } else {
        String::new()
    }
}

// ── Internal types and functions ──

/// Detect if an edit introduced duplicate blocks (a common weak-model failure mode).
/// Checks if new_string (>= 3 non-blank lines) appears more than once in the result.
/// Returns a warning string if duplicates found, empty string otherwise.
fn detect_duplicate_blocks(new_content: &str, new_string: &str) -> String {
    let sig_lines: Vec<&str> = new_string.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if sig_lines.len() < 3 {
        return String::new();
    }
    // Use first 3 significant lines as a fingerprint
    let fingerprint: Vec<&str> = sig_lines[..3.min(sig_lines.len())].to_vec();
    let content_lines: Vec<&str> = new_content.lines().map(|l| l.trim()).collect();

    let mut hits = 0usize;
    let mut i = 0;
    while i + fingerprint.len() <= content_lines.len() {
        if content_lines[i..i + fingerprint.len()] == fingerprint[..] {
            hits += 1;
            i += fingerprint.len();
        } else {
            i += 1;
        }
    }

    if hits > 1 {
        return format!(
            "\n\u{26a0} WARNING: The edit introduced DUPLICATE code blocks ({} copies detected). \
             This is likely a bug. Review the file and remove the duplicate.",
            hits
        );
    }

    // Secondary check: scan for consecutive duplicate non-trivial lines.
    // Catches cases where two edit regions produce the same declaration.
    let raw_lines: Vec<&str> = new_content.lines().collect();
    let mut dup_lines: Vec<(usize, &str)> = Vec::new();
    for i in 1..raw_lines.len() {
        let prev = raw_lines[i - 1].trim();
        let curr = raw_lines[i].trim();
        if prev == curr
            && !curr.is_empty()
            && curr.len() > 10  // ignore short lines like `}` or `return`
            && !curr.starts_with("//") && !curr.starts_with("*")
        {
            dup_lines.push((i + 1, curr)); // 1-indexed
        }
    }

    if !dup_lines.is_empty() {
        let examples: String = dup_lines.iter()
            .take(3)
            .map(|(line, text)| format!("  L{}: {}", line, text))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "\n\u{26a0} WARNING: {} consecutive duplicate line(s) detected after edit:\n{}\nRemove the duplicates.",
            dup_lines.len(), examples
        )
    } else {
        String::new()
    }
}

enum BraceFixResult {
    Balanced,
    AutoFixed(String, String),   // (fixed_content, message)
    CannotFix(String),
}

/// Try to fix brace imbalance in memory. Returns the fixed content if successful.
fn fix_braces(content: &str, file_path: &str) -> BraceFixResult {
    let ext = file_path.rsplit('.').next().unwrap_or("");
    if !matches!(ext, "js" | "ts" | "tsx" | "jsx" | "vue" | "svelte" | "java" | "rs" | "go" | "c" | "cpp" | "cs") {
        return BraceFixResult::Balanced;
    }

    // Count brace balance with string awareness
    let lines: Vec<&str> = content.lines().collect();
    let mut depth = 0i64;
    let mut in_string = false;
    let mut escape = false;
    let mut string_char = ' ';

    // Track depth at each line end to find insertion points
    let mut line_depths: Vec<i64> = Vec::with_capacity(lines.len());
    for line in &lines {
        for ch in line.chars() {
            if escape { escape = false; continue; }
            if ch == '\\' && in_string { escape = true; continue; }
            if in_string {
                if ch == string_char { in_string = false; }
                continue;
            }
            match ch {
                '\'' | '"' | '`' => { in_string = true; string_char = ch; }
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }
        line_depths.push(depth);
    }

    if depth == 0 {
        return BraceFixResult::Balanced;
    }
    if depth < 0 {
        // Extra closing braces — too risky to auto-fix
        return BraceFixResult::CannotFix(format!(
            "\n\u{26a0} BRACE MISMATCH in {}: {} extra closing '}}'. Remove the extra. Fix NOW.",
            file_path, depth.abs()
        ));
    }
    if depth > 3 {
        // Too many missing — likely a structural problem, don't guess
        return BraceFixResult::CannotFix(format!(
            "\n\u{26a0} BRACE MISMATCH in {}: {} unclosed '{{'. Too many to auto-fix. Fix manually.",
            file_path, depth
        ));
    }

    // Find insertion points: for each missing `}`, find the last line where
    // depth drops to the target level. Insert `}` after that line.
    let mut fixed_lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let mut remaining = depth;

    // Work backwards: find lines where depth is highest and insert `}` after them
    for target_depth in (1..=depth).rev() {
        // Find the last line where depth == target_depth (where the unclosed block ends)
        let mut insert_after = None;
        for i in (0..line_depths.len()).rev() {
            if line_depths[i] >= target_depth && !lines[i].trim().is_empty() {
                insert_after = Some(i);
                break;
            }
        }
        if let Some(idx) = insert_after {
            // Determine indentation: match the line that opened this block
            let indent = if idx > 0 {
                let prev_line = &fixed_lines[idx];
                let spaces = prev_line.len() - prev_line.trim_start().len();
                if spaces >= 2 { spaces - 2 } else { 0 }
            } else { 0 };
            let closing = format!("{}}}", " ".repeat(indent));
            fixed_lines.insert(idx + 1, closing);
            remaining -= 1;
        }
    }

    if remaining > 0 {
        return BraceFixResult::CannotFix(format!(
            "\n\u{26a0} BRACE MISMATCH in {}: {} unclosed '{{'. Could not determine where to insert closing braces. Fix NOW.",
            file_path, depth
        ));
    }

    // Build the fixed content in memory (do NOT write to disk)
    let new_content = if content.ends_with('\n') {
        format!("{}\n", fixed_lines.join("\n"))
    } else {
        fixed_lines.join("\n")
    };

    BraceFixResult::AutoFixed(
        new_content,
        format!(
            "\n[AUTO-FIXED: inserted {} missing closing '}}' in {}. File is now balanced.]",
            depth, file_path
        ),
    )
}

enum HtmlFixResult {
    Balanced,
    AutoFixed(String, String),   // (fixed_content, message)
    CannotFix(String),
}

/// Check and auto-fix HTML tag balance for Vue/HTML/Svelte files in memory.
fn fix_html_tags(content: &str, file_path: &str) -> HtmlFixResult {
    let ext = file_path.rsplit('.').next().unwrap_or("");
    if !matches!(ext, "vue" | "html" | "svelte" | "htm" | "jsx" | "tsx") {
        return HtmlFixResult::Balanced;
    }

    let lines: Vec<&str> = content.lines().collect();

    // Find <template> section boundaries for Vue files
    let (tpl_start, tpl_end) = if ext == "vue" {
        let s = lines.iter().position(|l| l.trim_start().starts_with("<template")).unwrap_or(0);
        let e = lines.iter().rposition(|l| l.trim_start().starts_with("</template>")).unwrap_or(lines.len());
        (s, e)
    } else {
        (0, lines.len())
    };
    if tpl_start >= tpl_end { return HtmlFixResult::Balanced; }

    let tags = ["div", "section", "main", "aside", "article", "nav", "header", "footer", "form", "ul", "ol"];
    let mut fixes: Vec<String> = Vec::new();
    let mut fixed_lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    let mut any_fixed = false;

    for tag in &tags {
        let open_pattern = format!("<{}", tag);
        let close_pattern = format!("</{}>", tag);

        let tpl_content: String = fixed_lines[tpl_start..tpl_end].join("\n");
        let opens = tpl_content.matches(&open_pattern).count();
        let closes = tpl_content.matches(&close_pattern).count();

        if opens > closes {
            let missing = opens - closes;
            for _ in 0..missing {
                let mut depth = 0i32;
                for i in (tpl_start..tpl_end).rev() {
                    let trimmed = fixed_lines[i].trim();
                    if trimmed.contains(&close_pattern) { depth += 1; }
                    if trimmed.contains(&open_pattern) {
                        if depth > 0 { depth -= 1; }
                        else {
                            let indent = fixed_lines[i].len() - fixed_lines[i].trim_start().len();
                            let closing = format!("{}</{}>", " ".repeat(indent), tag);
                            let insert_after = tpl_end - 1;
                            fixed_lines.insert(insert_after, closing);
                            fixes.push(format!("inserted </{}> at L{}", tag, insert_after + 1));
                            any_fixed = true;
                            break;
                        }
                    }
                }
            }
        } else if closes > opens {
            fixes.push(format!("<{}> has {} extra closing tag(s) — remove manually", tag, closes - opens));
        }
    }

    if any_fixed {
        let new_content = if content.ends_with('\n') {
            format!("{}\n", fixed_lines.join("\n"))
        } else {
            fixed_lines.join("\n")
        };
        return HtmlFixResult::AutoFixed(
            new_content,
            format!(
                "\n[AUTO-FIXED HTML: {}. File rewritten.]",
                fixes.join(", ")
            ),
        );
    }

    if !fixes.is_empty() {
        HtmlFixResult::CannotFix(format!(
            "\n\u{26a0} HTML TAG MISMATCH in {}: {}. Fix NOW.",
            file_path, fixes.join("; ")
        ))
    } else {
        HtmlFixResult::Balanced
    }
}

/// Check brace/bracket balance after edit. Catches missing closing `}` — the most
/// common weak-model error in multi-edit of large files.
#[allow(dead_code)]
pub fn check_brace_balance(content: &str, file_path: &str) -> String {
    let ext = file_path.rsplit('.').next().unwrap_or("");
    if !matches!(ext, "js" | "ts" | "tsx" | "jsx" | "vue" | "svelte" | "java" | "rs" | "go" | "c" | "cpp" | "cs" | "json") {
        return String::new();
    }

    let mut braces = 0i64;
    let mut in_string = false;
    let mut escape = false;
    let mut string_char = ' ';
    // Track where the deepest unmatched `{` is — likely where `}` is missing.
    let mut max_depth = 0i64;
    let mut max_depth_line = 0usize;
    let mut line_num = 1usize;

    for ch in content.chars() {
        if ch == '\n' { line_num += 1; }
        if escape { escape = false; continue; }
        if ch == '\\' && in_string { escape = true; continue; }
        if in_string {
            if ch == string_char { in_string = false; }
            continue;
        }
        match ch {
            '\'' | '"' | '`' => { in_string = true; string_char = ch; }
            '{' => {
                braces += 1;
                if braces > max_depth {
                    max_depth = braces;
                    max_depth_line = line_num;
                }
            }
            '}' => { braces -= 1; }
            _ => {}
        }
    }

    if braces == 0 {
        String::new()
    } else if braces > 0 {
        format!(
            "\n\u{26a0} BRACE MISMATCH in {}: {} unclosed '{{'. \
             Deepest nesting at line {}. Add {} closing '}}' near that function's end. Fix NOW.",
            file_path, braces, max_depth_line, braces
        )
    } else {
        format!(
            "\n\u{26a0} BRACE MISMATCH in {}: {} extra closing '}}'. Remove the extra. Fix NOW.",
            file_path, braces.abs()
        )
    }
}
