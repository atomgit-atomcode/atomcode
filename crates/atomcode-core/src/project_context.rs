use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Descriptor files to include in project context.
/// No per-file line limit — controlled by total token budget instead.
const DESCRIPTORS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "requirements.txt",
    "docker-compose.yml",
    "Makefile",
];

/// Total token budget for all descriptor files combined.
/// Each file gets up to ~200 tokens (~50 lines). Ensures model sees
/// dependency lists (e.g. [dependencies] in Cargo.toml) not just metadata.
const DESCRIPTOR_BUDGET_TOKENS: usize = 1200;

use crate::tool::SKIP_DIRS;

/// Result of building project context: the context string and the set of included file paths.
pub struct ProjectContext {
    pub text: String,
    /// Absolute paths of descriptor files whose content is already in the context.
    pub included_files: HashSet<PathBuf>,
}

/// Detect project tech stack from marker files in the working directory.
/// Scans root and common monorepo subdirectories for build configs and frameworks.
fn detect_tech_stack(working_dir: &Path) -> String {
    let mut stack: Vec<String> = Vec::new();

    let markers: &[(&str, &str)] = &[
        ("pom.xml", "Java/Maven"),
        ("build.gradle", "Java/Gradle"),
        ("build.gradle.kts", "Kotlin/Gradle"),
        ("Cargo.toml", "Rust/Cargo"),
        ("package.json", "Node.js"),
        ("go.mod", "Go"),
        ("requirements.txt", "Python"),
        ("pyproject.toml", "Python"),
        ("Gemfile", "Ruby"),
        ("composer.json", "PHP"),
        ("CMakeLists.txt", "C/C++/CMake"),
        ("Makefile", "Make"),
    ];

    // Root-level markers
    for &(file, label) in markers {
        if working_dir.join(file).exists() {
            stack.push(label.to_string());
        }
    }

    // Monorepo subdirectories
    for subdir in &["frontend", "backend", "server", "client", "web", "app"] {
        let sub = working_dir.join(subdir);
        if sub.is_dir() {
            for &(file, label) in markers {
                if sub.join(file).exists() {
                    stack.push(format!("{}/{}", subdir, label));
                    break;
                }
            }
        }
    }

    // Detect JS/TS frameworks from package.json
    let framework_markers: &[(&str, &str)] = &[
        ("\"vue\"", "Vue"),
        ("\"react\"", "React"),
        ("\"next\"", "Next.js"),
        ("\"nuxt\"", "Nuxt"),
        ("\"vite\"", "Vite"),
        ("\"tailwindcss\"", "Tailwind"),
        ("\"svelte\"", "Svelte"),
        ("\"angular\"", "Angular"),
    ];

    for pkg_path in &[
        working_dir.join("package.json"),
        working_dir.join("frontend").join("package.json"),
        working_dir.join("web").join("package.json"),
    ] {
        if pkg_path.exists() {
            if let Ok(content) = std::fs::read_to_string(pkg_path) {
                for &(needle, label) in framework_markers {
                    if content.contains(needle) {
                        stack.push(label.to_string());
                    }
                }
            }
        }
    }

    // Detect Spring Boot from pom.xml (root or backend/)
    for pom_path in &[
        working_dir.join("pom.xml"),
        working_dir.join("backend").join("pom.xml"),
    ] {
        if pom_path.exists() {
            if let Ok(content) = std::fs::read_to_string(pom_path) {
                if content.contains("spring-boot") {
                    stack.push("Spring Boot".to_string());
                }
            }
            break; // only check the first pom that exists
        }
    }

    // Deduplicate while preserving order
    let mut seen = HashSet::new();
    stack.retain(|item| seen.insert(item.clone()));

    if stack.is_empty() {
        String::new()
    } else {
        format!("Tech stack: {}\n", stack.join(", "))
    }
}

/// Build project context by scanning the tree and including raw descriptor file contents.
/// Build project context. `graph` is optional — when available, file tree
/// includes cross-file call relationships so the model can skip grepping.
pub fn build_project_context(dir: &Path) -> ProjectContext {
    build_project_context_with_graph(dir, None)
}

pub fn build_project_context_with_graph(dir: &Path, graph: Option<&crate::graph::CodeGraph>) -> ProjectContext {
    let mut ctx = String::new();
    let mut included_files = HashSet::new();

    // 0. Tech stack summary
    let tech_stack = detect_tech_stack(dir);
    if !tech_stack.is_empty() {
        ctx.push_str(&tech_stack);
        ctx.push('\n');
    }

    // 1. File tree (3 levels) with tree-sitter annotations + graph call relationships
    let mut searcher = crate::semantic::SemanticSearcher::new();
    ctx.push_str("Project files:\n");
    ctx.push_str(&scan_tree(dir, 0, 3, &mut searcher, dir, graph));

    // 1.5. Deep scan for config files — these live in deep directories
    // (e.g., backend/src/main/java/.../SecurityConfig.java) but are critical for diagnosis.
    let config_summaries = scan_config_files(dir);
    if !config_summaries.is_empty() {
        ctx.push_str("\nKey config:\n");
        ctx.push_str(&config_summaries);
    }

    // 2. Include raw content of descriptor files (token-budgeted)
    let mut included_names = Vec::new();
    let mut descriptor_tokens_used = 0usize;
    for &filename in DESCRIPTORS {
        if descriptor_tokens_used >= DESCRIPTOR_BUDGET_TOKENS { break; }
        let path = dir.join(filename);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let remaining = DESCRIPTOR_BUDGET_TOKENS - descriptor_tokens_used;
                // ~4 chars per token
                let max_chars = remaining * 4;
                let summary = if content.len() > max_chars {
                    let mut end = max_chars;
                    // Truncate at line boundary
                    if let Some(pos) = content[..end].rfind('\n') { end = pos; }
                    format!("{}...", &content[..end])
                } else {
                    content.clone()
                };
                let tokens = summary.len() / 4 + 4;
                descriptor_tokens_used += tokens;
                ctx.push_str(&format!("\n[{}]\n{}\n", filename, summary));
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

    // Cap total size — keep lean for faster inference
    if ctx.len() > 6000 {
        let mut end = 6000;
        while end > 0 && !ctx.is_char_boundary(end) {
            end -= 1;
        }
        ctx.truncate(end);
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

/// Source file extensions that get tree-sitter annotations in the file tree.
const ANNOTATE_EXTS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "vue", "svelte",
    "go", "java", "c", "cpp", "cc", "h", "hpp",
];

/// Config file extensions — any file with these extensions is a config file.
const CONFIG_EXTS: &[&str] = &[
    "properties", "ini", "toml", "conf", "cfg",
];

/// File name keywords — if the file stem contains any of these, it's a config file.
const CONFIG_NAME_KEYWORDS: &[&str] = &[
    "config", "settings", "security", "auth", "permission", "cors",
    "middleware", "routes", "urls",
];

/// Exact file names that are always config files.
const CONFIG_EXACT_NAMES: &[&str] = &[
    ".env", ".env.local", ".env.production", ".env.development",
    "nginx.conf", "Caddyfile",
];

/// Keywords in file content that indicate high-value config lines.
const HIGH_VALUE_KEYWORDS: &[&str] = &[
    "port", "host", "password", "passwd", "secret", "token",
    "database", "datasource", "db_", "redis",
    "url", "endpoint", "base_url", "api_key",
    "allow", "deny", "permit", "auth", "cors", "origin",
    "proxy", "target", "rewrite", "upstream",
    "debug", "log_level",
];

fn scan_tree(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    searcher: &mut crate::semantic::SemanticSearcher,
    _project_root: &Path,
    graph: Option<&crate::graph::CodeGraph>,
) -> String {
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

    let (dirs, files): (Vec<_>, Vec<_>) = items.iter()
        .partition(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false));
    let mut visible: Vec<&std::fs::DirEntry> = dirs.clone();
    let file_limit = 20;
    let truncated = files.len().saturating_sub(file_limit);
    visible.extend(files.iter().take(file_limit));
    visible.sort_by_key(|e| e.file_name());

    let mut out = String::new();
    for entry in &visible {
        let name = entry.file_name().to_string_lossy().to_string();
        let indent = "  ".repeat(depth);
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.push_str(&format!("{}{}/\n", indent, name));
            out.push_str(&scan_tree(&entry.path(), depth + 1, max_depth, searcher, _project_root, graph));
        } else {
            let entry_path = entry.path();

            // Config files: extract key-line summaries
            if let Some(summary) = extract_config_summary(&name, &entry_path) {
                out.push_str(&format!("{}{}\n", indent, name));
                for line in summary.lines() {
                    out.push_str(&format!("{}  {}\n", indent, line));
                }
                continue;
            }

            // Annotate source files with top-level symbol names (max 5)
            let ext = entry_path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if depth <= 2 && ANNOTATE_EXTS.contains(&ext) {
                let line_count = std::fs::read_to_string(entry.path())
                    .map(|c| c.lines().count())
                    .unwrap_or(0);
                if let Some(symbols) = searcher.list_symbols(&entry.path()) {
                    let sym_names: Vec<&str> = symbols.iter()
                        .filter(|s| !s.name.starts_with('<'))
                        .map(|s| s.name.as_str())
                        .take(5)
                        .collect();
                    if !sym_names.is_empty() {
                        out.push_str(&format!("{}{} ({}L): {}\n",
                            indent, name, line_count, sym_names.join(", ")));

                        // Graph: append cross-file call targets (max 3)
                        if let Some(g) = graph {
                            let callees = graph_file_callees(g, &entry_path);
                            if !callees.is_empty() {
                                out.push_str(&format!("{}  → {}\n", indent,
                                    callees.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
                            }
                        }
                        continue;
                    }
                }
                if line_count > 0 {
                    out.push_str(&format!("{}{} ({}L)\n", indent, name, line_count));
                } else {
                    out.push_str(&format!("{}{}\n", indent, name));
                }
            } else {
                out.push_str(&format!("{}{}\n", indent, name));
            }
        }
    }
    if truncated > 0 {
        let indent = "  ".repeat(depth);
        out.push_str(&format!("{}(... {} more files)\n", indent, truncated));
    }
    out
}

/// Deep scan for config files across the entire project (respects SKIP_DIRS).
/// Returns formatted summaries of all found config files.
fn scan_config_files(dir: &Path) -> String {
    let mut results = Vec::new();
    scan_config_recursive(dir, dir, 0, 8, &mut results);
    results.join("")
}

fn scan_config_recursive(
    dir: &Path,
    project_root: &Path,
    depth: usize,
    max_depth: usize,
    results: &mut Vec<String>,
) {
    if depth > max_depth || results.len() >= 10 { return; }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) { continue; }

        let path = entry.path();
        if path.is_dir() {
            scan_config_recursive(&path, project_root, depth + 1, max_depth, results);
        } else if let Some(summary) = extract_config_summary(&name, &path) {
            if !summary.trim().is_empty() {
                let rel = path.strip_prefix(project_root).unwrap_or(&path);
                results.push(format!("  [{}]\n", rel.display()));
                for line in summary.lines() {
                    results.push(format!("    {}\n", line));
                }
            }
        }
    }
}

/// Check if a file is a config file based on tech-stack-agnostic rules:
/// 1. Extension is a known config format (.properties, .ini, .toml, .conf, .cfg)
/// 2. File name stem contains config keywords (config, settings, security, auth, etc.)
/// 3. File is an exact match (.env, nginx.conf, etc.)
/// 4. File is YAML and lives in a config-like directory
fn is_config_file(name: &str, path: &Path) -> bool {
    let name_lower = name.to_lowercase();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    // Rule 1: Config extensions
    if CONFIG_EXTS.contains(&ext) {
        return true;
    }

    // Rule 2: Name contains config keywords
    if CONFIG_NAME_KEYWORDS.iter().any(|kw| stem.contains(kw)) {
        return true;
    }

    // Rule 3: Exact name match
    if CONFIG_EXACT_NAMES.iter().any(|n| name_lower == *n) {
        return true;
    }

    // Rule 4: YAML in config-like directories or with config-like names
    if matches!(ext, "yml" | "yaml") {
        let parent = path.parent().and_then(|p| p.file_name())
            .and_then(|n| n.to_str()).unwrap_or("");
        if parent == "resources" || parent == "config" || parent == "conf" {
            return true;
        }
    }

    false
}

/// Extract key-line summary from a config file.
/// Tech-stack agnostic: scans for HIGH_VALUE_KEYWORDS in content.
fn extract_config_summary(name: &str, path: &Path) -> Option<String> {
    if !is_config_file(name, path) {
        return None;
    }

    let content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }

    let mut lines: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Skip empty lines and comments (universal comment prefixes)
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
            || trimmed.starts_with("<!--")
        {
            continue;
        }

        let lower = trimmed.to_lowercase();

        // Include line if it contains any high-value keyword
        let is_high_value = HIGH_VALUE_KEYWORDS.iter().any(|kw| lower.contains(kw));

        if is_high_value {
            // Truncate very long lines (e.g., base64 encoded secrets)
            let display = if trimmed.len() > 120 {
                format!("{}...", &trimmed[..117])
            } else {
                trimmed.to_string()
            };
            lines.push(display);
            if lines.len() >= 10 { break; }
        }
    }

    if lines.is_empty() {
        return None;
    }

    Some(lines.join("\n"))
}

/// Get cross-file callee file names for a source file using the code graph.
/// Returns deduplicated set of file basenames that this file calls into.
fn graph_file_callees(graph: &crate::graph::CodeGraph, file_path: &Path) -> Vec<String> {
    let file_buf = file_path.to_path_buf();
    let symbol_ids = match graph.symbols_in_file(&file_buf) {
        Some(ids) => ids.clone(),
        None => return Vec::new(),
    };
    let mut callee_files: std::collections::HashSet<String> = std::collections::HashSet::new();

    for sid in &symbol_ids {
        for (callee_id, _depth) in graph.trace_callees(*sid, 1) {
            if let Some(callee) = graph.node(callee_id) {
                if callee.file != file_path {
                    if let Some(name) = callee.file.file_name() {
                        callee_files.insert(name.to_string_lossy().to_string());
                    }
                }
            }
        }
    }

    let mut result: Vec<String> = callee_files.into_iter().collect();
    result.sort();
    result
}
