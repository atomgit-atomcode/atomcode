use super::*;

impl AgentLoop {
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
            // Refresh env snapshot for the new directory. The old git
            // branch / status belongs to the previous repo; keeping it
            // would lie to the model.
            self.env_snapshot = crate::ctx::EnvSnapshot::capture(&resolved);
            // Reload skills for the new working directory (project-level skills may differ)
            if let Ok(mut reg) = self.skill_registry.write() {
                reg.reload(&resolved);
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
