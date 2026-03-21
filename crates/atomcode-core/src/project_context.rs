use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Descriptor files to include (filename, max_lines).
/// The model reads these directly — no interpretation by us.
const DESCRIPTORS: &[(&str, usize)] = &[
    ("README.md", 40),
    ("README", 40),
    ("Makefile", 50),
    ("package.json", 30),
    ("Cargo.toml", 20),
    ("pyproject.toml", 30),
    ("go.mod", 5),
    ("Gemfile", 15),
    ("docker-compose.yml", 30),
    ("docker-compose.yaml", 30),
    ("justfile", 50),
    ("Taskfile.yml", 40),
    (".env.example", 10),
];

use crate::tool::SKIP_DIRS;

/// Result of building project context: the context string and the set of included file paths.
pub struct ProjectContext {
    pub text: String,
    /// Absolute paths of descriptor files whose content is already in the context.
    pub included_files: HashSet<PathBuf>,
}

/// Build project context by scanning the tree and including raw descriptor file contents.
/// No hardcoded project-type detection — the model reads the files and figures it out.
pub fn build_project_context(dir: &Path) -> ProjectContext {
    let mut ctx = String::new();
    let mut included_files = HashSet::new();

    // 1. File tree (4 levels — deep enough for most projects)
    ctx.push_str("Project files:\n");
    ctx.push_str(&scan_tree(dir, 0, 4));

    // 2. Include raw content of descriptor files the model can read
    let mut included_names = Vec::new();
    for &(filename, max_lines) in DESCRIPTORS {
        let path = dir.join(filename);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let summary: String = content
                    .lines()
                    .take(max_lines)
                    .collect::<Vec<_>>()
                    .join("\n");
                let summary = if summary.len() > 2000 {
                    summary.chars().take(2000).collect::<String>() + "..."
                } else {
                    summary
                };
                ctx.push_str(&format!("\n[{}]\n{}\n", filename, summary));
                // Store the canonical absolute path for matching
                if let Ok(abs) = std::fs::canonicalize(&path) {
                    included_files.insert(abs);
                } else {
                    included_files.insert(path);
                }
                included_names.push(filename.to_string());
            }
        }
    }

    if !included_names.is_empty() {
        ctx.push_str(&format!(
            "\n(These files are already shown above — do NOT re-read them: {})\n",
            included_names.join(", ")
        ));
    }

    // 3. List executable/script files at root
    let executables = find_executables(dir);
    if !executables.is_empty() {
        ctx.push_str(&format!("\nExecutable files: {}\n", executables.join(", ")));
    }

    // Cap total size — generous limit for large projects
    if ctx.len() > 15000 {
        ctx.truncate(15000);
        ctx.push_str("\n...(truncated)");
    }

    ProjectContext {
        text: ctx,
        included_files,
    }
}

/// Find executable files and known script files at root level.
fn find_executables(dir: &Path) -> Vec<String> {
    let mut result = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return result,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if !path.is_file() { continue; }

        let known = name.ends_with(".sh")
            || name == "Procfile"
            || name == "Dockerfile";

        #[cfg(unix)]
        let is_exec = {
            use std::os::unix::fs::PermissionsExt;
            path.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        };
        #[cfg(not(unix))]
        let is_exec = false;

        if known || is_exec {
            result.push(name);
        }
    }
    result.sort();
    result
}

fn scan_tree(dir: &Path, depth: usize, max_depth: usize) -> String {
    if depth > max_depth { return String::new(); }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return String::new(),
    };

    let mut items: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
        .collect();
    items.sort_by_key(|e| e.file_name());

    let mut out = String::new();
    for entry in &items {
        let name = entry.file_name().to_string_lossy().to_string();
        let indent = "  ".repeat(depth);
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.push_str(&format!("{}{}/\n", indent, name));
            out.push_str(&scan_tree(&entry.path(), depth + 1, max_depth));
        } else {
            out.push_str(&format!("{}{}\n", indent, name));
        }
    }
    out
}
