use super::*;

/// Monotonic counter for synthetic local-shell call ids. `messages.len()`
/// is NOT unique — `add_user_message` merges consecutive User text without
/// growing the vec, so two `!` in a row would collide. A process-wide
/// counter guarantees a unique id regardless of conversation merging.
static LOCAL_SHELL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl AgentLoop {
    /// Handle a user-invoked `!cmd`. Executes the shell command in the
    /// agent's working dir, streams output to the TUI as a synthetic tool
    /// call, and records the command + output into the conversation as a
    /// User message. Does NOT start a turn — the model picks it up on the
    /// next real message. No approval (user-initiated, mirrors a terminal).
    pub(crate) async fn handle_local_shell(&mut self, cmd: String) {
        let cmd = cmd.trim().to_string();
        if cmd.is_empty() {
            return;
        }

        let wd = self.turn_runner.context.working_dir.read().await.clone();
        let call_id = format!(
            "local-shell-{}",
            LOCAL_SHELL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );

        let _ = self.event_tx.send(crate::agent::AgentEvent::ToolCallStarted {
            id: call_id.clone(),
            name: "bash".to_string(),
            arguments: serde_json::json!({ "command": cmd }).to_string(),
        });

        let event_tx = self.event_tx.clone();
        let cid = call_id.clone();
        let start = std::time::Instant::now();
        let outcome = crate::tool::bash::run_shell(&cmd, &wd, 300, move |chunk| {
            let _ = event_tx.send(crate::agent::AgentEvent::ToolOutputChunk {
                call_id: cid.clone(),
                chunk: chunk.to_string(),
            });
        })
        .await;

        let success = matches!(
            outcome.exit,
            crate::tool::bash::ShellExit::Exited { success: true, .. }
        );

        let _ = self.event_tx.send(crate::agent::AgentEvent::ToolCallResult {
            call_id,
            name: "bash".to_string(),
            output: crate::agent::local_shell::format_bash_display(&outcome),
            success,
            duration: start.elapsed(),
        });

        // Bound the injected context the same way per-turn tool results are
        // bounded (ctx::truncate), so `!cat bigfile` can't blow up the
        // conversation — the `!` path doesn't go through `truncate_output`.
        let char_limit = (self.ctx.ctx_window() / 8).min(32_000).max(8_000);
        let context_text =
            crate::agent::local_shell::format_bash_context(&cmd, &outcome, char_limit);
        self.conversation.add_user_message(&context_text);
        // Intentionally NOT calling run_turn_loop(): `!` records context
        // silently; the model reads it on the user's next message.
    }

    pub(crate) async fn change_dir(&mut self, path: &str) {
        let new_path = if path.starts_with('/') {
            std::path::PathBuf::from(path)
        } else if path.starts_with('~') {
            crate::tool::real_home_dir()
                .map(|h| h.join(path.strip_prefix("~/").unwrap_or(&path[1..])))
                .unwrap_or_else(|| std::path::PathBuf::from(path))
        } else {
            let wd: PathBuf = self
                .turn_runner
                .context
                .working_dir
                .try_read()
                .map(|g| g.clone())
                .unwrap_or_default();
            wd.join(path)
        };

        let resolved = std::fs::canonicalize(&new_path).unwrap_or(new_path);
        if resolved.is_dir() {
            {
                let mut wd = self.turn_runner.context.working_dir.write().await;
                *wd = resolved.clone();
            }
            self.datalog.set_working_dir(&resolved);
            // Clear conversation history — old paths from previous directory will confuse the model
            self.conversation.messages.clear();
            self.conversation.turn_tracker = crate::conversation::turn::TurnTracker::new();
            self.session_files.clear();
            // Explicit /cd is a deliberate context switch (cwd, git snapshot,
            // skills all change below) → rebuild the frozen system prompt.
            self.cached_system_prompt = None;
            // Refresh env snapshot for the new directory. The old git
            // branch / status belongs to the previous repo; keeping it
            // would lie to the model.
            self.env_snapshot = crate::ctx::EnvSnapshot::capture(&resolved);
            // Reload skills for the new working directory (project-level skills may differ)
            if let Ok(mut reg) = self.skill_registry.write() {
                // Non-interactive context: warnings would have nowhere to
                // render. Drop them; the TUI bootstrap reloads with a
                // renderer in scope and will surface anything important.
                let _ = reg.reload(&resolved);
            }
            // Reload code graph for the new project
            let graph_path = resolved.join(".atomcode").join("graph.bin");
            let new_graph = crate::graph::persist::load(&graph_path);
            // Swap graph data (reuse the same Arc, just replace contents)
            {
                let mut g = self.turn_runner.context.graph.write().await;
                *g = new_graph;
            }
            // Cancel the previous indexer so rapid `/cd` chains don't
            // stack parallel parses. Replace the token so the new spawn
            // below gets a fresh one; the old spawn cooperatively
            // exits at its next cancel check.
            self.indexer_cancel.cancel();
            self.indexer_cancel = CancellationToken::new();

            // Spawn new indexer — but only if the new dir is a real
            // project root. `/cd ~/project` (umbrella of many repos)
            // and `/cd ~` without markers would otherwise trigger a
            // multi-MB tree-sitter walk pegging CPU for minutes.
            // `should_index` covers $HOME / `/` / umbrella cases.
            if crate::graph::indexer::should_index(&resolved) {
                let graph_clone = self.turn_runner.context.graph.clone();
                let wd_for_indexer = resolved.clone();
                let cancel = self.indexer_cancel.clone();
                tokio::spawn(async move {
                    let mut indexer = crate::graph::indexer::GraphIndexer::new(
                        graph_clone.clone(),
                        wd_for_indexer.clone(),
                    );
                    indexer.index_all(cancel).await;
                    let gp = wd_for_indexer.join(".atomcode").join("graph.bin");
                    if let Ok(g) = graph_clone.try_read() {
                        let _ = crate::graph::persist::save(&g, &gp);
                    }
                });
            } else {
                let _ = self.event_tx.send(AgentEvent::TextDelta(
                    "[skipped code graph index: directory has no project marker \
                     (.git / Cargo.toml / package.json / pyproject.toml / go.mod / \
                     pom.xml / build.gradle) and looks like a parent of multiple \
                     projects. `cd` into a specific project to enable symbol search.]\n"
                        .to_string(),
                ));
            }
            let _ = self.event_tx.send(AgentEvent::WorkingDirChanged(resolved));
        }
    }
}
