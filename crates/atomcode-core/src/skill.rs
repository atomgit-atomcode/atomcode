use std::collections::HashMap;
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
    match Command::new("sh").arg("-c").arg(cmd).output() {
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

    let close = after_open
        .find("\n---\n")
        .map(|p| (p, 5usize))
        .or_else(|| after_open.find("\n---\r\n").map(|p| (p, 6)));

    let (close_pos, skip) = match close {
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
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        anyhow::bail!(
            "skill name '{}' must contain only lowercase letters, digits, and hyphens",
            name
        );
    }
    if name.starts_with('-') || name.ends_with('-') {
        anyhow::bail!("skill name '{}' must not start or end with a hyphen", name);
    }
    if name.contains("--") {
        anyhow::bail!("skill name '{}' must not contain consecutive hyphens", name);
    }
    Ok(())
}

fn make_name(base: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(ns) => format!("{}:{}", ns, base),
        None => base.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Registry of loaded skills, indexed by name.
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
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
    ///   4. `{home}/.atomcode/skills/*/SKILL.md`     directory-style, atomcode native
    ///
    /// Project (working dir):
    ///   5. `.claude/commands/*.md`
    ///   6. `.atomcode/commands/*.md`
    ///   7. `.claude/skills/*/SKILL.md`
    ///   8. `.atomcode/skills/*/SKILL.md`
    ///
    /// Same-name skill from a `skills/` directory beats one from `commands/`
    /// at the same level because it is loaded after.
    ///
    /// Note: If ATOMCODE_HOME env var is set, it overrides the default home directory
    /// for atomcode-specific paths (.atomcode/commands and .atomcode/skills).
    /// Claude Code compat paths (.claude/*) always use the system home directory.
    pub fn reload(&mut self, working_dir: &Path) {
        self.skills.clear();

        // System home directory (for Claude Code compat paths)
        let system_home = dirs::home_dir();
        
        // AtomCode home directory (respects ATOMCODE_HOME env var)
        let atomcode_home: Option<PathBuf> = std::env::var("ATOMCODE_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| system_home.clone());

        // Load Claude Code compat paths from system home (always)
        if let Some(ref home) = system_home {
            self.load_flat_commands(&home.join(".claude").join("commands"), None);
            self.load_skills_dir(&home.join(".claude").join("skills"), None);
        }

        // Load atomcode native paths from ATOMCODE_HOME (or system home as fallback)
        if let Some(ref home) = atomcode_home {
            self.load_flat_commands(&home.join(".atomcode").join("commands"), None);
            self.load_skills_dir(&home.join(".atomcode").join("skills"), None);
        }

        // Project-level skills (always from working dir)
        self.load_flat_commands(&working_dir.join(".claude").join("commands"), None);
        self.load_flat_commands(&working_dir.join(".atomcode").join("commands"), None);
        self.load_skills_dir(&working_dir.join(".claude").join("skills"), None);
        self.load_skills_dir(&working_dir.join(".atomcode").join("skills"), None);
    }

    /// Register a pre-built skill directly (used by plugin system).
    pub fn register(&mut self, skill: Skill) {
        self.skills.insert(skill.name.clone(), skill);
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
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

    // -----------------------------------------------------------------------

    /// Load all `.md` files from a flat `commands/` directory.
    fn load_flat_commands(&mut self, dir: &Path, namespace: Option<&str>) {
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
                    eprintln!("[skill] skipping {}: {}", path.display(), e);
                }
            }
        }
    }

    /// Load directory-style skills from a `skills/` directory.
    /// Each subdirectory that contains a `SKILL.md` becomes one skill.
    fn load_skills_dir(&mut self, dir: &Path, namespace: Option<&str>) {
        if !dir.is_dir() {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let skill_dir = entry.path();
            if !skill_dir.is_dir() {
                continue;
            }
            let skill_md = skill_dir.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            match parse_skill_dir(&skill_dir, &skill_md, namespace) {
                Ok(skill) => {
                    self.skills.insert(skill.name.clone(), skill);
                }
                Err(e) => {
                    eprintln!("[skill] skipping {}: {}", skill_dir.display(), e);
                }
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
}
