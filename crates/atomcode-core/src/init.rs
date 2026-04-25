//! Project initialization — generate .atomcode.md from project structure.

use std::path::Path;

/// Generate project instruction content by scanning the working directory.
pub fn generate_project_instructions(working_dir: &Path) -> String {
    let mut sections = Vec::new();
    let project_name = working_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());

    sections.push(format!("# {} — Project Instructions\n", project_name));

    // Detect tech stack
    let (stack, build_cmds) = detect_project(working_dir);
    if !stack.is_empty() {
        sections.push(format!("**Tech Stack:** {}\n", stack.join(", ")));
    }

    if !build_cmds.is_empty() {
        sections.push("## Build & Test\n".to_string());
        for cmd in &build_cmds {
            sections.push(format!("- `{}`", cmd));
        }
        sections.push(String::new());
    }

    sections.push("## Code Style\n".to_string());
    sections.push("- Follow existing patterns in the codebase".to_string());
    sections.push("- Write tests for new functionality".to_string());

    // Add framework-specific hints
    for hint in framework_hints(working_dir) {
        sections.push(format!("- {}", hint));
    }

    sections.join("\n")
}

/// Detect project type and return (tech_stack, build_commands).
fn detect_project(dir: &Path) -> (Vec<String>, Vec<String>) {
    let mut stack = Vec::new();
    let mut cmds = Vec::new();

    if dir.join("Cargo.toml").exists() {
        stack.push("Rust".to_string());
        cmds.push("cargo build".to_string());
        cmds.push("cargo test".to_string());
        cmds.push("cargo clippy".to_string());
    }
    if dir.join("package.json").exists() {
        stack.push("Node.js".to_string());
        if dir.join("pnpm-lock.yaml").exists() {
            cmds.push("pnpm install".to_string());
            cmds.push("pnpm test".to_string());
        } else if dir.join("yarn.lock").exists() {
            cmds.push("yarn install".to_string());
            cmds.push("yarn test".to_string());
        } else {
            cmds.push("npm install".to_string());
            cmds.push("npm test".to_string());
        }
        // Detect framework
        if let Ok(pkg) = std::fs::read_to_string(dir.join("package.json")) {
            if pkg.contains("\"react\"") { stack.push("React".to_string()); }
            if pkg.contains("\"vue\"") { stack.push("Vue".to_string()); }
            if pkg.contains("\"next\"") { stack.push("Next.js".to_string()); }
            if pkg.contains("\"typescript\"") { stack.push("TypeScript".to_string()); }
        }
    }
    if dir.join("pom.xml").exists() {
        stack.push("Java/Maven".to_string());
        cmds.push("mvn compile".to_string());
        cmds.push("mvn test".to_string());
    }
    if dir.join("build.gradle").exists() || dir.join("build.gradle.kts").exists() {
        stack.push("Java/Gradle".to_string());
        cmds.push("./gradlew build".to_string());
        cmds.push("./gradlew test".to_string());
    }
    if dir.join("pyproject.toml").exists() || dir.join("setup.py").exists() {
        stack.push("Python".to_string());
        cmds.push("pip install -e .".to_string());
        cmds.push("pytest".to_string());
    }
    if dir.join("requirements.txt").exists() && !stack.iter().any(|s| s == "Python") {
        stack.push("Python".to_string());
        cmds.push("pip install -r requirements.txt".to_string());
        cmds.push("pytest".to_string());
    }
    if dir.join("go.mod").exists() {
        stack.push("Go".to_string());
        cmds.push("go build ./...".to_string());
        cmds.push("go test ./...".to_string());
    }

    (stack, cmds)
}

/// Return framework-specific hints.
fn framework_hints(dir: &Path) -> Vec<String> {
    let mut hints = Vec::new();
    if dir.join("Cargo.toml").exists() {
        hints.push("Use `anyhow::Result` for error handling".to_string());
        hints.push("Run `cargo fmt` before committing".to_string());
    }
    if dir.join(".eslintrc.json").exists() || dir.join(".eslintrc.js").exists() {
        hints.push("Run linter before committing".to_string());
    }
    if dir.join("tsconfig.json").exists() {
        hints.push("Ensure TypeScript strict mode compliance".to_string());
    }
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rust_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"test\"").unwrap();
        let result = generate_project_instructions(tmp.path());
        assert!(result.contains("Rust"));
        assert!(result.contains("cargo build"));
        assert!(result.contains("cargo test"));
    }

    #[test]
    fn test_node_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"dependencies":{"react":"^18"}}"#).unwrap();
        let result = generate_project_instructions(tmp.path());
        assert!(result.contains("Node.js"));
        assert!(result.contains("React"));
        assert!(result.contains("npm install"));
    }

    #[test]
    fn test_python_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]\nname = \"test\"").unwrap();
        let result = generate_project_instructions(tmp.path());
        assert!(result.contains("Python"));
        assert!(result.contains("pytest"));
    }

    #[test]
    fn test_empty_project() {
        let tmp = tempfile::tempdir().unwrap();
        let result = generate_project_instructions(tmp.path());
        assert!(result.contains("Project Instructions"));
        assert!(result.contains("Follow existing patterns"));
    }

    #[test]
    fn test_go_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module example.com/test").unwrap();
        let result = generate_project_instructions(tmp.path());
        assert!(result.contains("Go"));
        assert!(result.contains("go test"));
    }
}
