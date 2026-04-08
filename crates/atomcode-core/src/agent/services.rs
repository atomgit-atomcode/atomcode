use super::*;

impl AgentLoop {
    /// Detect already-running dev servers by probing common ports.
    /// Runs once at startup to populate active_services.
    /// Detect running services via `lsof` — shows actual listening ports with process names.
    /// No hardcoded ports. The process name (java/node/python) is the label.
    pub(crate) async fn detect_running_services(&mut self) {
        let output = tokio::process::Command::new("lsof")
            .args(&["-i", "-P", "-n", "-sTCP:LISTEN"])
            .output()
            .await;

        let stdout = match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return, // lsof not available or failed — skip silently
        };

        // Parse lsof output. Each line looks like:
        // node    80162 yubangxu   23u  IPv4 0x... TCP 127.0.0.1:3004 (LISTEN)
        // java    79842 yubangxu   45u  IPv6 0x... TCP *:8080 (LISTEN)
        for line in stdout.lines().skip(1) { // skip header
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 9 {
                continue;
            }
            let process = parts[0].to_lowercase();
            // Find the TCP address:port part
            // Match any TCP address:port — localhost, 127.0.0.1, [::1], *:
            let addr_part = parts.iter()
                .find(|p| p.contains(':') && (
                    p.contains("localhost") || p.contains("127.0.0.1")
                    || p.contains("[::1]") || p.starts_with("*:")
                ))
                .copied()
                .unwrap_or("");

            if let Some(colon) = addr_part.rfind(':') {
                if let Ok(port) = addr_part[colon + 1..].parse::<u16>() {
                    if port >= 1024 {
                        let url = format!("http://localhost:{}", port);
                        let label = format!("{} ({})", process, port);
                        self.active_services.insert(label, url);
                    }
                }
            }
        }
    }

    pub(crate) fn change_dir(&mut self, path: &str) {
        let new_path = if path.starts_with('/') {
            std::path::PathBuf::from(path)
        } else if path.starts_with('~') {
            dirs::home_dir()
                .map(|h| h.join(path.strip_prefix("~/").unwrap_or(&path[1..])))
                .unwrap_or_else(|| std::path::PathBuf::from(path))
        } else {
            let wd: PathBuf = self
                .turn_runner.context
                .working_dir
                .try_read()
                .map(|g| g.clone())
                .unwrap_or_default();
            wd.join(path)
        };

        let resolved = std::fs::canonicalize(&new_path).unwrap_or(new_path);
        if resolved.is_dir() {
            if let Ok(mut wd) = self.turn_runner.context.working_dir.try_write() {
                *wd = resolved.clone();
            }
            self.project_context_cache = None; // invalidate on dir change
            // Clear conversation history — old paths from previous directory will confuse the model
            self.conversation.messages.clear();
            self.conversation.turn_tracker = crate::conversation::turn::TurnTracker::new();
            self.session_files.clear();
            // Reload skills for the new working directory (project-level skills may differ)
            if let Ok(mut reg) = self.skill_registry.write() {
                reg.reload(&resolved);
            }
            // Reload code graph for the new project
            let graph_path = resolved.join(".atomcode").join("graph.bin");
            let new_graph = crate::graph::persist::load(&graph_path);
            eprintln!("[cd] reloaded graph from {:?}: nodes={}", graph_path, new_graph.node_count());
            // Swap graph data (reuse the same Arc, just replace contents)
            if let Ok(mut g) = self.turn_runner.context.graph.try_write() {
                *g = new_graph;
            }
            // Spawn new indexer for the new project
            let graph_clone = self.turn_runner.context.graph.clone();
            let wd_for_indexer = resolved.clone();
            tokio::spawn(async move {
                let mut indexer = crate::graph::indexer::GraphIndexer::new(
                    graph_clone.clone(), wd_for_indexer.clone(),
                );
                indexer.index_all().await;
                let gp = wd_for_indexer.join(".atomcode").join("graph.bin");
                if let Ok(g) = graph_clone.try_read() {
                    let _ = crate::graph::persist::save(&g, &gp);
                }
            });
            let _ = self
                .event_tx
                .send(AgentEvent::WorkingDirChanged(resolved));
        }
    }
}

/// Extract http://localhost:PORT URLs from tool output and store them.
/// Uses the command to guess a label (frontend/backend/service).
#[allow(dead_code)]
pub(crate) fn extract_service_urls(
    output: &str,
    cmd: &str,
    services: &mut std::collections::HashMap<String, String>,
) {
    // Find all http://localhost:NNNN patterns in the output.
    let mut i = 0;
    let _bytes = output.as_bytes();
    while i < output.len() {
        if let Some(pos) = output[i..].find("http://localhost:") {
            let start = i + pos;
            let after = start + "http://localhost:".len();
            // Extract port digits.
            let port_end = output[after..].find(|c: char| !c.is_ascii_digit())
                .map(|p| after + p)
                .unwrap_or(output.len());
            if port_end > after {
                let url = &output[start..port_end];
                // Guess label from the command.
                let cmd_lower = cmd.to_lowercase();
                let label = if cmd_lower.contains("vite") || cmd_lower.contains("npm run dev")
                    || cmd_lower.contains("next") || cmd_lower.contains("webpack")
                    || cmd_lower.contains("frontend") || cmd_lower.contains("yarn dev") {
                    "frontend"
                } else if cmd_lower.contains("spring") || cmd_lower.contains("mvn")
                    || cmd_lower.contains("gradle") || cmd_lower.contains("flask")
                    || cmd_lower.contains("uvicorn") || cmd_lower.contains("backend")
                    || cmd_lower.contains("cargo run") || cmd_lower.contains("go run") {
                    "backend"
                } else {
                    "service"
                };
                services.insert(label.to_string(), url.to_string());
                i = port_end;
            } else {
                i = after;
            }
        } else {
            break;
        }
    }
}
