use std::path::Path;

/// Generate a concise project context by reading key files.
/// This gives the AI understanding of WHAT the project is, not just the file tree.
pub fn build_project_context(dir: &Path) -> String {
    let mut ctx = String::new();

    // 1. Directory tree (2 levels, skip noise)
    ctx.push_str("Files:\n");
    let tree = scan_tree(dir, 0, 2);
    ctx.push_str(&tree);

    // 2. Read key descriptor files to understand the project
    let key_files = [
        ("README.md", "Project description"),
        ("README", "Project description"),
        ("package.json", "Node.js project"),
        ("Cargo.toml", "Rust project"),
        ("pyproject.toml", "Python project"),
        ("requirements.txt", "Python dependencies"),
        ("go.mod", "Go project"),
        ("Makefile", "Build system"),
        ("docker-compose.yml", "Docker services"),
        ("docker-compose.yaml", "Docker services"),
        (".env.example", "Environment config"),
    ];

    for (filename, desc) in &key_files {
        let path = dir.join(filename);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                // Take first 30 lines or 2000 chars max
                let summary: String = content
                    .lines()
                    .take(30)
                    .collect::<Vec<_>>()
                    .join("\n");
                let summary = if summary.len() > 2000 {
                    summary.chars().take(2000).collect::<String>() + "..."
                } else {
                    summary
                };
                ctx.push_str(&format!("\n{}({}):\n{}\n", filename, desc, summary));
            }
        }
    }

    // 3. Detect project type — actionable rules
    let hints = detect_project_hints(dir);
    if !hints.is_empty() {
        ctx.push_str(&format!("\nProject rules: {}\n", hints));
    }

    // Keep total context under 4000 chars
    if ctx.len() > 4000 {
        ctx.truncate(4000);
        ctx.push_str("\n...(truncated)");
    }

    ctx
}

fn detect_project_hints(dir: &Path) -> String {
    let mut hints: Vec<String> = Vec::new();

    if dir.join("package.json").exists() {
        hints.push("Node.js — check scripts in package.json".into());
    }
    if dir.join("Cargo.toml").exists() {
        hints.push("Rust — `cargo run` or `cargo build`".into());
    }
    if dir.join("requirements.txt").exists() || dir.join("pyproject.toml").exists() {
        hints.push("Python — check entry point".into());
    }
    if dir.join("go.mod").exists() {
        hints.push("Go — `go run .`".into());
    }
    if dir.join("docker-compose.yml").exists() || dir.join("docker-compose.yaml").exists() {
        hints.push("Docker Compose available".into());
    }
    if dir.join("Makefile").exists() {
        hints.push("Makefile available".into());
    }
    if dir.join("start.sh").exists() {
        hints.push("Has start.sh: when user says 'start', run `nohup bash start.sh &` immediately".into());
    }

    for sub in ["frontend", "backend", "server", "client", "web", "api"] {
        let sub_dir = dir.join(sub);
        if sub_dir.is_dir() {
            if sub_dir.join("package.json").exists() {
                hints.push(format!("{}/: Node.js sub-project", sub));
            }
            if sub_dir.join("requirements.txt").exists() {
                hints.push(format!("{}/: Python sub-project", sub));
            }
        }
    }

    hints.join(". ")
}

fn scan_tree(dir: &Path, depth: usize, max_depth: usize) -> String {
    if depth > max_depth { return String::new(); }

    let skip = [
        "node_modules", ".git", "target", "__pycache__", ".next",
        "dist", "build", ".cache", "vendor", ".venv", "venv",
        ".idea", ".vscode", ".DS_Store", ".env",
    ];

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return String::new(),
    };

    let mut items: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| !skip.contains(&e.file_name().to_string_lossy().as_ref()))
        .collect();
    items.sort_by_key(|e| e.file_name());

    let mut out = String::new();
    for entry in &items {
        let name = entry.file_name().to_string_lossy().to_string();
        let indent = "  ".repeat(depth);
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            out.push_str(&format!("{}{}/\n", indent, name));
            out.push_str(&scan_tree(&entry.path(), depth + 1, max_depth));
        } else {
            out.push_str(&format!("{}{}\n", indent, name));
        }
    }
    out
}
