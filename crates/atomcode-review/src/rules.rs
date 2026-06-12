//! Language/file-type review rules: per-language checklists matched against the CHANGED
//! file names of a diff and rendered as a scoped "targeted review focus" section appended
//! to the system prompt.
//!
//! Built-in rule docs are compiled in from `rules/*.md` (the single source of truth);
//! `--rules-dir <dir>` overrides any rule by name at runtime (`<dir>/<name>.md`) for
//! hot-tuning without a rebuild.
//!
//! Scoping: each rule section names exactly the files it applies to, so a mixed-language
//! batch never cross-contaminates (a Go rule never gets applied to a SQL file).

use std::path::Path;

/// Ordered matcher table: FIRST match wins, so specific file names (build.gradle,
/// package.json, mapper xml) MUST precede the generic extension patterns (*.json, *.xml).
/// Patterns match the lowercased BASE NAME with `*` wildcards.
const MATCHERS: &[(&str, &str)] = &[
    // Specific file names (before generic extensions).
    ("build.gradle", "build_gradle"),
    ("pom.xml", "pom_xml"),
    ("package.json", "package_json"),
    ("requirements*.txt", "python_deps"),
    ("pyproject.toml", "python_deps"),
    ("setup.py", "python_deps"),
    ("setup.cfg", "python_deps"),
    ("pipfile", "python_deps"),
    ("readme*.md", "markdown"),
    ("*.md", "markdown"),
    ("*.markdown", "markdown"),
    // MyBatis mapper / dao xml (narrower than a generic *.xml).
    ("*mapper*.xml", "mapper_dao_xml"),
    ("*dao*.xml", "mapper_dao_xml"),
    // Languages.
    ("*.go", "go"),
    ("*.js", "ts"),
    ("*.jsx", "ts"),
    ("*.ts", "ts"),
    ("*.tsx", "ts"),
    ("*.mjs", "ts"),
    ("*.vue", "ts"),
    ("*.py", "python"),
    ("*.java", "java"),
    ("*.kt", "kotlin"),
    ("*.kts", "kotlin"),
    ("*.ets", "arkts"),
    ("*.sql", "sql"),
    ("*.sh", "shell"),
    ("*.bash", "shell"),
    ("*.c", "c"),
    ("*.h", "c"),
    ("*.cc", "cpp"),
    ("*.cpp", "cpp"),
    ("*.cxx", "cpp"),
    ("*.hpp", "cpp"),
    ("*.rs", "rust"),
    ("*.rb", "ruby"),
    ("*.php", "php"),
    // Generic config types (after the specific file names).
    ("*.properties", "properties"),
    ("*.yaml", "yaml"),
    ("*.yml", "yaml"),
    ("*.json", "json"),
    ("*.json5", "json"),
];

/// Built-in rule documents, compiled in. `--rules-dir` overrides by name at runtime.
const RULE_DOCS: &[(&str, &str)] = &[
    ("arkts", include_str!("../rules/arkts.md")),
    ("build_gradle", include_str!("../rules/build_gradle.md")),
    ("c", include_str!("../rules/c.md")),
    ("cpp", include_str!("../rules/cpp.md")),
    ("go", include_str!("../rules/go.md")),
    ("java", include_str!("../rules/java.md")),
    ("json", include_str!("../rules/json.md")),
    ("kotlin", include_str!("../rules/kotlin.md")),
    ("mapper_dao_xml", include_str!("../rules/mapper_dao_xml.md")),
    ("markdown", include_str!("../rules/markdown.md")),
    ("package_json", include_str!("../rules/package_json.md")),
    ("php", include_str!("../rules/php.md")),
    ("pom_xml", include_str!("../rules/pom_xml.md")),
    ("properties", include_str!("../rules/properties.md")),
    ("python", include_str!("../rules/python.md")),
    ("python_deps", include_str!("../rules/python_deps.md")),
    ("ruby", include_str!("../rules/ruby.md")),
    ("rust", include_str!("../rules/rust.md")),
    ("shell", include_str!("../rules/shell.md")),
    ("sql", include_str!("../rules/sql.md")),
    ("ts", include_str!("../rules/ts.md")),
    ("yaml", include_str!("../rules/yaml.md")),
];

/// Rule name for a changed file path, or `None` when no rule applies.
/// Matches the lowercased base name against the ordered table — first hit wins.
pub fn match_rule(path: &str) -> Option<&'static str> {
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    MATCHERS.iter().find(|(pat, _)| wildcard_match(pat, &base)).map(|(_, name)| *name)
}

/// Changed file paths of a unified diff, parsed from `+++ b/<path>` lines
/// (`/dev/null` = deletion, skipped). Source-agnostic: works for git/PR/stdin diffs.
pub fn changed_files_from_diff(diff: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("+++ ") else { continue };
        let path = rest.trim();
        if path == "/dev/null" {
            continue;
        }
        let path = path.strip_prefix("b/").unwrap_or(path);
        if !path.is_empty() && !files.iter().any(|f| f == path) {
            files.push(path.to_string());
        }
    }
    files
}

/// Render the scoped rules section for `files`. Files are grouped by matched rule; each
/// group names exactly the files it applies to and renders its doc ONCE. Returns an empty
/// string when nothing matches (the caller appends nothing).
pub fn render_rules_section(files: &[String], rules_dir: Option<&Path>) -> String {
    // rule name -> files, preserving first-appearance order.
    let mut order: Vec<&'static str> = Vec::new();
    let mut groups: std::collections::HashMap<&'static str, Vec<&str>> = std::collections::HashMap::new();
    for f in files {
        if let Some(rule) = match_rule(f) {
            if !groups.contains_key(rule) {
                order.push(rule);
            }
            groups.entry(rule).or_default().push(f.as_str());
        }
    }
    if order.is_empty() {
        return String::new();
    }
    order.sort_unstable();

    let mut out = String::from(
        "## Targeted review focus for this batch (each rule applies ONLY to its listed files)\n\n",
    );
    for rule in order {
        let doc = load_rule_doc(rule, rules_dir);
        let doc = doc.trim();
        if doc.is_empty() {
            continue;
        }
        out.push_str("### Applies to: ");
        out.push_str(&groups[rule].join(", "));
        out.push('\n');
        out.push_str(doc);
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

/// Rule doc by name: `--rules-dir/<name>.md` wins when present and readable, else the
/// compiled-in default.
fn load_rule_doc(name: &str, rules_dir: Option<&Path>) -> String {
    if let Some(dir) = rules_dir {
        if let Ok(s) = std::fs::read_to_string(dir.join(format!("{name}.md"))) {
            if !s.trim().is_empty() {
                return s;
            }
        }
    }
    RULE_DOCS.iter().find(|(n, _)| *n == name).map(|(_, d)| (*d).to_string()).unwrap_or_default()
}

/// Minimal `*` wildcard match (the only metachar our table uses).
fn wildcard_match(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // Anchored prefix.
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            // Anchored suffix.
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(pos) => rest = &rest[pos + part.len()..],
                None => return false,
            }
        }
    }
    // Pattern ended with `*` (last part empty) — anything left matches.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matcher_order_specific_before_generic() {
        assert_eq!(match_rule("web/package.json"), Some("package_json"), "specific beats *.json");
        assert_eq!(match_rule("conf/settings.json"), Some("json"));
        assert_eq!(match_rule("mapper/UserMapper.xml"), Some("mapper_dao_xml"), "case-insensitive base");
        assert_eq!(match_rule("requirements-dev.txt"), Some("python_deps"));
        assert_eq!(match_rule("internal/foo/bar.go"), Some("go"));
        assert_eq!(match_rule("assets/data.bin"), None);
    }

    #[test]
    fn files_parsed_from_diff_plus_lines() {
        let diff = "diff --git a/a.go b/a.go\n--- a/a.go\n+++ b/a.go\n@@ -1 +1 @@\n+x\n\
                    diff --git a/x.sql b/x.sql\n--- a/x.sql\n+++ b/x.sql\n@@ -1 +1 @@\n+y\n\
                    diff --git a/gone.py b/gone.py\n--- a/gone.py\n+++ /dev/null\n";
        assert_eq!(changed_files_from_diff(diff), vec!["a.go".to_string(), "x.sql".to_string()]);
    }

    #[test]
    fn render_groups_scope_files_and_renders_doc_once() {
        let files = vec!["svc/a.go".to_string(), "svc/b.go".to_string(), "db/schema.sql".to_string()];
        let s = render_rules_section(&files, None);
        assert!(s.contains("Applies to: svc/a.go, svc/b.go"), "same-language files grouped:\n{s}");
        assert!(s.contains("Applies to: db/schema.sql"));
        assert_eq!(s.matches("[Go]").count(), 1, "go doc rendered once");
        assert!(s.contains("[SQL]"));
        // Unmatched-only input renders nothing.
        assert_eq!(render_rules_section(&["a.bin".to_string()], None), "");
    }

    #[test]
    fn rules_dir_overrides_builtin() {
        let dir = std::env::temp_dir().join(format!("rules-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("go.md"), "[Go-TUNED] custom doc").unwrap();
        let s = render_rules_section(&["a.go".to_string()], Some(&dir));
        assert!(s.contains("[Go-TUNED]"), "dir override wins:\n{s}");
        let s2 = render_rules_section(&["x.sql".to_string()], Some(&dir));
        assert!(s2.contains("[SQL]"), "missing override falls back to built-in");
        std::fs::remove_dir_all(&dir).ok();
    }
}
