use super::*;

impl AgentLoop {
    // NOTE: the per-turn dynamic reminder mechanism (a string injected
    // before each LLM turn containing CURRENT TASK + prev edited files)
    // has been removed. The verbatim user task now rides on the cadence
    // reflection checkpoint instead — see
    // `agent::discipline::reflection_prompt`.

    pub(crate) fn build_system_prompt(&mut self) -> String {
        // ── Session-level immutability (LLM prefix-cache stability) ──
        // The system prompt is messages[0]; a single byte change zeroes the
        // ENTIRE prefix cache for the whole conversation. Measured: ~20% of
        // long-session cache collapses started right here, because the prompt
        // was rebuilt every turn from live inputs that drift mid-session —
        // chiefly the working directory (rewritten on every model `cd`, see
        // tool/cd.rs + tool/bash.rs) and the plan-mode block, plus memory /
        // layered-instructions re-read from disk each turn.
        //
        // So build it ONCE per session and reuse the exact same bytes every
        // turn. The cache is invalidated (set to None) only at explicit,
        // user-initiated contract boundaries — plan-mode toggle, /clear,
        // config reload, explicit /cd — each rare and each a legitimate
        // one-time reset. The model's OWN `cd` tool does NOT invalidate:
        // live cwd still reaches the model through the cd/bash tool RESULTS,
        // so freezing the cwd line here never blinds it. See
        // `system_prompt_is_frozen_across_model_cwd_change`.
        if let Some(ref cached) = self.cached_system_prompt {
            return cached.clone();
        }
        let prompt = self.assemble_system_prompt();
        self.cached_system_prompt = Some(prompt.clone());
        prompt
    }

    /// Assemble the system prompt from scratch. Called by
    /// `build_system_prompt` only on a cold cache. Every mid-session-variable
    /// input it reads (cwd, plan_mode, on-disk memory/instructions, skills,
    /// hook extensions) is snapshotted HERE and frozen until the next
    /// explicit cache invalidation — that is the whole point.
    fn assemble_system_prompt(&self) -> String {
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

        // Stable environment metadata. The date is DAY-GRANULAR and snapshotted
        // here ONCE per session (assemble_system_prompt runs only on a cold cache
        // — see build_system_prompt), so it stays byte-identical across every turn
        // and does NOT trigger the per-turn prefix-cache collapse the old "no date"
        // note worried about. Residual cost: cross-day sessions don't share this
        // prefix (~one cold prefill on the first session of a new day) — negligible
        // vs the within-session reuse, which is preserved. Without a date anchor
        // the model falls back to its training-era year and builds e.g. "2025
        // latest news" web_search queries in 2026 (user-reported). A long session
        // crossing midnight keeps the start-of-session date until the next cache
        // invalidation (/clear, /cd, config reload, plan toggle); the YEAR — the
        // part that fixes the search bug — stays correct regardless. (opencode does
        // the same, day-granular, in its <env> block: session/system.ts.)
        let shell = if cfg!(target_os = "windows") {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "bash".into())
        };
        let today = chrono::Local::now().format("%Y-%m-%d (%A)");
        let env_info = format!(
            "Platform: {} | Shell: {} | Today's date: {}",
            std::env::consts::OS,
            shell,
            today,
        );

        // Identity: inject model name so the model correctly identifies itself.
        let model_display = self
            .config
            .providers
            .get(&self.config.default_provider)
            .map(|p| p.model.as_str())
            .unwrap_or("unknown");

        // AtomCode's own config home (~/.atomcode or $ATOMCODE_HOME). Injected
        // so the model writes skills / commands / memory / hook rules HERE
        // instead of defaulting to ~/.claude (which it learned from training
        // and which belongs to a different product). See the CONFIG line below.
        let config_dir = crate::config::Config::config_dir();

        // Assemble prompt: identity + env → rules LAST (recency effect).
        let mut prompt = format!(
            "You are AtomCode. When asked who you are, say you are AtomCode (an AI coding agent by AtomGit) running the {model} model. Never claim to be another product.\n\
             Working directory: {wd}\n\
             All file paths in tool calls must be absolute, resolved under {wd}. Verify file existence before editing.\n\
             SCOPE: stay inside {wd}. Do not read, write, scan, or `cd` into directories outside it — sibling projects, parent directories, or anywhere else on the machine — unless the user explicitly names a path outside it. The lone exception is AtomCode's own config dir below. Reaching into neighbouring directories on your own initiative is almost never what the user wants.\n\
             CONFIG: AtomCode's own config — skills, commands, memory, hook rules — lives in {config_dir} (global) and {wd}/.atomcode (project); read and write it there. NEVER create or edit files under ~/.claude: that directory belongs to a different product, not AtomCode.\n\
             {env_info}\n",
            model = model_display, wd = wd.display(), config_dir = config_dir.display(), env_info = env_info,
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
                    "This is the COMPLETE list of available skills. Skills NOT listed here do NOT exist — do NOT fabricate or assume skills that are not in this list.\n\
To MODIFY a skill, edit its SKILL.md file directly (e.g. .atomcode/skills/<name>/SKILL.md or ~/.atomcode/skills/<name>/SKILL.md), NOT the project instructions file.\n\
Use the `use_skill` tool to invoke a skill when relevant to the task.\n",
                );
                prompt.push_str(&skills.join("\n"));
                prompt.push('\n');
            }
        }

        // Git snapshot (branch / HEAD / status) captured at session start.
        // Empty string when `wd` isn't a git repo — push is a no-op.
        // See `ctx::env` for the snapshot / disclaimer rationale.
        prompt.push_str(&self.env_snapshot.as_prompt_section());

        // NOTE: the PLAN MODE block used to be injected into the system prompt
        // here. It was moved OUT — toggling plan mode mid-session rewrote
        // messages[0] and zeroed the ENTIRE prefix cache (~12% of system-prompt
        // cache breaks in the line-data). Plan mode is now announced ONCE via a
        // synthetic history message when it is toggled (see
        // `AgentCommand::SetPlanMode`), and enforced structurally by read-only
        // tool gating (`use_read_only`). The system prompt stays a session-level
        // constant. See `plan_mode_is_not_in_system_prompt`.

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

        // --- SystemPrompt hook extensions ---
        // SystemPromptHook 允许用户（通过脚本/webhook）在 system prompt 末尾注入
        // 自定义内容（如安全策略、命名约定等）。这些扩展由 HookEngine 统一收集。
        // 此方法为同步（fn build_system_prompt），依赖调用方在调用之前预先调用
        // refresh_hook_extensions() 收集并缓存扩展。
        if !self.cached_system_prompt_extensions.is_empty() {
            prompt.push_str("\n=== HOOK SYSTEM PROMPT EXTENSIONS ===\n");
            for ext in &self.cached_system_prompt_extensions {
                prompt.push_str(ext);
                prompt.push('\n');
            }
        }

        prompt
    }
}
