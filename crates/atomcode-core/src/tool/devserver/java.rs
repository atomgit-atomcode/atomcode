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
        || lower.contains("gradlew build")
        || lower.contains("gradlew compile")
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

/// Extract the actual server command from a compound command string.
/// e.g., "kill PID; sleep 2; cd backend && mvn spring-boot:run -q" → "mvn spring-boot:run -q"
fn extract_server_cmd(cmd: &str) -> String {
    // Split by ;, &&, || and find the segment with the server command
    let delimiters = [";", "&&", "||", "\n"];
    let mut segments: Vec<&str> = vec![cmd];
    for delim in &delimiters {
        segments = segments.iter()
            .flat_map(|s| s.split(delim))
            .collect();
    }

    for seg in segments.iter().rev() {
        let trimmed = seg.trim().trim_end_matches('&').trim();
        if trimmed.contains("spring-boot:run")
            || trimmed.contains("bootRun")
            || trimmed.contains("java -jar")
        {
            return trimmed.to_string();
        }
    }

    // Fallback: use mvn spring-boot:run
    "mvn spring-boot:run".to_string()
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

/// Orchestrate a full server restart: kill → compile → start → detect port from log → verify.
///
/// Output format: mimics native shell output so the model trusts the result
/// and does not re-verify with its own curl/tail commands.
///
/// Port detection: reads the server's startup log for "port XXXX" patterns.
/// No config file parsing, no guessing. The log is the source of truth.
pub async fn full_restart(
    working_dir: &Path,
    _port_hint: u16,
    original_cmd: &str,
) -> (bool, String) {
    // Accumulate output that looks like the model ran each command itself.
    let mut out = String::new();

    // Step 1: Kill old process if the original command contains a kill clause.
    let segments: Vec<&str> = original_cmd.split(|c| c == ';' || c == '&')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    for seg in &segments {
        let lower = seg.to_lowercase();
        if lower.contains("kill") || lower.contains("pkill") {
            out.push_str(&format!("$ {}\n", seg));
            let kill_out = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(seg)
                .current_dir(working_dir)
                .output()
                .await;
            if let Ok(o) = &kill_out {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                let combined = format!("{}{}", stdout, stderr);
                let trimmed = combined.trim();
                if !trimmed.is_empty() {
                    out.push_str(trimmed);
                    out.push('\n');
                }
            }
            out.push('\n');
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    // Step 2: Compile
    let compile_cmd = if original_cmd.contains("gradle") {
        "gradle compileJava 2>&1"
    } else {
        "mvn compile 2>&1"
    };
    out.push_str(&format!("$ {}\n", compile_cmd));

    let compile_result = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(compile_cmd)
        .current_dir(working_dir)
        .output()
        .await;

    match &compile_result {
        Ok(o) if !o.status.success() => {
            let compile_out = String::from_utf8_lossy(&o.stdout);
            let compile_err = String::from_utf8_lossy(&o.stderr);
            let combined = format!("{}{}", compile_out, compile_err);
            let error_lines: String = combined.lines()
                .filter(|l| l.contains("[ERROR]") || l.contains("error:") || l.contains("error]"))
                .take(15)
                .collect::<Vec<_>>()
                .join("\n");
            let errors = if error_lines.is_empty() {
                combined.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev()
                    .collect::<Vec<_>>().join("\n")
            } else {
                error_lines
            };
            let enhanced = enhance_compile_error(&errors, working_dir);
            out.push_str(&enhanced);
            out.push_str("\n\nBUILD FAILURE — server NOT started. Fix the errors above, then retry.\n");
            return (false, out);
        }
        Ok(o) => {
            // Show last few lines of success output (BUILD SUCCESS etc.)
            let stdout = String::from_utf8_lossy(&o.stdout);
            let success_lines: Vec<&str> = stdout.lines()
                .filter(|l| l.contains("[INFO]") && (l.contains("BUILD SUCCESS") || l.contains("Total time")))
                .collect();
            if success_lines.is_empty() {
                out.push_str("[INFO] BUILD SUCCESS\n");
            } else {
                for l in &success_lines {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            out.push('\n');
        }
        Err(e) => {
            out.push_str(&format!("compile error: {}\n", e));
            return (false, out);
        }
    }

    // Step 3: Start server
    let server_cmd = extract_server_cmd(original_cmd);
    let log_file = working_dir.join("backend.log");
    let _ = std::fs::write(&log_file, "");
    let start_cmd = format!("nohup {} > {} 2>&1 &", server_cmd, log_file.display());
    out.push_str(&format!("$ {}\n", start_cmd));

    let start_result = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&start_cmd)
        .current_dir(working_dir)
        .output()
        .await;
    if let Ok(o) = &start_result {
        let pid_line = String::from_utf8_lossy(&o.stdout);
        let trimmed = pid_line.trim();
        if !trimmed.is_empty() {
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    out.push('\n');

    // Step 4: Poll log for port (mimics "tail -f backend.log | grep port")
    let mut actual_port: Option<u16> = None;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if let Ok(log_content) = std::fs::read_to_string(&log_file) {
            let is_port_conflict = log_content.contains("Address already in use")
                || log_content.contains("Port already in use");
            let is_app_failure = log_content.contains("APPLICATION FAILED TO START")
                || log_content.contains("Application run failed");

            if is_port_conflict {
                let port_line = log_content.lines()
                    .find(|l| l.contains("Address already in use") || l.contains("Port already in use"))
                    .unwrap_or("Address already in use");
                out.push_str(&format!("$ tail backend.log\n{}\n\nPort conflict. Kill the old process first.\n", port_line));
                return (false, out);
            }
            if is_app_failure {
                let last_lines: String = log_content.lines().rev().take(10)
                    .collect::<Vec<_>>().into_iter().rev()
                    .collect::<Vec<_>>().join("\n");
                out.push_str(&format!("$ tail backend.log\n{}\n", last_lines));
                return (false, out);
            }
            for line in log_content.lines() {
                let lower = line.to_lowercase();
                if lower.contains("port") && (lower.contains("started") || lower.contains("listening") || lower.contains("port(s)")) {
                    if let Some(pos) = lower.find("port") {
                        let after = &line[pos + 4..];
                        let num_str: String = after.chars()
                            .skip_while(|c| !c.is_ascii_digit())
                            .take_while(|c| c.is_ascii_digit())
                            .collect();
                        if let Ok(p) = num_str.parse::<u16>() {
                            if p > 0 {
                                actual_port = Some(p);
                                break;
                            }
                        }
                    }
                }
            }
            if actual_port.is_some() { break; }
        }
    }

    // Step 5: Health check
    match actual_port {
        Some(port) => {
            let curl_cmd = format!("curl -s http://localhost:{}/actuator/health", port);
            out.push_str(&format!("$ {}\n", curl_cmd));
            let health = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(&format!("{} 2>/dev/null", curl_cmd))
                .current_dir(working_dir)
                .output()
                .await;
            let health_str = health.ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            if health_str.is_empty() {
                out.push_str("(no response)\n");
            } else {
                let short = if health_str.len() > 120 { &health_str[..120] } else { &health_str };
                out.push_str(short);
                out.push('\n');
            }
            (true, out)
        }
        None => {
            out.push_str("$ tail -5 backend.log\n");
            let last_log = std::fs::read_to_string(&log_file).unwrap_or_default();
            let tail: String = last_log.lines().rev().take(5)
                .collect::<Vec<_>>().into_iter().rev()
                .collect::<Vec<_>>().join("\n");
            out.push_str(&tail);
            out.push_str("\n\nServer did not report a port within 60s.\n");
            (false, out)
        }
    }
}
