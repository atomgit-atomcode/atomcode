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
    /// If true, the edit should be REJECTED — don't write to disk.
    /// The model must fix its edit and retry.
    pub rejected: bool,
}

/// Run all pre-write validations on the content in memory:
/// 1. Duplicate block detection
/// 2. Brace auto-fix
/// 3. HTML tag auto-fix
///
/// Returns a `ValidateResult` with the (possibly fixed) content.
/// The caller should write `fixed_content` to disk, then optionally call
/// `post_edit_syntax_check` for on-disk syntax validation.
/// `content` is the post-edit content (what would be written to disk).
/// `original_content` is the pre-edit content (for delta validation).
pub async fn validate_and_fix(
    content: &str,
    _file_path: &str,
    _new_string: &str,
    _original_content: &str,
) -> ValidateResult {
    let warnings: Vec<String> = Vec::new();
    let current = content.to_string();

    // Duplicate detection REMOVED.
    // Zero proven positive value across all sessions.
    // False positives on HTML (repeated <div>/<button>) caused edit rejections.
    // auto-compile catches real structural errors with better diagnostics.

    // Structural checks (brace balance, HTML balance, Vue SFC) REMOVED.
    // auto-compile after edit catches all structural errors with better
    // diagnostics (line number + error type from compiler).
    // Delta validation was net negative: 7 false rejections today, 0 true catches.
    // Principle: "don't block the model, enhance the tool."

    if !warnings.is_empty() {
        return ValidateResult {
            fixed_content: current,
            warnings,
            was_fixed: false,
            rejected: true,
        };
    }

    ValidateResult {
        fixed_content: current,
        warnings: vec![],
        was_fixed: false,
        rejected: false,
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
                        format!(
                            "\n\u{26a0} SYNTAX ERROR: {} is not valid JSON. Fix before proceeding.",
                            file_path
                        )
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

            // NOTE: auto-build removed — violates "do NOT deploy" principle.
            // Model should run build manually via bash when it wants to verify.

            if warnings.is_empty() {
                return String::new();
            }
            return format!("\n⚠ VUE SYNTAX: {}", warnings.join("; "));
        }
        "py" => Some((
            "python3",
            vec![
                "-m".to_string(),
                "py_compile".to_string(),
                file_path.to_string(),
            ],
        )),
        _ => None,
    };

    if let Some((program, args)) = cmd {
        let mut command = tokio::process::Command::new(program);
        command.args(&args);
        crate::process_utils::suppress_console_window(&mut command);
        match command.output().await {
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
