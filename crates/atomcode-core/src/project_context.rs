use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Descriptor files to include (filename, max_lines).
/// The model reads these directly — no interpretation by us.
/// Descriptor files: only build/dependency configs. No README (wastes tokens on prose).
const DESCRIPTORS: &[(&str, usize)] = &[
    ("package.json", 10),
    ("Cargo.toml", 10),
    ("pyproject.toml", 10),
    ("go.mod", 5),
    ("pom.xml", 10),
    ("build.gradle", 10),
    ("requirements.txt", 10),
    ("docker-compose.yml", 10),
    ("Makefile", 10),
];

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
pub fn build_project_context(dir: &Path) -> ProjectContext {
    let mut ctx = String::new();
    let mut included_files = HashSet::new();

    // 0. Tech stack summary — model sees this first, no need to explore marker files
    let tech_stack = detect_tech_stack(dir);
    if !tech_stack.is_empty() {
        ctx.push_str(&tech_stack);
        ctx.push('\n');
    }

    // 1. File tree (3 levels) with tree-sitter annotations
    // Each source file gets top-level symbol names so the model can navigate without grepping
    let mut searcher = crate::semantic::SemanticSearcher::new();
    ctx.push_str("Project files:\n");
    ctx.push_str(&scan_tree(dir, 0, 3, &mut searcher, dir));

    // 1.5. Deep scan for config files — these live in deep directories
    // (e.g., backend/src/main/java/.../SecurityConfig.java) but are critical for diagnosis.
    let config_summaries = scan_config_files(dir);
    if !config_summaries.is_empty() {
        ctx.push_str("\nKey config:\n");
        ctx.push_str(&config_summaries);
    }

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

/// Config file patterns that get key-line summaries in the file tree.
/// (name_pattern, is_prefix_match)
const CONFIG_PATTERNS: &[(&str, bool)] = &[
    ("SecurityConfig", true),     // Java Spring Security
    ("AuthConfig", true),         // Generic auth config
    ("CorsConfig", true),         // CORS config
    ("WebSecurityConfig", true),  // Spring Security variant
    ("application.properties", false),
    ("application.yml", false),
    ("application.yaml", false),
    (".env", false),
    ("vite.config", true),
    ("next.config", true),
    ("webpack.config", true),
    ("tsconfig", true),
    ("nginx.conf", false),
];

fn scan_tree(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    searcher: &mut crate::semantic::SemanticSearcher,
    _project_root: &Path,
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

    let mut out = String::new();
    for entry in &items {
        let name = entry.file_name().to_string_lossy().to_string();
        let indent = "  ".repeat(depth);
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            out.push_str(&format!("{}{}/\n", indent, name));
            out.push_str(&scan_tree(&entry.path(), depth + 1, max_depth, searcher, _project_root));
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
                        .filter(|s| !s.name.starts_with('<')) // skip <template>/<style> pseudo-symbols
                        .map(|s| s.name.as_str())
                        .take(5)
                        .collect();
                    if !sym_names.is_empty() {
                        out.push_str(&format!("{}{} ({}L): {}\n",
                            indent, name, line_count, sym_names.join(", ")));
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

/// Check if a file is a config file and extract key-line summary.
fn extract_config_summary(name: &str, path: &Path) -> Option<String> {
    let name_lower = name.to_lowercase();
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let is_config = CONFIG_PATTERNS.iter().any(|(pattern, is_prefix)| {
        let pat_lower = pattern.to_lowercase();
        if *is_prefix {
            stem.to_lowercase().contains(&pat_lower)
        } else {
            name_lower == pat_lower
        }
    });

    if !is_config {
        return None;
    }

    let content = std::fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let summary = match ext {
        // Java Security/Auth config: extract permission rules
        "java" => extract_java_security_summary(&content),
        // Properties files: extract key=value pairs
        "properties" => extract_properties_summary(&content),
        // YAML files: extract top-level key: value pairs
        "yml" | "yaml" => extract_yaml_summary(&content),
        // .env files: extract KEY=value pairs
        _ if name == ".env" || name.starts_with(".env") => extract_properties_summary(&content),
        // JS/TS config: extract key lines (proxy, port, etc.)
        "js" | "ts" | "mjs" => extract_js_config_summary(&content),
        // Default: first 5 non-comment lines
        _ => extract_generic_summary(&content),
    };

    if summary.trim().is_empty() {
        None
    } else {
        Some(summary)
    }
}

/// Extract permission rules from Java Security config.
fn extract_java_security_summary(content: &str) -> String {
    let mut rules = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        // Match: .requestMatchers("/api/...").permitAll()
        if trimmed.contains("requestMatchers") || trimmed.contains("antMatchers") {
            // Clean up and collect
            let clean = trimmed
                .trim_start_matches('.')
                .replace("  ", " ");
            rules.push(format!("[auth] {}", clean));
            if rules.len() >= 8 { break; }
        }
    }
    rules.join("\n")
}

/// Extract key=value from .properties or .env files.
fn extract_properties_summary(content: &str) -> String {
    let important_keys = [
        "server.port", "spring.datasource", "database", "port",
        "DB_", "DATABASE_URL", "API_KEY", "SECRET", "HOST",
        "VITE_", "NEXT_PUBLIC_",
    ];
    let mut lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        let is_important = important_keys.iter()
            .any(|k| trimmed.to_uppercase().contains(&k.to_uppercase()));
        if is_important {
            // Mask sensitive values partially
            lines.push(format!("[config] {}", trimmed));
            if lines.len() >= 8 { break; }
        }
    }
    lines.join("\n")
}

/// Extract top-level key: value from YAML.
fn extract_yaml_summary(content: &str) -> String {
    let mut lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // Top-level or important nested keys
        let is_top_level = !line.starts_with(' ') && trimmed.contains(':');
        let is_important = trimmed.contains("port") || trimmed.contains("datasource")
            || trimmed.contains("url:") || trimmed.contains("password")
            || trimmed.contains("username") || trimmed.contains("host");
        if is_top_level || is_important {
            lines.push(format!("[config] {}", trimmed));
            if lines.len() >= 8 { break; }
        }
    }
    lines.join("\n")
}

/// Extract key config lines from JS/TS config files.
fn extract_js_config_summary(content: &str) -> String {
    let mut lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.contains("proxy") || trimmed.contains("port")
            || trimmed.contains("target:") || trimmed.contains("rewrite")
            || trimmed.contains("server:") || trimmed.contains("base:")
        {
            lines.push(format!("[config] {}", trimmed));
            if lines.len() >= 6 { break; }
        }
    }
    lines.join("\n")
}

/// Generic: first N non-comment, non-empty lines.
fn extract_generic_summary(content: &str) -> String {
    content.lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with('#') && !t.starts_with("//")
        })
        .take(5)
        .map(|l| format!("[config] {}", l.trim()))
        .collect::<Vec<_>>()
        .join("\n")
}
