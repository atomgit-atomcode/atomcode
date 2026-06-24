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
        let skill_dir = self.skill_dir.to_string_lossy();

        // SINGLE left-to-right pass: each substitution's value is emitted literally and
        // never re-scanned — so an argument that itself contains `$1` is NOT re-expanded.
        let t = self.template.as_str();
        let mut result = String::with_capacity(t.len());
        let mut i = 0;
        while i < t.len() {
            let rest = &t[i..];
            if let Some((value, len)) = match_substitution(rest, &positional, arguments, session_id, skill_dir.as_ref()) {
                result.push_str(value);
                i += len;
            } else {
                let ch = rest.chars().next().unwrap();
                result.push(ch);
                i += ch.len_utf8();
            }
        }
        // A template with no `$ARGUMENTS` token at all still gets the full args appended.
        if !self.template.contains("$ARGUMENTS") && !arguments.trim().is_empty() {
            result = format!("{}\n\nARGUMENTS: {}", result.trim_end(), arguments);
        }
        expand_shell_injections(&result)
    }
}

/// Match a substitution token at the START of `rest`; returns `(replacement, consumed)`.
/// Longest-token-first; only DEFINED positional indices substitute (others stay literal,
/// matching production). `$N` consumes a maximal digit run (so `$10` ≠ `$1` + `0`).
fn match_substitution<'a>(
    rest: &str,
    positional: &[&'a str],
    arguments: &'a str,
    session_id: &'a str,
    skill_dir: &'a str,
) -> Option<(&'a str, usize)> {
    if let Some(after) = rest.strip_prefix("$ARGUMENTS[") {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && after[digits.len()..].starts_with(']') {
            if let Ok(n) = digits.parse::<usize>() {
                if n < positional.len() {
                    return Some((positional[n], "$ARGUMENTS[".len() + digits.len() + 1));
                }
            }
        }
        return None; // malformed / out-of-range → literal
    }
    if rest.starts_with("${CLAUDE_SESSION_ID}") {
        return Some((session_id, "${CLAUDE_SESSION_ID}".len()));
    }
    if rest.starts_with("${CLAUDE_SKILL_DIR}") {
        return Some((skill_dir, "${CLAUDE_SKILL_DIR}".len()));
    }
    if rest.starts_with("$ARGUMENTS") {
        return Some((arguments, "$ARGUMENTS".len()));
    }
    if let Some(after) = rest.strip_prefix('$') {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            if let Ok(n) = digits.parse::<usize>() {
                if n < positional.len() {
                    return Some((positional[n], 1 + digits.len()));
                }
            }
        }
    }
    None
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
    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd);
    // No console-window flash when run from a console-less daemon (mirrors core's
    // skill runner); no-op off Windows.
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
    // Strip a surrounding pair of double OR single quotes (production parity).
    s.trim().trim_matches('"').trim_matches('\'').to_string()
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
            // AgentSkills spec is space-delimited; also accept commas (Claude Code compat).
            fm.allowed_tools = v.split([' ', ',']).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }
    }
    (fm, body.to_string())
}

/// Locate the closing `---`. Returns `(offset_of_close_newline, bytes_to_skip)`.
fn find_frontmatter_close(after_open: &str) -> Option<(usize, usize)> {
    // Closing delimiter at EOF with no trailing newline (empty / minimal frontmatter).
    if after_open == "---" {
        return Some((0, 3));
    }
    if after_open == "---\r" {
        return Some((0, 4));
    }
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
    fn argument_containing_dollar_token_is_not_re_expanded() {
        // arg0 is the literal "$1"; the single pass must NOT re-expand it into arg1.
        let out = skill("a=$0 b=$1").expand("$1 V", "");
        assert!(out.starts_with("a=$1 b=V"), "{out}");
    }

    #[test]
    fn out_of_range_positional_stays_literal() {
        let out = skill("x=$5").expand("a b", "");
        assert!(out.starts_with("x=$5"), "undefined $5 stays literal: {out}");
    }

    #[test]
    fn frontmatter_single_quotes_and_space_tools() {
        let (fm, _) = parse_frontmatter("---\nname: 'my-skill'\nallowed-tools: read_file bash grep\n---\nbody\n");
        assert_eq!(fm.name.as_deref(), Some("my-skill"));
        assert_eq!(fm.allowed_tools, vec!["read_file".to_string(), "bash".to_string(), "grep".to_string()]);
    }

    #[test]
    fn frontmatter_close_at_eof() {
        let (fm, body) = parse_frontmatter("---\ndescription: x\n---");
        assert_eq!(fm.description, "x");
        assert_eq!(body, "");
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
