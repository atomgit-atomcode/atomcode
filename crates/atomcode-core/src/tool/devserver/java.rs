//! Java/JVM dev server detection + compile error diagnosis + restart orchestration.
//!
//! Two key capabilities beyond basic detection:
//! 1. `enhance_compile_error` — extracts file:line from Maven/Gradle errors,
//!    reads the source code around that line, appends to tool result.
//! 2. `full_restart` — kills old process, compiles, starts, polls port, verifies.

use std::path::Path;

use super::DetectedServer;

/// Detect if a command is a Java dev server.
pub fn detect(cmd: &str) -> Option<DetectedServer> {
    let trimmed = cmd.trim();

    if trimmed.contains("spring-boot:run") {
        return Some(DetectedServer {
            label: "Spring Boot",
            default_port: 8080,
            pre_command: None,
        });
    }

    if trimmed.contains("gradle bootRun") || trimmed.contains("gradlew bootRun") {
        return Some(DetectedServer {
            label: "Spring Boot (Gradle)",
            default_port: 8080,
            pre_command: None,
        });
    }

    if trimmed.contains("java -jar") || trimmed.contains("java -cp") {
        return Some(DetectedServer {
            label: "Java Application",
            default_port: 8080,
            pre_command: None,
        });
    }

    None
}

/// Check if a bash command is a Java compile command.
pub fn is_compile_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    lower.contains("mvn compile")
        || lower.contains("mvn clean")
        || lower.contains("mvn package")
        || lower.contains("mvn install")
        || lower.contains("gradle compile")
        || lower.contains("gradle build")
}

/// Enhance a failed compile output with source code context.
///
/// Parses Maven/Gradle error output for `[ERROR] /path/File.java:[line,col]` patterns,
/// reads the source code around that line, and appends it to the output.
/// This saves the model from having to manually find and read the error location.
pub fn enhance_compile_error(output: &str, working_dir: &Path) -> String {
    if !output.contains("[ERROR]") && !output.contains("error:") {
        return output.to_string();
    }

    let mut enhanced = output.to_string();
    let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut snippets: Vec<String> = Vec::new();

    for line in output.lines() {
        // Maven pattern: [ERROR] /path/to/File.java:[42,15] error message
        // Also: /path/to/File.java:42: error: message
        if let Some((file, line_num)) = extract_error_location(line) {
            let key = format!("{}:{}", file, line_num);
            if seen_files.contains(&key) {
                continue;
            }
            seen_files.insert(key);

            // Try absolute path first, then relative to working_dir
            let file_path = if Path::new(&file).exists() {
                std::path::PathBuf::from(&file)
            } else {
                working_dir.join(&file)
            };

            if let Ok(content) = std::fs::read_to_string(&file_path) {
                let lines: Vec<&str> = content.lines().collect();
                let total = lines.len();
                let ln = line_num.saturating_sub(1); // 0-indexed
                let start = ln.saturating_sub(3);
                let end = (ln + 4).min(total);

                let snippet: String = lines[start..end]
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        let num = start + i + 1;
                        let marker = if num == line_num { " >>>" } else { "    " };
                        format!("{}{:4}| {}", marker, num, l)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                let short_name = file_path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| file.clone());

                snippets.push(format!(
                    "\n--- Error in {} line {} ---\n{}\n",
                    short_name, line_num, snippet,
                ));
            }

            // Max 3 error locations to avoid flooding
            if snippets.len() >= 3 {
                break;
            }
        }
    }

    if !snippets.is_empty() {
        enhanced.push_str("\n\n[AUTO-DIAGNOSIS: Source code at error locations]");
        for s in &snippets {
            enhanced.push_str(s);
        }
        enhanced.push_str("Fix the lines marked with >>> then compile again.");
    }

    enhanced
}

/// Extract file path and line number from a compile error line.
fn extract_error_location(line: &str) -> Option<(String, usize)> {
    // Maven: [ERROR] /path/File.java:[42,15] message
    if line.contains("[ERROR]") && line.contains(".java:") {
        let after_error = line.find("[ERROR]").map(|p| &line[p + 7..]).unwrap_or(line).trim();
        if let Some(java_pos) = after_error.find(".java:") {
            let file_end = java_pos + 5; // include ".java"
            let file = after_error[..file_end].trim().to_string();
            let after_colon = &after_error[file_end + 1..];
            // Parse [line,col] or just line
            let num_str: String = after_colon
                .trim_start_matches('[')
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(num) = num_str.parse::<usize>() {
                return Some((file, num));
            }
        }
    }

    // Generic: /path/File.java:42: error:
    if line.contains(".java:") && (line.contains("error:") || line.contains("error]")) {
        if let Some(java_pos) = line.find(".java:") {
            // Walk backwards to find file path start
            let before = &line[..java_pos + 5];
            let file_start = before.rfind(|c: char| c.is_whitespace()).map(|p| p + 1).unwrap_or(0);
            let file = before[file_start..].to_string();
            let after_colon = &line[java_pos + 6..];
            let num_str: String = after_colon.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(num) = num_str.parse::<usize>() {
                return Some((file, num));
            }
        }
    }

    None
}

/// Orchestrate a full Java server restart: kill → compile → start → poll port.
///
/// Returns the combined output. If compile fails, returns the error with diagnosis.
/// If everything succeeds, returns the startup confirmation with port status.
pub async fn full_restart(
    working_dir: &Path,
    port: u16,
    original_cmd: &str,
) -> (bool, String) {
    let mut output = String::new();

    // Step 1: Kill old process on the port
    let kill_result = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(format!("lsof -ti:{} | xargs kill -9 2>/dev/null; sleep 1", port))
        .current_dir(working_dir)
        .output()
        .await;
    if let Ok(o) = &kill_result {
        if o.status.success() {
            output.push_str(&format!("[Step 1/4] Killed old process on port {}\n", port));
        } else {
            output.push_str(&format!("[Step 1/4] No process on port {} (clean start)\n", port));
        }
    }

    // Step 2: Compile
    let compile_cmd = if original_cmd.contains("gradle") {
        "gradle compileJava 2>&1 | tail -30"
    } else {
        "mvn compile 2>&1 | tail -30"
    };
    let compile_result = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(compile_cmd)
        .current_dir(working_dir)
        .output()
        .await;

    match compile_result {
        Ok(o) => {
            let compile_out = String::from_utf8_lossy(&o.stdout);
            let compile_err = String::from_utf8_lossy(&o.stderr);
            let combined = format!("{}{}", compile_out, compile_err);
            if !o.status.success() {
                let enhanced = enhance_compile_error(&combined, working_dir);
                output.push_str(&format!("[Step 2/4] Compile FAILED:\n{}\n", enhanced));
                return (false, output);
            }
            output.push_str("[Step 2/4] Compile passed\n");
        }
        Err(e) => {
            output.push_str(&format!("[Step 2/4] Compile error: {}\n", e));
            return (false, output);
        }
    }

    // Step 3: Start server in background
    let start_cmd = format!(
        "nohup {} >/dev/null 2>&1 &",
        original_cmd.trim().trim_end_matches('&').trim()
    );
    let _ = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&start_cmd)
        .current_dir(working_dir)
        .output()
        .await;
    output.push_str("[Step 3/4] Server starting...\n");

    // Step 4: Poll port until ready
    let mut ready = false;
    for i in 0..20 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
            ready = true;
            output.push_str(&format!("[Step 4/4] Port {} ready after {}s\n", port, (i + 1) * 2));
            break;
        }
    }
    if !ready {
        output.push_str(&format!(
            "[Step 4/4] Port {} not responding after 40s. Check: tail -30 backend.log\n",
            port
        ));
        return (false, output);
    }

    (true, output)
}
