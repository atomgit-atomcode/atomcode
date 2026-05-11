use super::*;

impl AgentLoop {
    /// Auto-diagnose: when user mentions error keywords, scan log files for recent errors
    /// and append them to the user message. The model starts Turn 1 with the real error.
    pub(crate) async fn auto_diagnose_errors(&self, content: &str) -> String {
        let lower = content.to_lowercase();
        let has_error_keyword = [
            "错误",
            "报错",
            "失败",
            "error",
            "500",
            "404",
            "crash",
            "异常",
            "exception",
            "内部错误",
            "not work",
            "不行",
            "不好使",
            "bug",
            "不对",
            "有问题",
            "不正确",
            "不应该",
            "还是不行",
            "没有用",
            "没效果",
            "显示错误",
            "返回错误",
            "结果不对",
            "broken",
            "wrong",
            "incorrect",
        ]
        .iter()
        .any(|k| lower.contains(k));

        if !has_error_keyword {
            return content.to_string();
        }

        let wd: PathBuf = self
            .turn_runner
            .context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Find log files: *.log in project root and common subdirs
        let log_candidates = [
            "backend.log",
            "server.log",
            "app.log",
            "nohup.out",
            "backend/backend.log",
            "backend/nohup.out",
            "logs/app.log",
            "log/development.log",
        ];

        let mut diagnostics = Vec::new();

        for log_name in &log_candidates {
            let log_path = wd.join(log_name);
            if !log_path.exists() {
                continue;
            }

            // Check if log is stale (mtime > 5 min ago).
            // Stale logs contain only old startup output, not the runtime error
            // the user is reporting. Still scan but tag as stale.
            let is_stale = std::fs::metadata(&log_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|mtime| mtime.elapsed().unwrap_or_default().as_secs() > 300)
                .unwrap_or(false);

            if let Ok(output) = {
                let mut cmd = tokio::process::Command::new("grep");
                cmd.args(&[
                    "-i",
                    "-E",
                    "error|exception|fail|caused by",
                    &log_path.to_string_lossy(),
                ]);
                crate::process_utils::suppress_console_window(&mut cmd);
                cmd.output().await
            } {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.trim().is_empty() {
                    let lines: Vec<&str> = stdout.lines().collect();
                    let start = lines.len().saturating_sub(15);
                    let recent = lines[start..].join("\n");
                    if is_stale {
                        // Stale logs (>5min) are noise — skip injection entirely.
                        // The model can read them itself if needed.
                    } else {
                        diagnostics.push(format!("[Auto-detected from {}:]\n{}", log_name, recent));
                    }
                }
            }
        }

        // Fallback: if all logs are stale or empty, try to capture live output
        // from running Java/Node processes via their recent stderr.
        let all_stale_or_empty =
            diagnostics.is_empty() || diagnostics.iter().all(|d| d.contains("STALE"));
        if all_stale_or_empty {
            // Try Spring Boot default log location
            let spring_log = wd.join("backend/logs/spring.log");
            if spring_log.exists() {
                if let Ok(output) = {
                    let mut cmd = tokio::process::Command::new("tail");
                    cmd.args(&["-50", &spring_log.to_string_lossy()]);
                    crate::process_utils::suppress_console_window(&mut cmd);
                    cmd.output().await
                } {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let error_lines: Vec<&str> = stdout
                        .lines()
                        .filter(|l| {
                            let low = l.to_lowercase();
                            low.contains("error")
                                || low.contains("exception")
                                || low.contains("caused by")
                        })
                        .collect();
                    if !error_lines.is_empty() {
                        let start = error_lines.len().saturating_sub(15);
                        diagnostics.push(format!(
                            "[Auto-detected from logs/spring.log:]\n{}",
                            error_lines[start..].join("\n")
                        ));
                    }
                }
            }
        }

        if diagnostics.is_empty() {
            return content.to_string();
        }

        // Phase 2: Parse stack traces for file:line references, extract function code via tree-sitter.
        // This gives the model the actual broken code so it can edit directly in Turn 1.
        let diag_text = diagnostics.join("\n");
        let mut extracted_code = Vec::new();
        let mut searcher = self.turn_runner.context.semantic.lock().await;

        // Match patterns like "FileName.java:45" or "file.py:123" or "file.rs:45"
        let file_line_re = regex::Regex::new(r"(\w+\.\w+):(\d+)")
            .unwrap_or_else(|_| regex::Regex::new(".^").unwrap());
        let mut seen_files = std::collections::HashSet::new();

        for cap in file_line_re.captures_iter(&diag_text) {
            let filename = &cap[1];
            let line_no: usize = cap[2].parse().unwrap_or(0);
            if line_no == 0 || seen_files.contains(filename) {
                continue;
            }

            // Find the actual file path in the project
            let file_path = Self::find_file_in_project(&wd, filename);
            if let Some(ref fp) = file_path {
                seen_files.insert(filename.to_string());
                // Use tree-sitter to find the enclosing function at this line
                if let Some(symbols) = searcher.list_symbols(fp) {
                    if let Some(sym) = symbols
                        .iter()
                        .find(|s| line_no >= s.start_line && line_no <= s.end_line)
                    {
                        // Extract the function code
                        if let Some(slice) = searcher.extract_symbol(fp, &sym.name) {
                            let mut code = format!(
                                "[Source: {} → {}() lines {}-{}]\n",
                                filename, sym.name, slice.start_line, slice.end_line
                            );
                            for (i, line) in slice.text.lines().enumerate() {
                                code.push_str(&format!("{:4}| {}\n", slice.start_line + i, line));
                            }
                            extracted_code.push(code);
                            if extracted_code.len() >= 2 {
                                break;
                            } // Max 2 functions
                        }
                    }
                }
            }
        }
        // If the stack trace mentions a specific object/call (e.g., "tagRepository.count"),
        // scan the entire file for ALL similar calls so the model can fix them all at once.
        // This prevents the "fix one call, miss nine others" pattern.
        {
            let obj_re = regex::Regex::new(r"(\w+Repository|\w+Service|\w+Dao)\.\w+")
                .unwrap_or_else(|_| regex::Regex::new(".^").unwrap());
            // First pass: collect object names to scan
            let mut objects_to_scan: Vec<String> = Vec::new();
            for code in &extracted_code {
                for cap in obj_re.captures_iter(code) {
                    let obj_name = cap[1].to_string();
                    if !objects_to_scan.contains(&obj_name) {
                        objects_to_scan.push(obj_name);
                    }
                }
            }
            // Second pass: scan and append results
            for obj_name in &objects_to_scan {
                for fp in &seen_files {
                    if let Some(file_path) = Self::find_file_in_project(&wd, fp) {
                        if let Some(call_list) =
                            searcher.find_similar_calls(&file_path, &obj_name.to_lowercase())
                        {
                            extracted_code.push(format!(
                                "\n[All {} calls in this file — fix ALL at once:]\n{}",
                                obj_name, call_list
                            ));
                        }
                    }
                }
            }
        }

        drop(searcher);

        // Phase 3: Auto-inject call chain from code graph.
        {
            let graph = self.turn_runner.context.graph.read().await;
            if graph.is_ready() {
                let mut injected_chains = Vec::new();
                let mut fn_names: Vec<String> = Vec::new();

                for code in &extracted_code {
                    if let Some(start) = code.find("→ ") {
                        let rest = &code[start + 4..];
                        if let Some(end) = rest.find("()") {
                            fn_names.push(rest[..end].to_string());
                        }
                    }
                }

                let fn_re = regex::Regex::new(r"\b([a-z_][a-z0-9_]{3,})\b")
                    .unwrap_or_else(|_| regex::Regex::new(".^").unwrap());
                for cap in fn_re.captures_iter(content) {
                    let name = &cap[1];
                    if !graph.find_by_name(name).is_empty() && !fn_names.contains(&name.to_string())
                    {
                        fn_names.push(name.to_string());
                    }
                }

                for fn_name in fn_names.iter().take(2) {
                    if let Some(chain) = graph.call_chain_summary(fn_name) {
                        injected_chains.push(chain);
                    }
                }

                if !injected_chains.is_empty() {
                    extracted_code.push(format!(
                        "\n[Code graph — execution flow (trace the chain to find the root cause):]\n{}",
                        injected_chains.join("\n")
                    ));
                }
            }
        }

        // Extract exception signature (e.g. "TransactionRequiredException") for recurrence detection.
        let exception_re = regex::Regex::new(r"(\w+Exception|\w+Error)")
            .unwrap_or_else(|_| regex::Regex::new(".^").unwrap());
        let current_exception = exception_re
            .captures_iter(&diag_text)
            .next()
            .map(|c| c[1].to_string())
            .unwrap_or_default();

        let mut result = format!("{}\n\n{}", content, diagnostics.join("\n\n"));

        // If the same exception recurs after a previous fix attempt, tell the model
        // its approach isn't working and it needs a different strategy.
        if !current_exception.is_empty()
            && current_exception == self.discipline_state.last_diagnosed_error
        {
            result.push_str(&format!(
                "\n\n[RECURRING ERROR: {} appeared again after your previous fix. \
                 Your last approach did not resolve it. Try a fundamentally different fix — \
                 e.g. add @Transactional at the method level instead of wrapping individual calls.]",
                current_exception
            ));
        }
        // Store for next comparison (caller updates self.discipline_state.last_diagnosed_error)
        // We embed it in the result with a hidden marker for the caller to extract.
        if !current_exception.is_empty() {
            result.push_str(&format!("\n<!-- diag_exception:{} -->", current_exception));
        }

        if !extracted_code.is_empty() {
            result
                .push_str("\n\n[Relevant source code from stack trace — you can edit directly:]\n");
            result.push_str(&extracted_code.join("\n"));
        }
        result
    }

    /// Find a file by name in the project directory (searches up to 4 levels deep).
    pub(crate) fn find_file_in_project(
        wd: &std::path::Path,
        filename: &str,
    ) -> Option<std::path::PathBuf> {
        fn walk(dir: &std::path::Path, target: &str, depth: usize) -> Option<std::path::PathBuf> {
            if depth > 4 {
                return None;
            }
            let entries = std::fs::read_dir(dir).ok()?;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str == target && entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    return Some(entry.path());
                }
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && !crate::tool::should_skip_dir(&name_str)
                {
                    if let Some(found) = walk(&entry.path(), target, depth + 1) {
                        return Some(found);
                    }
                }
            }
            None
        }
        walk(wd, filename, 0)
    }
}
