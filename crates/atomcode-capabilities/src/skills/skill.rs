//! A loaded skill: a markdown template with optional YAML-ish frontmatter, plus the
//! argument/variable substitution engine. Ported from production `skill.rs`.
//!
//! `expand` runs any `` !`command` `` blocks through a shell — skills are TRUSTED,
//! user-authored content (the same trust as a slash command the user installed), so this
//! is by design, not arbitrary remote code.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The template body (everything after the frontmatter block).
    pub template: String,
    /// Tools the specialization MAY auto-approve while this skill is active (metadata;
    /// the L1 capability does not enforce it — that's an L2 approval-policy concern).
    pub allowed_tools: Vec<String>,
    /// Directory containing the skill file (for `${CLAUDE_SKILL_DIR}`).
    pub skill_dir: PathBuf,
    pub source_path: PathBuf,
}

impl Skill {
    /// Substitute arguments + variables into the template:
    /// `$ARGUMENTS[N]` / `$N` (positional), `$ARGUMENTS` (all; appended if absent),
    /// `${CLAUDE_SESSION_ID}`, `${CLAUDE_SKILL_DIR}`, and `` !`cmd` `` (shell pre-exec).
    pub fn expand(&self, arguments: &str, session_id: &str) -> String {
        let positional: Vec<&str> = arguments.split_whitespace().collect();
        let mut result = self.template.clone();

        for (i, arg) in positional.iter().enumerate() {
            result = result.replace(&format!("$ARGUMENTS[{i}]"), arg);
        }
        for (i, arg) in positional.iter().enumerate() {
            result = replace_positional_short(&result, i, arg);
        }
        // Check the ORIGINAL template for $ARGUMENTS so $ARGUMENTS[N]-only templates are
        // treated as "handled" (no append); $N-only templates still get args appended.
        if self.template.contains("$ARGUMENTS") {
            result = result.replace("$ARGUMENTS", arguments);
        } else if !arguments.trim().is_empty() {
            result = format!("{}\n\nARGUMENTS: {}", result.trim_end(), arguments);
        }
        result = result.replace("${CLAUDE_SESSION_ID}", session_id);
        result = result.replace("${CLAUDE_SKILL_DIR}", &self.skill_dir.to_string_lossy());
        expand_shell_injections(&result)
    }
}

/// Replace `$N` only when not followed by another digit (so `$1` ≠ inside `$10`).
fn replace_positional_short(s: &str, n: usize, replacement: &str) -> String {
    let pattern = format!("${n}");
    let pat = pattern.as_bytes();
    let src = s.as_bytes();
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < src.len() {
        if src[i..].starts_with(pat) {
            let after = i + pat.len();
            let next_is_digit = src.get(after).map(u8::is_ascii_digit).unwrap_or(false);
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

/// Replace each `` !`cmd` `` with the command's trimmed stdout (sh -c). Stops on an
/// unclosed backtick.
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
            s.trim_end().to_string()
        }
        Err(e) => format!("[error: {e}]"),
    }
}

// ── Frontmatter + parsing ────────────────────────────────────────────────────

#[derive(Default)]
struct Frontmatter {
    name: Option<String>,
    description: String,
    allowed_tools: Vec<String>,
}

fn fm_value(s: &str) -> String {
    let t = s.trim();
    t.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(t).to_string()
}

/// Parse `---`-delimited frontmatter; returns `(Frontmatter, body)`. Absent/unclosed
/// frontmatter → empty frontmatter + the whole content as body.
fn parse_frontmatter(content: &str) -> (Frontmatter, String) {
    let mut fm = Frontmatter::default();
    if !content.starts_with("---\n") && !content.starts_with("---\r\n") {
        return (fm, content.to_string());
    }
    let after_open = &content[if content.starts_with("---\r\n") { 5 } else { 4 }..];
    let (close_pos, skip) = match find_frontmatter_close(after_open) {
        Some(x) => x,
        None => return (fm, content.to_string()),
    };
    let block = &after_open[..close_pos];
    let body = &after_open[close_pos + skip..];
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            fm.name = Some(fm_value(v));
        } else if let Some(v) = line.strip_prefix("description:") {
            fm.description = fm_value(v);
        } else if let Some(v) = line.strip_prefix("allowed-tools:") {
            fm.allowed_tools = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
    }
    (fm, body.to_string())
}

/// Locate the closing `---`. Returns `(offset_of_close_newline, bytes_to_skip)`.
fn find_frontmatter_close(after_open: &str) -> Option<(usize, usize)> {
    if after_open.starts_with("---\n") {
        return Some((0, 4)); // empty frontmatter
    }
    if after_open.starts_with("---\r\n") {
        return Some((0, 5));
    }
    if let Some(pos) = after_open.find("\n---\n") {
        return Some((pos, 5));
    }
    if let Some(pos) = after_open.find("\n---\r\n") {
        return Some((pos, 6));
    }
    if after_open.ends_with("\n---") {
        return Some((after_open.len() - 4, 4));
    }
    if after_open.ends_with("\n---\r") {
        return Some((after_open.len() - 5, 5));
    }
    None
}

fn first_paragraph(template: &str) -> String {
    template
        .split("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!("skill name '{name}' must be 1-64 characters"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '/') {
        return Err(format!("skill name '{name}' has invalid characters"));
    }
    if name.starts_with(['/', '-']) || name.ends_with(['/', '-']) || name.contains("//") || name.contains("--") {
        return Err(format!("skill name '{name}' has a bad slash/hyphen position"));
    }
    Ok(())
}

fn make_name(base: &str, namespace: Option<&str>) -> String {
    let norm = base.to_ascii_lowercase().replace('/', "-");
    match namespace {
        Some(ns) => format!("{}:{norm}", ns.to_ascii_lowercase()),
        None => norm,
    }
}

/// Parse a flat `name.md` skill (name = file stem unless overridden in frontmatter).
pub(crate) fn parse_skill_file(path: &Path, namespace: Option<&str>) -> Result<Skill, String> {
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let stem = path.file_stem().and_then(|s| s.to_str()).ok_or("invalid file name")?;
    build_skill(&content, stem, path.parent().unwrap_or(Path::new(".")), path, namespace)
}

/// Parse a directory-style `<dir>/SKILL.md` (name = directory name unless overridden).
pub(crate) fn parse_skill_dir(skill_dir: &Path, skill_md: &Path, namespace: Option<&str>) -> Result<Skill, String> {
    let content = std::fs::read_to_string(skill_md).map_err(|e| e.to_string())?;
    let dir_name = skill_dir.file_name().and_then(|s| s.to_str()).ok_or("invalid directory name")?;
    build_skill(&content, dir_name, skill_dir, skill_md, namespace)
}

fn build_skill(content: &str, default_name: &str, skill_dir: &Path, source: &Path, namespace: Option<&str>) -> Result<Skill, String> {
    let (fm, template) = parse_frontmatter(content);
    let base = fm.name.as_deref().unwrap_or(default_name);
    validate_skill_name(base)?;
    let name = make_name(base, namespace);
    let description = if fm.description.is_empty() { first_paragraph(&template) } else { fm.description };
    Ok(Skill {
        name,
        description,
        template,
        allowed_tools: fm.allowed_tools,
        skill_dir: skill_dir.to_path_buf(),
        source_path: source.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(template: &str) -> Skill {
        Skill {
            name: "t".into(),
            description: String::new(),
            template: template.into(),
            allowed_tools: vec![],
            skill_dir: PathBuf::from("/sk"),
            source_path: PathBuf::from("/sk/SKILL.md"),
        }
    }

    #[test]
    fn expand_arguments_full_and_positional() {
        // $ARGUMENTS = all args; positional $N / $ARGUMENTS[N] are 0-based ($0 = first).
        // A template WITHOUT $ARGUMENTS still gets the full args appended (production behavior).
        assert_eq!(skill("do $ARGUMENTS now").expand("a b c", ""), "do a b c now");
        assert_eq!(skill("first=$0 second=$1").expand("a b", "").lines().next().unwrap(), "first=a second=b");
        assert_eq!(skill("idx=$ARGUMENTS[1]").expand("a b", ""), "idx=b");
    }

    #[test]
    fn dollar_n_boundary() {
        // $1 (0-based → second arg) must not match inside $10 (eleventh arg).
        let out = skill("$1 and $10").expand("X Y Z Q R S T U V W K", "");
        assert!(out.starts_with("Y and K"), "{out}");
    }

    #[test]
    fn appends_args_when_no_arguments_token() {
        let out = skill("plain template").expand("hello world", "");
        assert!(out.contains("plain template"));
        assert!(out.contains("ARGUMENTS: hello world"), "{out}");
    }

    #[test]
    fn variable_substitution() {
        let out = skill("dir=${CLAUDE_SKILL_DIR} sid=${CLAUDE_SESSION_ID}").expand("", "sess-1");
        assert_eq!(out, "dir=/sk sid=sess-1");
    }

    #[test]
    fn shell_injection_runs() {
        let out = skill("value=!`echo hi`").expand("", "");
        assert_eq!(out, "value=hi");
    }

    #[test]
    fn frontmatter_parse() {
        let (fm, body) = parse_frontmatter("---\nname: my-skill\ndescription: \"does X\"\nallowed-tools: read_file, bash\n---\nbody here\n");
        assert_eq!(fm.name.as_deref(), Some("my-skill"));
        assert_eq!(fm.description, "does X");
        assert_eq!(fm.allowed_tools, vec!["read_file".to_string(), "bash".to_string()]);
        assert_eq!(body.trim(), "body here");
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let (fm, body) = parse_frontmatter("just a template\nmore");
        assert!(fm.name.is_none());
        assert_eq!(body, "just a template\nmore");
    }

    #[test]
    fn name_validation() {
        assert!(validate_skill_name("good-name_1").is_ok());
        assert!(validate_skill_name("").is_err());
        assert!(validate_skill_name("-bad").is_err());
        assert!(validate_skill_name("a--b").is_err());
        assert!(validate_skill_name("has space").is_err());
        assert_eq!(make_name("My/Skill", Some("Plug")), "plug:my-skill");
    }
}
