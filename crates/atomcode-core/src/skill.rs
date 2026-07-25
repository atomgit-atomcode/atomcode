use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A loaded skill parsed from a `SKILL.md` or legacy `.md` file.
#[derive(Debug, Clone)]
pub struct Skill {
    /// Command name without leading slash, e.g. "commit" or "superpowers:brainstorming".
    pub name: String,
    /// Human-readable description (frontmatter > first paragraph of template).
    pub description: String,
    /// Raw template content (everything after the frontmatter block).
    pub template: String,
    /// If true, hidden from Claude's context — user must invoke manually via `/name`.
    pub disable_model_invocation: bool,
    /// If false, hidden from the `/` menu — Claude can still invoke automatically.
    pub user_invocable: bool,
    /// Autocomplete hint shown next to the skill name, e.g. "[issue-number]".
    pub argument_hint: Option<String>,
    /// Tools auto-approved when this skill is active.
    pub allowed_tools: Vec<String>,
    /// Directory containing the skill file (used for `${CLAUDE_SKILL_DIR}` substitution).
    pub skill_dir: PathBuf,
    /// Source file path, for diagnostics.
    pub source_path: PathBuf,
}

impl Skill {
    /// Expand the template, applying all substitutions in order:
    ///
    /// 1. `$ARGUMENTS[N]` → positional argument by 0-based index
    /// 2. `$N`            → shorthand for `$ARGUMENTS[N]`
    /// 3. `$ARGUMENTS`    → all arguments (appended as `ARGUMENTS: …` if absent)
    /// 4. `${CLAUDE_SESSION_ID}` → the provided session id
    /// 5. `${CLAUDE_SKILL_DIR}`  → absolute path of the skill's directory
    /// 6. `` !`command` ``       → preprocess: run shell command, insert stdout
    pub fn expand(&self, arguments: &str, session_id: &str) -> String {
        let positional: Vec<&str> = arguments.split_whitespace().collect();
        let mut result = self.template.clone();

        // 1. $ARGUMENTS[N]
        for (i, arg) in positional.iter().enumerate() {
            result = result.replace(&format!("$ARGUMENTS[{}]", i), arg);
        }

        // 2. $N shorthand — only when not followed by another digit
        for (i, arg) in positional.iter().enumerate() {
            result = replace_positional_short(&result, i, arg);
        }

        // 3. $ARGUMENTS
        // Check the ORIGINAL template, not `result`: $ARGUMENTS[N] starts with "$ARGUMENTS",
        // so this correctly treats positional-bracket templates as "handled" and avoids
        // the append fallback. Templates that use only $N shorthand (no $ARGUMENTS) still
        // get the full args appended so Claude can see them.
        if self.template.contains("$ARGUMENTS") {
            result = result.replace("$ARGUMENTS", arguments);
        } else if !arguments.trim().is_empty() {
            result = format!("{}\n\nARGUMENTS: {}", result.trim_end(), arguments);
        }

        // 4. ${CLAUDE_SESSION_ID}
        result = result.replace("${CLAUDE_SESSION_ID}", session_id);

        // 5. ${CLAUDE_SKILL_DIR}
        result = result.replace("${CLAUDE_SKILL_DIR}", &self.skill_dir.to_string_lossy());

        // 6. !`command` → shell pre-injection
        result = expand_shell_injections(&result);

        result
    }

    /// The expanded skill body PLUS a leading note telling the model where the
    /// skill's bundled files live. Use this at INJECTION sites (slash command /
    /// Skill tool); [`expand`](Self::expand) stays a pure template substitution.
    ///
    /// Without the note, a directory-style skill that references bundled resources
    /// with bare relative paths (`scripts/foo.py`, `references/bar.md`) makes the
    /// model resolve them against the CURRENT WORKING DIRECTORY — where they don't
    /// exist — so it reports the scripts "missing" and refuses to run (the reported
    /// bug). The note pins resolution to the skill's own directory instead.
    pub fn expand_for_injection(&self, arguments: &str, session_id: &str) -> String {
        let body = self.expand(arguments, session_id);
        match self.bundled_resource_note() {
            Some(note) => format!("{note}\n\n{body}"),
            None => body,
        }
    }

    /// A `<system-reminder>` naming the skill's install directory, emitted only for
    /// directory-style skills (source file literally `SKILL.md`) — those own a
    /// dedicated folder that can bundle `scripts/` / `references/`. Single-file
    /// `.md` skills share a skills folder, so a "your files are here" note would
    /// point at the wrong (shared) directory and is omitted.
    fn bundled_resource_note(&self) -> Option<String> {
        if self.source_path.file_name().and_then(|n| n.to_str()) != Some("SKILL.md") {
            return None;
        }
        let dir = display_skill_dir(&self.skill_dir.to_string_lossy(), cfg!(windows));
        Some(format!(
            "<system-reminder>\n\
             This skill is installed at: {dir}\n\
             Any files it references (e.g. `scripts/…`, `references/…`, templates) live \
             UNDER that directory, NOT the current working directory. Resolve every \
             relative path in this skill against the skill directory above — for a \
             bundled script, run it by its absolute path under that directory. Do not \
             search the project / working directory for these files.\n\
             </system-reminder>"
        ))
    }
}

/// Format a skill directory for the base-dir note (see `bundled_resource_note`).
///
/// On Windows, convert `\` → `/` so the path works uniformly across `read_file`,
/// Python, AND Git Bash: a raw backslash path breaks when the model runs a bundled
/// script via bash (Git Bash treats `\U`/`\s` as escapes and eats them). A Windows
/// path separator is ALWAYS `\` and filenames cannot contain `\`, so the replace is
/// lossless. On Unix, `\` is a legal filename character (not a separator), so leave
/// the path untouched. Note-text only — never fed to any path/IO.
fn display_skill_dir(raw: &str, is_windows: bool) -> String {
    if is_windows {
        raw.replace('\\', "/")
    } else {
        raw.to_string()
    }
}

/// Replace `$N` (where N matches `n`) only when the character immediately after
/// is not a digit — so `$1` does not accidentally match inside `$10`.
fn replace_positional_short(s: &str, n: usize, replacement: &str) -> String {
    let pattern = format!("${}", n);
    let pat = pattern.as_bytes();
    let src = s.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;

    while i < src.len() {
        if src[i..].starts_with(pat) {
            let after = i + pat.len();
            let next_is_digit = src.get(after).map(|b| b.is_ascii_digit()).unwrap_or(false);
            if !next_is_digit {
                out.extend_from_slice(replacement.as_bytes());
                i += pat.len();
                continue;
            }
        }
        out.push(src[i]);
        i += 1;
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// Find all `` !`…` `` occurrences, execute them via `sh -c`, and substitute
/// their trimmed stdout in-place. Stops on unclosed backtick.
fn expand_shell_injections(template: &str) -> String {
    let mut result = template.to_string();

    loop {
        let Some(start) = result.find("!`") else {
            break;
        };
        let search_from = start + 2;
        let Some(rel_end) = result[search_from..].find('`') else {
            break; // unclosed — leave as-is
        };
        let end = search_from + rel_end;
        let cmd = result[search_from..end].to_string();
        let output = run_shell_command(&cmd);
        result = format!("{}{}{}", &result[..start], output, &result[end + 1..]);
    }

    result
}

fn run_shell_command(cmd: &str) -> String {
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd);
    #[cfg(unix)]
    crate::process_utils::apply_utf8_locale_env_sync(&mut command);
    crate::process_utils::suppress_console_window_sync(&mut command);
    match command.output() {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !stderr.trim().is_empty() {
                    s.push('\n');
                    s.push_str(stderr.trim());
                }
            }
            // Trim trailing whitespace so inline substitution looks clean
            s.trim_end().to_string()
        }
        Err(e) => format!("[error: {}]", e),
    }
}

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

struct Frontmatter {
    name: Option<String>,
    description: String,
    disable_model_invocation: bool,
    user_invocable: bool,
    argument_hint: Option<String>,
    allowed_tools: Vec<String>,
}

impl Frontmatter {
    fn default() -> Self {
        Self {
            name: None,
            description: String::new(),
            disable_model_invocation: false,
            user_invocable: true,
            argument_hint: None,
            allowed_tools: Vec::new(),
        }
    }
}

/// Parse YAML frontmatter and return `(Frontmatter, template_body)`.
///
/// Requires `---\n` as the very first line. Unclosed or absent frontmatter
/// returns defaults and treats the entire content as the template body.
fn parse_frontmatter(content: &str) -> (Frontmatter, String) {
    let mut fm = Frontmatter::default();

    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return (fm, content.to_string());
    }

    let after_open = &content[if content.starts_with("---\r\n") { 5 } else { 4 }..];

    let (close_pos, skip) = match find_frontmatter_close(after_open) {
        Some(v) => v,
        None => return (fm, content.to_string()),
    };

    let fm_text = &after_open[..close_pos];
    let template = after_open[close_pos + skip..].to_string();

    for line in fm_text.lines() {
        if let Some(val) = line.strip_prefix("name:") {
            let v = val.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                fm.name = Some(v.to_string());
            }
        } else if let Some(val) = line.strip_prefix("description:") {
            fm.description = val.trim().trim_matches('"').trim_matches('\'').to_string();
        } else if let Some(val) = line.strip_prefix("disable-model-invocation:") {
            fm.disable_model_invocation = val.trim() == "true";
        } else if let Some(val) = line.strip_prefix("user-invocable:") {
            fm.user_invocable = val.trim() != "false";
        } else if let Some(val) = line.strip_prefix("argument-hint:") {
            let v = val.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                fm.argument_hint = Some(v.to_string());
            }
        } else if let Some(val) = line.strip_prefix("allowed-tools:") {
            // AgentSkills spec: space-delimited. Also accept comma for Claude Code compat.
            fm.allowed_tools = val
                .split(|c| c == ' ' || c == ',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    (fm, template)
}

fn find_frontmatter_close(after_open: &str) -> Option<(usize, usize)> {
    if after_open == "---" {
        return Some((0, 3));
    }
    if after_open == "---\r" {
        return Some((0, 4));
    }
    if after_open.starts_with("---\n") {
        return Some((0, 4));
    }
    if after_open.starts_with("---\r\n") {
        return Some((0, 5));
    }

    after_open
        .find("\n---\n")
        .map(|p| (p, 5usize))
        .or_else(|| after_open.find("\n---\r\n").map(|p| (p, 6)))
        .or_else(|| {
            after_open
                .strip_suffix("\n---")
                .map(|_| (after_open.len() - 4, 4))
        })
        .or_else(|| {
            after_open
                .strip_suffix("\n---\r")
                .map(|_| (after_open.len() - 5, 5))
        })
}

/// Extract a description from the first non-empty paragraph of the template,
/// used as a fallback when `description` is absent in frontmatter.
fn first_paragraph(template: &str) -> String {
    template
        .lines()
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .unwrap_or("")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Skill parsers
// ---------------------------------------------------------------------------

/// Parse a legacy flat `.md` file: name = file stem.
fn parse_skill_file(path: &Path, namespace: Option<&str>) -> anyhow::Result<Skill> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("filename is not valid UTF-8"))?;

    validate_skill_name(stem)?;

    let content = std::fs::read_to_string(path)?;
    let (fm, template) = parse_frontmatter(&content);

    let base_name = fm.name.as_deref().unwrap_or(stem);
    let name = make_name(base_name, namespace);

    let description = if fm.description.is_empty() {
        first_paragraph(&template)
    } else {
        fm.description
    };

    Ok(Skill {
        name,
        description,
        template,
        disable_model_invocation: fm.disable_model_invocation,
        user_invocable: fm.user_invocable,
        argument_hint: fm.argument_hint,
        allowed_tools: fm.allowed_tools,
        skill_dir: path.parent().unwrap_or(Path::new(".")).to_path_buf(),
        source_path: path.to_path_buf(),
    })
}

/// Parse a directory-style skill: name = directory name (or frontmatter `name`).
/// The entry point file is `<skill_dir>/SKILL.md`.
fn parse_skill_dir(
    skill_dir: &Path,
    skill_md: &Path,
    namespace: Option<&str>,
) -> anyhow::Result<Skill> {
    let dir_name = skill_dir
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("directory name is not valid UTF-8"))?;

    let content = std::fs::read_to_string(skill_md)?;
    let (fm, template) = parse_frontmatter(&content);

    let base_name = fm.name.as_deref().unwrap_or(dir_name);
    validate_skill_name(base_name)?;
    let name = make_name(base_name, namespace);

    let description = if fm.description.is_empty() {
        first_paragraph(&template)
    } else {
        fm.description
    };

    Ok(Skill {
        name,
        description,
        template,
        disable_model_invocation: fm.disable_model_invocation,
        user_invocable: fm.user_invocable,
        argument_hint: fm.argument_hint,
        allowed_tools: fm.allowed_tools,
        skill_dir: skill_dir.to_path_buf(),
        source_path: skill_md.to_path_buf(),
    })
}

fn validate_skill_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.len() > 64 {
        anyhow::bail!("skill name '{}' must be 1-64 characters", name);
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphabetic() || c.is_ascii_digit() || c == '-' || c == '_' || c == '/')
    {
        anyhow::bail!(
            "skill name '{}' must contain only letters, digits, hyphens, slashes, and underscores",
            name
        );
    }
    if name.starts_with('/') || name.ends_with('/') {
        anyhow::bail!("skill name '{}' must not start or end with a slash", name);
    }
    if name.contains("//") {
        anyhow::bail!("skill name '{}' must not contain consecutive slashes", name);
    }
    if name.starts_with('-') || name.ends_with('-') {
        anyhow::bail!("skill name '{}' must not start or end with a hyphen", name);
    }
    if name.contains("--") {
        anyhow::bail!("skill name '{}' must not contain consecutive hyphens", name);
    }
    Ok(())
}

/// Normalize a skill name for internal storage: lowercase all letters and
/// replace `/` with `-` so that names like `ssh-dev-suite/long-task` become
/// `ssh-dev-suite-long-task`. This avoids ambiguity with the namespace
/// separator `:` and keeps the key filesystem-safe.
pub(crate) fn normalize_skill_name(name: &str) -> String {
    name.trim_start_matches('/')
        .trim_start_matches('\\')
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace('/', "-")
}

fn make_name(base: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("{}:{}", ns.to_ascii_lowercase(), normalize_skill_name(base)),
        None => normalize_skill_name(base),
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Registry of loaded skills, indexed by name.
///
/// `BTreeMap` (not `HashMap`) so iteration order is deterministic (sorted by
/// name). The skill list is injected into the system prompt; a stable order
/// keeps the prompt prefix byte-identical across process launches, which is
/// required for provider-side prompt-prefix caching to hit. Same rationale as
/// `ToolRegistry`. See `skill_iteration_is_deterministic_sorted_order`.
pub struct SkillRegistry {
    skills: BTreeMap<String, Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: BTreeMap::new(),
        }
    }

    /// Reload skills from all sources.
    ///
    /// Load order (later entries overwrite earlier ones — higher priority wins):
    ///
    /// Global (home dir or ATOMCODE_HOME):
    ///   1. `{home}/.claude/commands/*.md`          legacy flat, Claude Code compat
    ///   2. `{home}/.atomcode/commands/*.md`         legacy flat, atomcode native
    ///   3. `{home}/.claude/skills/*/SKILL.md`       directory-style, Claude Code compat
    ///   4. `{home}/.agents/skills/*/SKILL.md`       directory-style, cross-agent shared (opencode)
    ///   5. `{home}/.atomcode/skills/*/SKILL.md`     directory-style, atomcode native
    ///
    /// Project (working dir):
    ///   6. `.claude/commands/*.md`
    ///   7. `.atomcode/commands/*.md`
    ///   8. `.claude/skills/*/SKILL.md`
    ///   9. `.agents/skills/*/SKILL.md`              cross-agent shared (opencode)
    ///  10. `.atomcode/skills/*/SKILL.md`
    ///
    /// `.agents/skills` sits between `.claude` and `.atomcode` at each level, so a
    /// same-name atomcode-native skill overrides a shared one (files are never
    /// modified — precedence is just which one the registry keeps).
    ///
    /// Same-name skill from a `skills/` directory beats one from `commands/`
    /// at the same level because it is loaded after.
    ///
    /// Note: If ATOMCODE_HOME env var is set, it overrides the default home directory
    /// for atomcode-specific paths (.atomcode/commands and .atomcode/skills).
    /// Claude Code compat paths (.claude/*) always use the system home directory.
    /// Reload skills. Returns a list of "skipped" diagnostics (one per
    /// rejected skill on disk). Callers in interactive contexts (TUI) can
    /// surface these gated behind verbose mode; non-interactive callers
    /// (agent bootstrap, /cd) drop them.
    pub fn reload(&mut self, working_dir: &Path) -> Vec<String> {
        self.skills.clear();
        let mut warnings: Vec<String> = Vec::new();

        // System home directory (for Claude Code compat paths)
        let system_home = crate::process_utils::real_home_dir();

        // AtomCode config dir (respects ATOMCODE_HOME env var; defaults to
        // ~/.atomcode). This is the SAME root used by config.toml, history,
        // plugins/, etc. — see Config::config_dir() for the single source of
        // truth.
        let atomcode_config_dir = atomcode_config::config::Config::config_dir();

        // All "loose" skills (i.e. not loaded through a plugin manifest)
        // share the synthetic `skills:` namespace so they're visually
        // distinguishable from built-in slash commands in the `/` menu —
        // e.g. `/skills:brainstorming`. Plugin loaders (future) will pass
        // their own namespace derived from the plugin manifest, matching
        // Claude Code's `<plugin>:<skill>` convention (`superpowers:foo`).
        const LOOSE_NS: Option<&str> = Some("skills");

        // Load Claude Code compat paths from system home (always)
        if let Some(ref home) = system_home {
            self.load_flat_commands(
                &home.join(".claude").join("commands"),
                LOOSE_NS,
                &mut warnings,
            );
            self.load_skills_dir(
                &home.join(".claude").join("skills"),
                LOOSE_NS,
                &mut warnings,
            );
            // `~/.agents/skills` — cross-agent shared convention (opencode et al.).
            // Between `.claude` and the `.atomcode` native dir below so an
            // atomcode-native skill still wins a same-name collision.
            self.load_skills_dir(
                &home.join(".agents").join("skills"),
                LOOSE_NS,
                &mut warnings,
            );
        }

        // Load atomcode native paths from the unified config dir.
        self.load_flat_commands(
            &atomcode_config_dir.join("commands"),
            LOOSE_NS,
            &mut warnings,
        );
        self.load_skills_dir(&atomcode_config_dir.join("skills"), LOOSE_NS, &mut warnings);

        // Project-level skills (always from working dir)
        self.load_flat_commands(
            &working_dir.join(".claude").join("commands"),
            LOOSE_NS,
            &mut warnings,
        );
        self.load_flat_commands(
            &working_dir.join(".atomcode").join("commands"),
            LOOSE_NS,
            &mut warnings,
        );
        self.load_skills_dir(
            &working_dir.join(".claude").join("skills"),
            LOOSE_NS,
            &mut warnings,
        );
        // `.agents/skills` (project) — same shared convention, before `.atomcode` so native wins.
        self.load_skills_dir(
            &working_dir.join(".agents").join("skills"),
            LOOSE_NS,
            &mut warnings,
        );
        self.load_skills_dir(
            &working_dir.join(".atomcode").join("skills"),
            LOOSE_NS,
            &mut warnings,
        );

        // Plugin layer — installed plugins contribute namespaced skills.
        for assets in crate::plugin::loader::iter_installed_plugin_assets() {
            for skills_dir in assets.skills_dirs() {
                self.load_skills_dir(&skills_dir, Some(&assets.plugin), &mut warnings);
            }
        }
        warnings
    }

    /// Register a pre-built skill directly (used by plugin system).
    pub fn register(&mut self, skill: Skill) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Look up a skill by name. Falls back to a unique `*:name` suffix
    /// match when the exact name misses AND the request is unqualified —
    /// covers the case where a hook-injected workflow plan or other
    /// external material refers to a plugin skill by its bare name
    /// (`ascend-model-verification`) instead of the registered fully
    /// qualified key (`ascend-model-agent-plugin:ascend-model-verification`).
    ///
    /// The lookup normalizes the input (lowercase + `/` → `-`) to match
    /// the storage convention, so callers can pass names in any case.
    ///
    /// Discipline: returns `None` when more than one namespace would match
    /// the bare name. Silent-pick-the-first would mask real ambiguity (and
    /// the LLM would invoke the wrong plugin) — better to error out so the
    /// caller surfaces the candidates.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        let normalized = normalize_skill_name(name);
        if let Some(s) = self.skills.get(&normalized) {
            return Some(s);
        }
        // Only run the fallback when the request is unqualified. A
        // qualified-but-missing lookup (`foo:bar` typo'd as `foo:baz`)
        // should fail loudly, not silently rebind to some other plugin.
        if normalized.contains(':') {
            return None;
        }
        let suffix = format!(":{}", normalized);
        let mut hits = self.skills.iter().filter(|(k, _)| k.ends_with(&suffix));
        let first = hits.next()?;
        if hits.next().is_some() {
            // Ambiguous — the bare name maps to multiple plugins. Refuse.
            return None;
        }
        Some(first.1)
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// All skills, regardless of invocation flags.
    pub fn all(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }

    /// Skills visible in the `/` menu (user-invocable).
    pub fn user_invocable(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values().filter(|s| s.user_invocable)
    }

    /// Skills that Claude may invoke automatically.
    pub fn invocable_by_llm(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values().filter(|s| !s.disable_model_invocation)
    }

    /// Render the `=== AVAILABLE SKILLS ===` system-prompt section (budget-gated,
    /// source-ranked), or `None` when nothing is invocable. Verbatim-aligned twin of
    /// [`crate::skill_render`]'s capabilities counterpart. Only LLM-invocable skills
    /// are listed (respects `disable-model-invocation`).
    pub fn render_catalog(&self) -> Option<String> {
        let entries: Vec<crate::skill_render::CatalogEntry> = self
            .invocable_by_llm()
            .map(|s| crate::skill_render::CatalogEntry {
                name: s.name.clone(),
                hint: s.argument_hint.clone(),
                description: s.description.clone(),
                source_rank: crate::skill_render::source_rank(&s.source_path),
            })
            .collect();
        crate::skill_render::render_skill_catalog(&entries)
    }

    // -----------------------------------------------------------------------

    /// Load all `.md` files from a flat `commands/` directory.
    fn load_flat_commands(
        &mut self,
        dir: &Path,
        namespace: Option<&str>,
        warnings: &mut Vec<String>,
    ) {
        if !dir.is_dir() {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            match parse_skill_file(&path, namespace) {
                Ok(skill) => {
                    self.skills.insert(skill.name.clone(), skill);
                }
                Err(e) => {
                    warnings.push(format!("[skill] skipping {}: {}", path.display(), e));
                }
            }
        }
    }

    /// Load directory-style skills from a `skills/` directory.
    ///
    /// Two layouts are supported:
    ///
    /// 1. **AtomCode / parent-directory layout**: `dir/` contains subdirectories
    ///    each with a `SKILL.md` — e.g. `dir/brainstorming/SKILL.md`.
    /// 2. **Claude Code array layout**: `dir/` itself contains a `SKILL.md`,
    ///    meaning `dir` *is* the skill directory. This happens when `plugin.json`
    ///    declares `skills: ["./skills/foo"]` — each entry points directly to a
    ///    skill directory rather than to a parent of skill directories.
    ///
    /// Both layouts can coexist: if `dir/SKILL.md` exists it is loaded first,
    /// then any `dir/*/SKILL.md` subdirectory skills are loaded after (and win
    /// on name collision, matching the higher-priority-wins convention).
    pub(crate) fn load_skills_dir(
        &mut self,
        dir: &Path,
        namespace: Option<&str>,
        warnings: &mut Vec<String>,
    ) {
        if !dir.is_dir() {
            return;
        }
        // CC array / hybrid layout: the passed directory may itself be a skill.
        let self_md = dir.join("SKILL.md");
        if self_md.exists() {
            self.try_load_skill(dir, &self_md, namespace, warnings);
        }
        // AtomCode / parent-directory layout: scan subdirectories. A subdir
        // that contains a SKILL.md is a skill; one that does not is treated as
        // a grouping directory and recursed into, so nested skills such as
        // `skills/B-SKILLS/SUB-SKILL/SKILL.md` are still discovered.
        self.scan_skill_children(dir, namespace, warnings, 0);
    }

    /// Recursively scan the subdirectories of `dir` for skills. Each subdir
    /// holding a `SKILL.md` is loaded (and not descended into — its own
    /// children are skill resources, not sub-skills); a subdir without one is
    /// a grouping directory whose nested skills are discovered recursively.
    /// `depth` is bounded to guard against symlink cycles.
    fn scan_skill_children(
        &mut self,
        dir: &Path,
        namespace: Option<&str>,
        warnings: &mut Vec<String>,
        depth: usize,
    ) {
        const MAX_DEPTH: usize = 8;
        if depth > MAX_DEPTH {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let child = entry.path();
            if !child.is_dir() {
                continue;
            }
            let skill_md = child.join("SKILL.md");
            if skill_md.exists() {
                self.try_load_skill(&child, &skill_md, namespace, warnings);
            } else {
                self.scan_skill_children(&child, namespace, warnings, depth + 1);
            }
        }
    }

    /// Parse and register a single skill directory, recording a diagnostic on
    /// failure instead of aborting the surrounding scan.
    fn try_load_skill(
        &mut self,
        dir: &Path,
        skill_md: &Path,
        namespace: Option<&str>,
        warnings: &mut Vec<String>,
    ) {
        match parse_skill_dir(dir, skill_md, namespace) {
            Ok(skill) => {
                self.skills.insert(skill.name.clone(), skill);
            }
            Err(e) => {
                warnings.push(format!("[skill] skipping {}: {}", dir.display(), e));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_skill(template: &str) -> Skill {
        Skill {
            name: "test".into(),
            description: "".into(),
            template: template.into(),
            disable_model_invocation: false,
            user_invocable: true,
            argument_hint: None,
            allowed_tools: vec![],
            skill_dir: PathBuf::new(),
            source_path: PathBuf::new(),
        }
    }

    #[test]
    fn test_normalize_skill_name_trimming() {
        assert_eq!(
            normalize_skill_name("/skills:gitcode-issue"),
            "skills:gitcode-issue"
        );
        assert_eq!(
            normalize_skill_name("-skills:gitcode-issue"),
            "skills:gitcode-issue"
        );
        assert_eq!(
            normalize_skill_name("\\skills:gitcode-issue"),
            "skills:gitcode-issue"
        );
        assert_eq!(normalize_skill_name("gitcode-issue"), "gitcode-issue");
    }

    // --- expand: $ARGUMENTS ---

    #[test]
    fn test_expand_with_arguments() {
        let s = make_skill("Do $ARGUMENTS please.");
        assert_eq!(s.expand("foo bar", ""), "Do foo bar please.");
    }

    #[test]
    fn test_expand_no_placeholder_with_args() {
        let s = make_skill("Do something.");
        assert_eq!(s.expand("extra", ""), "Do something.\n\nARGUMENTS: extra");
    }

    #[test]
    fn test_expand_no_placeholder_no_args() {
        let s = make_skill("Do something.");
        assert_eq!(s.expand("", ""), "Do something.");
    }

    // --- expand: $ARGUMENTS[N] and $N ---

    #[test]
    fn test_expand_positional_brackets() {
        // $ARGUMENTS[N] starts with "$ARGUMENTS" → treated as handled, no append
        let s = make_skill("Migrate $ARGUMENTS[0] from $ARGUMENTS[1] to $ARGUMENTS[2].");
        assert_eq!(
            s.expand("Button React Vue", ""),
            "Migrate Button from React to Vue."
        );
    }

    #[test]
    fn test_expand_positional_short() {
        // $N shorthand: template has no "$ARGUMENTS" literal → full args appended
        let s = make_skill("Migrate $0 from $1 to $2.");
        assert_eq!(
            s.expand("Button React Vue", ""),
            "Migrate Button from React to Vue.\n\nARGUMENTS: Button React Vue"
        );
    }

    #[test]
    fn test_expand_positional_short_no_partial_match() {
        // $1 must not eat the '0' from $10; no "$ARGUMENTS" → args appended
        let s = make_skill("a=$10 b=$1.");
        assert_eq!(s.expand("x y", ""), "a=$10 b=y.\n\nARGUMENTS: x y");
    }

    #[test]
    fn test_expand_session_id() {
        let s = make_skill("session=${CLAUDE_SESSION_ID}");
        assert_eq!(s.expand("", "abc-123"), "session=abc-123");
    }

    #[test]
    fn test_expand_skill_dir() {
        let mut s = make_skill("dir=${CLAUDE_SKILL_DIR}");
        s.skill_dir = PathBuf::from("/home/user/.claude/skills/my-skill");
        assert_eq!(s.expand("", ""), "dir=/home/user/.claude/skills/my-skill");
    }

    #[test]
    fn injection_prepends_skill_dir_note_for_dir_style() {
        // Directory-style skill (source is SKILL.md) → body gets a base-dir note so
        // bare `scripts/…` paths resolve against the skill dir, not the cwd.
        let mut s = make_skill("Run scripts/scan.py then read references/spec.md");
        s.skill_dir = PathBuf::from("/home/user/.claude/skills/wiki");
        s.source_path = s.skill_dir.join("SKILL.md");
        let out = s.expand_for_injection("", "");
        assert!(
            out.contains("/home/user/.claude/skills/wiki"),
            "note must name the skill dir"
        );
        assert!(out.contains("NOT the current working directory"));
        // Original body is preserved after the note.
        assert!(out
            .trim_end()
            .ends_with("Run scripts/scan.py then read references/spec.md"));
    }

    #[test]
    fn display_skill_dir_normalizes_only_on_windows() {
        // Windows: backslashes → forward slashes (works for read_file, python, and
        // Git Bash alike; a raw backslash path breaks bash script invocation).
        assert_eq!(
            display_skill_dir(r"C:\Users\admin\.claude\skills\wiki", true),
            "C:/Users/admin/.claude/skills/wiki"
        );
        // Unix: untouched — `\` is a legal filename char there, not a separator.
        assert_eq!(
            display_skill_dir("/home/u/.claude/skills/wiki", false),
            "/home/u/.claude/skills/wiki"
        );
        assert_eq!(display_skill_dir(r"weird\dir", false), r"weird\dir");
    }

    #[test]
    fn injection_no_note_for_single_file_skill() {
        // Single-file `.md` skill → skill_dir is a SHARED folder, so no note (it
        // would point at the wrong directory). Body is returned verbatim.
        let mut s = make_skill("Just a prompt.");
        s.skill_dir = PathBuf::from("/home/user/.claude/skills");
        s.source_path = s.skill_dir.join("my-skill.md");
        assert_eq!(s.expand_for_injection("", ""), "Just a prompt.");
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn shell_injection_preserves_utf8_paths_when_parent_locale_is_c() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().to_string_lossy().replace('\'', "'\\''");
        let command = format!(
            r#"
            cd '{}'
            mkdir -p "产品需求/流水线/帮助文档"
            printf 'line\n' > "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
            wc -l "产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"
        "#,
            cwd
        );
        let _guard = crate::process_utils::EnvVarGuard::new(&["LC_ALL", "LANG", "LC_CTYPE"]);
        std::env::set_var("LC_ALL", "C");
        std::env::set_var("LANG", "C");
        std::env::set_var("LC_CTYPE", "C");

        let output = run_shell_command(&command);

        assert!(
            output.contains("产品需求/流水线/帮助文档/GitCode-Action-官网文档.md"),
            "output was: {:?}",
            output
        );
    }

    // --- frontmatter ---

    #[test]
    fn test_frontmatter_none() {
        let (fm, tmpl) = parse_frontmatter("Just a template.");
        assert_eq!(fm.description, "");
        assert!(!fm.disable_model_invocation);
        assert!(fm.user_invocable);
        assert!(fm.name.is_none());
        assert_eq!(tmpl, "Just a template.");
    }

    #[test]
    fn test_frontmatter_full() {
        let content = "---\nname: my-skill\ndescription: \"My skill\"\ndisable-model-invocation: true\nuser-invocable: false\nargument-hint: \"[file]\"\nallowed-tools: Read Grep\n---\nBody.\n";
        let (fm, tmpl) = parse_frontmatter(content);
        assert_eq!(fm.name.as_deref(), Some("my-skill"));
        assert_eq!(fm.description, "My skill");
        assert!(fm.disable_model_invocation);
        assert!(!fm.user_invocable);
        assert_eq!(fm.argument_hint.as_deref(), Some("[file]"));
        assert_eq!(fm.allowed_tools, vec!["Read", "Grep"]);
        assert_eq!(tmpl, "Body.\n");
    }

    #[test]
    fn test_frontmatter_closing_delimiter_at_eof() {
        let content = "---\nname: eof-skill\ndescription: EOF skill\n---";
        let (fm, tmpl) = parse_frontmatter(content);
        assert_eq!(fm.name.as_deref(), Some("eof-skill"));
        assert_eq!(fm.description, "EOF skill");
        assert_eq!(tmpl, "");
    }

    #[test]
    fn test_empty_frontmatter_before_body() {
        let content = "---\n---\nBody.\n";
        let (fm, tmpl) = parse_frontmatter(content);
        assert_eq!(fm.description, "");
        assert_eq!(tmpl, "Body.\n");
    }

    #[test]
    fn test_frontmatter_unclosed() {
        let content = "---\ndescription: broken\nno closing delimiter";
        let (fm, tmpl) = parse_frontmatter(content);
        assert_eq!(fm.description, "");
        assert_eq!(tmpl, content);
    }

    #[test]
    fn test_description_fallback_to_first_paragraph() {
        // The fallback is tested via first_paragraph directly
        assert_eq!(
            first_paragraph("# Title\n\nActual description."),
            "Actual description."
        );
        assert_eq!(first_paragraph("  text  "), "text");
        assert_eq!(first_paragraph("# Heading"), ""); // heading skipped
    }

    // --- replace_positional_short ---

    #[test]
    fn test_replace_positional_short_boundary() {
        // $1 should not touch $10
        assert_eq!(replace_positional_short("$10 $1", 1, "Y"), "$10 Y");
    }

    // --- namespace prefix on disk-loaded skills ---

    #[test]
    fn test_load_skills_dir_applies_namespace() {
        // A skill loaded with namespace = Some("skills") must be stored
        // under the prefixed name `skills:<base>` and lookup by the bare
        // base name must miss. This pins the loader contract that the
        // TUI relies on for the visual `/skills:foo` distinction.
        let tmp = tempfile::tempdir().expect("tempdir");
        let skill_dir = tmp.path().join("brainstorming");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: \"Test\"\n---\nTemplate body.\n",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let mut warnings = Vec::new();
        reg.load_skills_dir(tmp.path(), Some("skills"), &mut warnings);

        assert!(
            reg.get("skills:brainstorming").is_some(),
            "namespaced lookup must succeed"
        );
        // Bare-name lookup falls back to a unique `:name` suffix match —
        // a deliberate accommodation for hook-injected workflow plans
        // that reference plugin skills without their plugin prefix.
        assert!(
            reg.get("brainstorming").is_some(),
            "bare name must resolve via suffix fallback when unambiguous"
        );
        // Verify storage key actually IS the prefixed form (the loader
        // contract; the fallback above could otherwise mask a regression
        // where the prefix wasn't applied).
        assert!(
            reg.skills.contains_key("skills:brainstorming"),
            "storage must use prefixed key"
        );
        assert!(
            !reg.skills.contains_key("brainstorming"),
            "storage must not duplicate under bare key"
        );
    }

    #[test]
    fn test_get_suffix_fallback_ambiguous_misses() {
        // When two plugins each contribute a skill named "verify", a bare
        // lookup must NOT silently pick one — that would invoke the wrong
        // plugin's tool. Caller must use the qualified form.
        let mut reg = SkillRegistry::new();
        for ns in ["plugin-a", "plugin-b"] {
            let key = format!("{}:verify", ns);
            reg.skills.insert(
                key.clone(),
                Skill {
                    name: key,
                    description: "v".into(),
                    template: "body".into(),
                    disable_model_invocation: false,
                    user_invocable: true,
                    argument_hint: None,
                    allowed_tools: vec![],
                    skill_dir: PathBuf::new(),
                    source_path: PathBuf::new(),
                },
            );
        }
        assert!(
            reg.get("verify").is_none(),
            "ambiguous bare name must miss (forces qualified lookup)"
        );
        assert!(reg.get("plugin-a:verify").is_some());
        assert!(reg.get("plugin-b:verify").is_some());
    }

    fn named_skill(name: &str) -> Skill {
        Skill {
            name: name.into(),
            description: "d".into(),
            template: "body".into(),
            disable_model_invocation: false,
            user_invocable: true,
            argument_hint: None,
            allowed_tools: vec![],
            skill_dir: PathBuf::new(),
            source_path: PathBuf::new(),
        }
    }

    /// Prompt-cache stability guard. The skill list is injected verbatim into
    /// the system prompt (agent/prompt.rs `=== AVAILABLE SKILLS ===`). If the
    /// registry iterates in a per-process-random order (the old `HashMap`
    /// behavior), the system prompt prefix differs byte-for-byte on every
    /// launch, so provider-side prompt-prefix caching misses on every fresh
    /// session — measured ~3% vs ~90%+ hit rate on a 78-skill setup. Iteration
    /// MUST be deterministic (sorted by name), matching `ToolRegistry`'s
    /// BTreeMap rationale.
    #[test]
    fn skill_iteration_is_deterministic_sorted_order() {
        let mut reg = SkillRegistry::new();
        // Insert in deliberately scrambled order.
        for name in [
            "superpowers:zeta",
            "skills:alpha",
            "plugin:mid",
            "skills:beta",
            "aaa:first",
        ] {
            reg.register(named_skill(name));
        }

        let got: Vec<&str> = reg.invocable_by_llm().map(|s| s.name.as_str()).collect();
        let mut want = got.clone();
        want.sort_unstable();
        assert_eq!(
            got, want,
            "invocable_by_llm must yield names in sorted order for prompt-cache stability"
        );

        let got_all: Vec<&str> = reg.all().map(|s| s.name.as_str()).collect();
        let mut want_all = got_all.clone();
        want_all.sort_unstable();
        assert_eq!(
            got_all, want_all,
            "all() iteration must be deterministic too"
        );
    }

    #[test]
    fn test_get_qualified_miss_does_not_fallback() {
        // A qualified-but-typo'd name must not silently rebind to a
        // suffix match — that would mask plugin name typos.
        let mut reg = SkillRegistry::new();
        reg.skills.insert(
            "real-plugin:verify".into(),
            Skill {
                name: "real-plugin:verify".into(),
                description: "v".into(),
                template: "body".into(),
                disable_model_invocation: false,
                user_invocable: true,
                argument_hint: None,
                allowed_tools: vec![],
                skill_dir: PathBuf::new(),
                source_path: PathBuf::new(),
            },
        );
        assert!(reg.get("typo-plugin:verify").is_none());
    }

    #[test]
    fn test_load_flat_commands_applies_namespace() {
        // Same contract for flat `.md` commands (legacy layout).
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("commit.md"),
            "---\ndescription: \"Commit\"\n---\nDo a commit.\n",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let mut warnings = Vec::new();
        reg.load_flat_commands(tmp.path(), Some("skills"), &mut warnings);

        assert!(reg.get("skills:commit").is_some());
        // Suffix fallback: unambiguous bare name resolves.
        assert!(reg.get("commit").is_some());
        assert!(reg.skills.contains_key("skills:commit"));
        assert!(!reg.skills.contains_key("commit"));
    }

    #[test]
    #[serial_test::serial]
    fn reload_picks_up_installed_plugin_skills() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", tmp.path());

        // Fake a registered + installed plugin on disk.
        // Under unified ATOMCODE_HOME semantics, plugins live at $HOME/plugins
        // (not $HOME/.atomcode/plugins) — see plugin/paths.rs.
        let plugins_root = tmp.path().join("plugins");
        let plugin_dir = plugins_root.join("marketplaces/p");
        let skill_dir = plugin_dir.join("skills/hello");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: hello\ndescription: hi\n---\nhi",
        )
        .unwrap();
        std::fs::write(
            plugins_root.join("installed_plugins.json"),
            r#"{"version":1,"plugins":{"p@p":{"marketplace":"p","plugin":"p","plugin_dir":"marketplaces/p","installed_at":"x"}}}"#,
        )
        .unwrap();

        let working = tempfile::tempdir().unwrap();
        let mut reg = SkillRegistry::new();
        reg.reload(working.path());
        assert!(
            reg.get("p:hello").is_some(),
            "expected namespaced plugin skill"
        );
    }

    #[test]
    fn reload_picks_up_agents_skills_project_level() {
        // A project sharing skills with opencode et al. via `.agents/skills/`.
        // (Unique name so the real ~/.claude|~/.agents|~/.atomcode can't collide.)
        let working = tempfile::tempdir().unwrap();
        let skill_dir = working
            .path()
            .join(".agents")
            .join("skills")
            .join("agents-shared-xyz");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: shared across agents\n---\nDo the shared thing.\n",
        )
        .unwrap();
        let mut reg = SkillRegistry::new();
        reg.reload(working.path());
        assert!(
            reg.get("skills:agents-shared-xyz").is_some() || reg.get("agents-shared-xyz").is_some(),
            "project-level .agents/skills must be discovered"
        );
    }

    /// Regression test: when `load_skills_dir` is given a directory that
    /// *directly* contains `SKILL.md` (CC array layout where the `skills`
    /// field points at the skill directory itself, not a parent), the skill
    /// must still be loaded.
    #[test]
    fn test_load_skills_dir_cc_array_layout() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Create a skill directory that contains SKILL.md directly
        // (CC-style: skills: ["./skills/karpathy-guidelines"])
        let skill_dir = tmp.path().join("skills/karpathy-guidelines");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: karpathy-guidelines\ndescription: Guidelines\n---\nBe simple.",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let mut warnings = Vec::new();
        // Pass the skill directory itself, not the parent "skills/" dir
        reg.load_skills_dir(&skill_dir, Some("karpathy-skills"), &mut warnings);

        assert!(
            reg.get("karpathy-skills:karpathy-guidelines").is_some(),
            "CC array layout: skill directory containing SKILL.md should be loaded"
        );
        assert!(warnings.is_empty(), "no warnings expected");
    }

    /// When a directory both contains SKILL.md itself and has subdirectories
    /// with SKILL.md, both are loaded (subdirectory skills win on collision).
    #[test]
    fn test_load_skills_dir_hybrid_layout() {
        let tmp = tempfile::tempdir().expect("tempdir");

        // Self-SKILL.md
        std::fs::write(
            tmp.path().join("SKILL.md"),
            "---\nname: hybrid\ndescription: self\n---\nself body",
        )
        .unwrap();

        // Subdirectory SKILL.md
        let sub = tmp.path().join("sub-skill");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("SKILL.md"),
            "---\nname: sub-skill\ndescription: sub\n---\nsub body",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let mut warnings = Vec::new();
        reg.load_skills_dir(tmp.path(), Some("test"), &mut warnings);

        assert!(
            reg.get("test:hybrid").is_some(),
            "self SKILL.md should load"
        );
        assert!(
            reg.get("test:sub-skill").is_some(),
            "subdirectory SKILL.md should load"
        );
    }

    /// A grouping directory without its own SKILL.md should still have its
    /// nested skills discovered (e.g. `skills/B-SKILLS/SUB-SKILL/SKILL.md`).
    /// Regression: previously only one level of subdirectory was scanned.
    #[test]
    fn test_load_skills_dir_nested_grouping() {
        let tmp = tempfile::tempdir().expect("tempdir");

        // Top-level skill (depth 1) — the case that already worked.
        let flat = tmp.path().join("a-skill");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::write(
            flat.join("SKILL.md"),
            "---\nname: a-skill\ndescription: flat\n---\nflat body",
        )
        .unwrap();

        // Grouping dir with NO SKILL.md of its own, holding a nested skill (depth 2).
        let nested = tmp.path().join("b-skills/sub-skill");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("SKILL.md"),
            "---\nname: sub-skill\ndescription: nested\n---\nnested body",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let mut warnings = Vec::new();
        reg.load_skills_dir(tmp.path(), Some("test"), &mut warnings);

        assert!(
            reg.get("test:a-skill").is_some(),
            "depth-1 skill should load"
        );
        assert!(
            reg.get("test:sub-skill").is_some(),
            "nested skill under a grouping directory should load"
        );
    }

    /// Regression test for the full plugin install path with CC-style
    /// `skills: ["./skills/karpathy-guidelines"]` in plugin.json.
    #[test]
    #[serial_test::serial]
    fn reload_picks_up_cc_array_plugin_skills() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", tmp.path());

        // Fake a plugin whose plugin.json uses CC array format.
        // Plugins live directly under ATOMCODE_HOME (unified semantics).
        let plugins_root = tmp.path().join("plugins");
        let plugin_dir = plugins_root.join("marketplaces/karpathy-skills");
        let skill_dir = plugin_dir.join("skills/karpathy-guidelines");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: karpathy-guidelines\ndescription: Guidelines\n---\nBe simple.",
        )
        .unwrap();
        // CC-style plugin.json with skills as an array of individual skill dirs
        std::fs::create_dir_all(plugin_dir.join(".claude-plugin")).unwrap();
        std::fs::write(
            plugin_dir.join(".claude-plugin/plugin.json"),
            r#"{"name":"andrej-karpathy-skills","skills":["./skills/karpathy-guidelines"]}"#,
        )
        .unwrap();
        std::fs::write(
            plugins_root.join("installed_plugins.json"),
            r#"{"version":1,"plugins":{"andrej-karpathy-skills@karpathy-skills":{"marketplace":"karpathy-skills","plugin":"andrej-karpathy-skills","plugin_dir":"marketplaces/karpathy-skills","installed_at":"x"}}}"#,
        )
        .unwrap();

        let working = tempfile::tempdir().unwrap();
        let mut reg = SkillRegistry::new();
        reg.reload(working.path());
        assert!(
            reg.get("andrej-karpathy-skills:karpathy-guidelines")
                .is_some(),
            "CC array plugin: skill should be loaded from direct skill directory"
        );
    }

    /// Regression test: skill names containing uppercase letters should be
    /// accepted and normalized to lowercase internally. This covers the bug
    /// where plugins with mixed-case skill directory names (e.g.
    /// `Ascend-Model-Verification`) caused `validate_skill_name` to reject
    /// them, making `/plugin reload` fail.
    #[test]
    fn test_uppercase_skill_name_accepted_and_normalized() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skill_dir = tmp.path().join("Ascend-Model-Verification");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: Ascend-Model-Verification\ndescription: Verify model\n---\nDo verify.",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let mut warnings = Vec::new();
        reg.load_skills_dir(tmp.path(), Some("MyPlugin"), &mut warnings);

        assert!(
            warnings.is_empty(),
            "uppercase skill name should not produce warnings: {:?}",
            warnings
        );
        // Internal storage key must be all-lowercase
        assert!(
            reg.get("myplugin:ascend-model-verification").is_some(),
            "skill should be stored under lowercased name"
        );
        // Suffix fallback also works with lowercase
        assert!(
            reg.get("ascend-model-verification").is_some(),
            "bare lowercased name should resolve via suffix fallback"
        );
    }

    /// When the frontmatter `name` field contains uppercase but the directory
    /// name is lowercase, the frontmatter name wins but is lowercased.
    #[test]
    fn test_frontmatter_uppercase_name_normalized() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: MyCustomSkill\ndescription: custom\n---\nBody.",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let mut warnings = Vec::new();
        reg.load_skills_dir(tmp.path(), Some("skills"), &mut warnings);

        assert!(warnings.is_empty(), "no warnings expected: {:?}", warnings);
        assert!(
            reg.get("skills:mycustomskill").is_some(),
            "frontmatter name with uppercase should be lowercased in storage"
        );
    }

    /// Flat .md file with uppercase stem should also be accepted and normalized.
    #[test]
    fn test_flat_command_uppercase_stem() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("MyCommand.md"),
            "---\ndescription: \"Upper cmd\"\n---\nDo stuff.\n",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let mut warnings = Vec::new();
        reg.load_flat_commands(tmp.path(), Some("skills"), &mut warnings);

        assert!(warnings.is_empty(), "no warnings expected: {:?}", warnings);
        assert!(
            reg.get("skills:mycommand").is_some(),
            "flat command with uppercase stem should be lowercased in storage"
        );
    }

    /// Regression test: skill names containing `/` (e.g. `ssh-dev-suite/long-task`)
    /// should be accepted and normalized so `/` becomes `-` in the internal key.
    /// This covers the bug where plugin frontmatter names with slash separators
    /// caused `validate_skill_name` to reject them.
    #[test]
    fn test_slash_in_skill_name_accepted_and_normalized() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let skill_dir = tmp.path().join("long-task");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: ssh-dev-suite/long-task\ndescription: Long task\n---\nDo task.",
        )
        .unwrap();

        let mut reg = SkillRegistry::new();
        let mut warnings = Vec::new();
        reg.load_skills_dir(tmp.path(), Some("MyPlugin"), &mut warnings);

        assert!(
            warnings.is_empty(),
            "slash in skill name should not produce warnings: {:?}",
            warnings
        );
        // `/` is normalized to `-` in the storage key
        assert!(
            reg.get("myplugin:ssh-dev-suite-long-task").is_some(),
            "skill with slash should be stored with slash replaced by hyphen"
        );
        // Suffix fallback works
        assert!(
            reg.get("ssh-dev-suite-long-task").is_some(),
            "bare normalized name should resolve via suffix fallback"
        );
    }

    #[test]
    fn test_normalize_skill_name() {
        assert_eq!(normalize_skill_name("Foo"), "foo");
        assert_eq!(
            normalize_skill_name("ssh-dev-suite/long-task"),
            "ssh-dev-suite-long-task"
        );
        assert_eq!(normalize_skill_name("MySkill/SubName"), "myskill-subname");
        assert_eq!(normalize_skill_name("hello"), "hello");
        assert_eq!(normalize_skill_name("A/B/C"), "a-b-c");
    }

    /// `SkillRegistry::get()` is case-insensitive and normalizes `/` → `-`
    /// so callers (LLM, TUI) can pass names in any case or with slashes.
    #[test]
    fn test_get_is_case_insensitive_and_normalizes_slash() {
        let mut reg = SkillRegistry::new();
        reg.skills.insert(
            "myplugin:ssh-dev-suite-long-task".into(),
            Skill {
                name: "myplugin:ssh-dev-suite-long-task".into(),
                description: "v".into(),
                template: "body".into(),
                disable_model_invocation: false,
                user_invocable: true,
                argument_hint: None,
                allowed_tools: vec![],
                skill_dir: PathBuf::new(),
                source_path: PathBuf::new(),
            },
        );

        // Case-insensitive exact match
        assert!(reg.get("MyPlugin:SSH-Dev-Suite-Long-Task").is_some());
        // Slash normalized to hyphen
        assert!(reg.get("ssh-dev-suite/long-task").is_some());
        // Case + slash combined
        assert!(reg.get("SSH-Dev-Suite/Long-Task").is_some());
        // Qualified name with slash
        assert!(reg.get("MyPlugin:ssh-dev-suite/long-task").is_some());
    }
}
