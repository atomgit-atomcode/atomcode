use super::*;

impl AgentLoop {
    // NOTE: the per-turn dynamic reminder mechanism (a string injected
    // before each LLM turn containing CURRENT TASK + prev edited files)
    // has been removed. The verbatim user task now rides on the cadence
    // reflection checkpoint instead — see
    // `agent::discipline::reflection_prompt`.

    pub(crate) fn build_system_prompt(&mut self) -> String {
        // Dynamic rules: select prompt sections based on task type.
        // If user has a custom system_prompt in config, use that instead (override).
        let rules = if let Some(custom) = self
            .config
            .providers
            .get(&self.config.default_provider)
            .and_then(|p| p.system_prompt.as_deref())
        {
            custom.to_string()
        } else {
            crate::config::prompt_sections::build_rules().to_string()
        };

        let wd: PathBuf = self
            .turn_runner
            .context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Load layered instructions (global → project → user)
        let instructions = crate::config::instructions::LayeredInstructions::load(&wd);
        let merged_instructions = instructions.merged();

        // Stable environment metadata (no date — changes every day, breaks cache)
        let shell = if cfg!(target_os = "windows") {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "bash".into())
        };
        let env_info = format!("Platform: {} | Shell: {}", std::env::consts::OS, shell,);

        // Identity: inject model name so the model correctly identifies itself.
        let model_display = self
            .config
            .providers
            .get(&self.config.default_provider)
            .map(|p| p.model.as_str())
            .unwrap_or("unknown");

        // Assemble prompt: identity + env → rules LAST (recency effect).
        let mut prompt = format!(
            "You are AtomCode. When asked who you are, say you are AtomCode (an AI coding agent by AtomGit) running the {} model. Never claim to be another product.\n\
             Working directory: {wd}\nAll file paths in tool calls must be absolute, resolved under {wd}. Verify file existence before editing.\n{env_info}\n",
            model_display, wd = wd.display(), env_info = env_info,
        );

        // Git commit attribution. Mirrors Claude Code's convention:
        // when the agent runs git commit on the user's behalf, append
        // a Co-Authored-By trailer naming AtomCode + the model so
        // history reflects which AI did the work. Hardcoded into the
        // prompt rather than enforced via a bash wrapper because:
        //
        // 1. wrapping `git commit` would also catch revert/amend/cherry-
        //    pick paths that the user may not want tagged;
        // 2. the LLM constructs commit messages anyway, so injecting at
        //    the prompt layer is sufficient and keeps the bash tool
        //    transparent;
        // 3. users who want a different attribution can override this
        //    with `[providers.<name>] system_prompt = "..."` since that
        //    short-circuits the entire prompt assembly above.
        //
        // The trailer is consistent with GitHub / GitLab co-author
        // convention (case-insensitive `co-authored-by:` recognised by
        // both for "Co-authored" attribution display).
        prompt.push_str(&format!(
            "\n=== GIT COMMITS ===\n\
             When you create a git commit on the user's behalf, end the commit \
             message with this trailer (preceded by a blank line):\n\
             \n\
             Co-Authored-By: AtomCode ({}) <noreply@atomgit.com>\n\
             \n\
             Use a HEREDOC for `git commit -m` so the trailer's blank line is \
             preserved verbatim. Skip this trailer for `git commit --amend` \
             and `git revert` (those operate on existing commits whose \
             attribution shouldn't change).\n",
            model_display
        ));

        // Opening files in the GUI is a user-visible side effect — a
        // browser window popping up uninvited is jarring. Tell the
        // model to ASK first rather than auto-open after every HTML
        // write. The `open_file` tool handles cross-platform dispatch
        // (open / xdg-open / start / wslview) and refuses cleanly on
        // SSH / CI / headless so the model never has to second-guess
        // whether a window will actually appear.
        prompt.push_str(
            "\n=== OPENING FILES (PREVIEW) ===\n\
             After you create or edit an HTML / PDF / image / SVG file, DO NOT \
             automatically open it in the user's browser or viewer. The file \
             existing on disk is enough — opening a window is a visible side \
             effect the user may not want.\n\
             \n\
             Ask first. Phrasing like \"Want me to open it for preview?\" is \
             plenty. Only call the `open_file` tool when:\n\
             - the user explicitly asks (\"preview it\", \"open in browser\", \
             \"show me\"), OR\n\
             - the user has just confirmed they want a preview after you asked.\n\
             \n\
             `open_file` handles the OS / WSL / SSH / CI dispatch itself — \
             prefer it over raw `bash open`, `bash xdg-open`, etc. so the \
             behaviour stays consistent and headless sessions refuse cleanly.\n",
        );

        // Layered instructions (global / project / user)
        if !merged_instructions.is_empty() {
            prompt.push_str(&format!("\n{}\n", merged_instructions));
        }

        // Persistent memory
        {
            use crate::config::memory::MemoryStore;
            let wd = self.turn_runner.context.working_dir.try_read()
                .map(|g| g.clone()).unwrap_or_default();
            let project_name = wd.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            let global = MemoryStore::global();
            let project = MemoryStore::project(&wd);
            let memory_block = MemoryStore::merged_for_prompt(&global, &project, &project_name);
            if !memory_block.is_empty() {
                prompt.push_str(&format!("\n{}\n", memory_block));
            }
        }

        // Available skills — inject skill descriptions so LLM knows what skills exist.
        // Only inject skills that allow model invocation (disable_model_invocation = false).
        if let Ok(registry) = self.skill_registry.read() {
            let skills: Vec<String> = registry
                .invocable_by_llm()
                .map(|s| {
                    let hint = s
                        .argument_hint
                        .as_ref()
                        .map(|h| format!(" {}", h))
                        .unwrap_or_default();
                    format!("- /{}{}: {}", s.name, hint, s.description)
                })
                .collect();
            if !skills.is_empty() {
                prompt.push_str("\n=== AVAILABLE SKILLS ===\n");
                prompt.push_str(
                    "Use the `use_skill` tool to invoke a skill when relevant to the task.\n",
                );
                prompt.push_str(&skills.join("\n"));
                prompt.push('\n');
            }
        }

        // Git snapshot (branch / HEAD / status) captured at session start.
        // Empty string when `wd` isn't a git repo — push is a no-op.
        // See `ctx::env` for the snapshot / disclaimer rationale.
        prompt.push_str(&self.env_snapshot.as_prompt_section());

        // Plan mode: inject planning-only instructions before rules.
        if self.plan_mode {
            prompt.push_str(
                "\n=== PLAN MODE (ACTIVE) ===\n\
                 You are in PLAN MODE. You can explore, read files, run commands, and analyze the codebase.\n\
                 You MUST NOT edit, create, or delete any files. Only read-only tools are available.\n\
                 Your job is to:\n\
                 1. Analyze the codebase and understand the current state\n\
                 2. Create a detailed implementation plan with specific files, functions, and changes\n\
                 3. Present the plan clearly so the user can review before executing\n\
                 Do NOT attempt to make any changes. Focus on analysis and planning only.\n\n"
            );
        }

        // RULES GO LAST — recency effect ensures the model remembers these
        // when it starts generating tool calls.
        prompt.push_str(&format!(
            "\n=== RULES (follow these strictly) ===\n{rules}\n"
        ));

        // Platform-specific rules — only injected on the target OS.
        let platform = crate::config::platform_rules();
        if !platform.is_empty() {
            prompt.push_str(platform);
            prompt.push('\n');
        }

        // NOTE: model-specific directives (CJK language lock for MiniMax/
        // Qwen/DeepSeek/Kimi, MiniMax thinking discipline) were here but
        // moved to `ctx::render::apply_model_directives`, invoked by each
        // CtxBuilder impl in `build_messages`. Keeping them out of this
        // function keeps agent::prompt free of `if model_id.contains(...)`
        // branches — per-model customization now lives in ctx.

        prompt
    }
}
