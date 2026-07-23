//! Scan a project directory and produce ProjectSignals. Pure filesystem
//! introspection — no external commands (no git/npm/cargo CLI), no LLM.

use crate::setup::state::compute_signals_hash;
use crate::setup::types::*;
use std::path::{Path, PathBuf};

const README_HEAD_BYTES: usize = 2048;
const ROOT_TREE_MAX_ENTRIES: usize = 50;

pub fn scan(project_root: &Path) -> ProjectSignals {
    let mut s = ProjectSignals::empty(project_root.to_path_buf());
    s.markers = collect_markers(project_root);
    s.stacks = derive_stacks(&s.markers);
    s.frameworks = derive_frameworks(project_root, &s.markers);
    s.package_mgrs = derive_pkg_mgrs(project_root, &s.markers);
    s.vcs = derive_vcs(project_root);
    s.ci = derive_ci(project_root);
    s.containerized = s
        .markers
        .iter()
        .any(|m| m.kind == MarkerKind::Dockerfile || m.kind == MarkerKind::K8sManifest);
    s.test_frameworks = derive_test_frameworks(project_root, &s.markers);
    s.root_tree = collect_root_tree(project_root);
    s.readme_head = read_readme_head(project_root);
    s.signals_hash =
        compute_signals_hash(&s.markers.iter().map(|m| m.path.clone()).collect::<Vec<_>>());
    s
}

fn collect_markers(root: &Path) -> Vec<Marker> {
    let probes: &[(&str, MarkerKind)] = &[
        ("Cargo.toml", MarkerKind::CargoToml),
        ("package.json", MarkerKind::PackageJson),
        ("pom.xml", MarkerKind::PomXml),
        ("build.gradle", MarkerKind::BuildGradle),
        ("build.gradle.kts", MarkerKind::BuildGradle),
        ("pyproject.toml", MarkerKind::PyprojectToml),
        ("requirements.txt", MarkerKind::RequirementsTxt),
        ("go.mod", MarkerKind::GoMod),
        ("Dockerfile", MarkerKind::Dockerfile),
        (".eslintrc.js", MarkerKind::EslintConfig),
        (".eslintrc.json", MarkerKind::EslintConfig),
        (".eslintrc.yml", MarkerKind::EslintConfig),
        ("rustfmt.toml", MarkerKind::RustfmtToml),
        ("clippy.toml", MarkerKind::ClippyToml),
        ("tsconfig.json", MarkerKind::TsConfig),
    ];
    let mut found = vec![];
    for (name, kind) in probes {
        let p = root.join(name);
        if p.exists() {
            found.push(Marker {
                path: p,
                kind: *kind,
            });
        }
    }
    if root.join(".git").is_dir() {
        found.push(Marker {
            path: root.join(".git"),
            kind: MarkerKind::GitDir,
        });
    }
    if root.join(".github/workflows").is_dir() {
        found.push(Marker {
            path: root.join(".github/workflows"),
            kind: MarkerKind::GhActionsDir,
        });
    }
    if root.join("prisma").is_dir() {
        found.push(Marker {
            path: root.join("prisma"),
            kind: MarkerKind::PrismaDir,
        });
    }
    // k8s heuristic — top-level k8s/ or helm/ dir.
    if root.join("k8s").is_dir() || root.join("helm").is_dir() {
        let path = if root.join("k8s").is_dir() {
            root.join("k8s")
        } else {
            root.join("helm")
        };
        found.push(Marker {
            path,
            kind: MarkerKind::K8sManifest,
        });
    }
    found
}

fn derive_stacks(markers: &[Marker]) -> Vec<Stack> {
    let mut s = vec![];
    let has = |k: MarkerKind| markers.iter().any(|m| m.kind == k);
    if has(MarkerKind::CargoToml) {
        s.push(Stack::Rust);
    }
    if has(MarkerKind::PackageJson) {
        s.push(Stack::Node);
    }
    if has(MarkerKind::PomXml) || has(MarkerKind::BuildGradle) {
        s.push(Stack::Java);
    }
    if has(MarkerKind::PyprojectToml) || has(MarkerKind::RequirementsTxt) {
        s.push(Stack::Python);
    }
    if has(MarkerKind::GoMod) {
        s.push(Stack::Go);
    }
    s
}

fn derive_frameworks(root: &Path, markers: &[Marker]) -> Vec<Framework> {
    let mut f = vec![];
    if markers.iter().any(|m| m.kind == MarkerKind::PackageJson) {
        if let Ok(raw) = std::fs::read_to_string(root.join("package.json")) {
            if raw.contains("\"react\"") {
                f.push(Framework::React);
            }
            if raw.contains("\"vue\"") {
                f.push(Framework::Vue);
            }
            if raw.contains("\"next\"") {
                f.push(Framework::Next);
            }
        }
    }
    if markers.iter().any(|m| m.kind == MarkerKind::CargoToml) {
        if let Ok(raw) = std::fs::read_to_string(root.join("Cargo.toml")) {
            if raw.contains("tokio") {
                f.push(Framework::Tokio);
            }
        }
    }
    if markers.iter().any(|m| m.kind == MarkerKind::PomXml) {
        if let Ok(raw) = std::fs::read_to_string(root.join("pom.xml")) {
            if raw.contains("spring-boot-starter") {
                f.push(Framework::Spring);
            }
        }
    }
    for fname in &["pyproject.toml", "requirements.txt"] {
        if let Ok(raw) = std::fs::read_to_string(root.join(fname)) {
            let lower = raw.to_lowercase();
            if lower.contains("django") && !f.contains(&Framework::Django) {
                f.push(Framework::Django);
            }
            if lower.contains("flask") && !f.contains(&Framework::Flask) {
                f.push(Framework::Flask);
            }
        }
    }
    f
}

fn derive_pkg_mgrs(root: &Path, markers: &[Marker]) -> Vec<PkgMgr> {
    let mut p = vec![];
    let has = |k: MarkerKind| markers.iter().any(|m| m.kind == k);
    if has(MarkerKind::CargoToml) {
        p.push(PkgMgr::Cargo);
    }
    if has(MarkerKind::GoMod) {
        p.push(PkgMgr::GoMod);
    }
    if has(MarkerKind::PomXml) {
        p.push(PkgMgr::Maven);
    }
    if has(MarkerKind::BuildGradle) {
        p.push(PkgMgr::Gradle);
    }
    if has(MarkerKind::PyprojectToml) {
        if root.join("poetry.lock").exists() {
            p.push(PkgMgr::Poetry);
        } else {
            p.push(PkgMgr::Pip);
        }
    } else if has(MarkerKind::RequirementsTxt) {
        p.push(PkgMgr::Pip);
    }
    if has(MarkerKind::PackageJson) {
        if root.join("pnpm-lock.yaml").exists() {
            p.push(PkgMgr::Pnpm);
        } else if root.join("yarn.lock").exists() {
            p.push(PkgMgr::Yarn);
        } else {
            p.push(PkgMgr::Npm);
        }
    }
    p
}

fn derive_vcs(root: &Path) -> VcsInfo {
    if !root.join(".git").exists() {
        return VcsInfo::None;
    }
    let remote = std::fs::read_to_string(root.join(".git/config"))
        .ok()
        .and_then(|cfg| {
            cfg.lines()
                .find(|l| l.trim().starts_with("url"))
                .and_then(|l| l.split('=').nth(1))
                .map(|s| s.trim().to_string())
        });
    VcsInfo::Git { remote }
}

fn derive_ci(root: &Path) -> CiInfo {
    let workflows = root.join(".github/workflows");
    if workflows.is_dir() {
        let count = std::fs::read_dir(&workflows)
            .map(|it| it.filter_map(|e| e.ok()).count())
            .unwrap_or(0);
        return CiInfo::GhActions {
            workflow_count: count,
        };
    }
    if root.join(".gitlab-ci.yml").exists() {
        return CiInfo::GitLab;
    }
    if root.join(".circleci").is_dir() || root.join("Jenkinsfile").exists() {
        return CiInfo::Other;
    }
    CiInfo::None
}

fn derive_test_frameworks(root: &Path, markers: &[Marker]) -> Vec<TestFw> {
    let mut tfs = vec![];
    if markers.iter().any(|m| m.kind == MarkerKind::CargoToml) {
        tfs.push(TestFw::CargoTest);
    }
    if let Ok(raw) = std::fs::read_to_string(root.join("package.json")) {
        if raw.contains("\"jest\"") {
            tfs.push(TestFw::Jest);
        }
        if raw.contains("\"vitest\"") {
            tfs.push(TestFw::Vitest);
        }
    }
    if root.join("pytest.ini").exists()
        || std::fs::read_to_string(root.join("pyproject.toml"))
            .ok()
            .map_or(false, |s| s.contains("[tool.pytest"))
    {
        tfs.push(TestFw::Pytest);
    }
    if root.join("pom.xml").exists()
        && std::fs::read_to_string(root.join("pom.xml"))
            .ok()
            .map_or(false, |s| s.contains("junit"))
    {
        tfs.push(TestFw::JUnit);
    }
    tfs
}

fn collect_root_tree(root: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !matches!(
                name,
                "node_modules" | "target" | ".git" | "dist" | "build" | ".next"
            )
        })
        .collect();
    entries.sort();
    entries.truncate(ROOT_TREE_MAX_ENTRIES);
    entries
}

fn read_readme_head(root: &Path) -> Option<String> {
    for name in ["README.md", "README.rst", "README.txt", "README"] {
        if let Ok(bytes) = std::fs::read(root.join(name)) {
            let head = if bytes.len() > README_HEAD_BYTES {
                &bytes[..README_HEAD_BYTES]
            } else {
                &bytes
            };
            return Some(String::from_utf8_lossy(head).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        dir
    }

    #[test]
    fn scan_empty_dir_returns_empty_signals() {
        let dir = tempfile::tempdir().unwrap();
        let s = scan(dir.path());
        assert!(s.markers.is_empty());
        assert!(s.stacks.is_empty());
        assert!(matches!(s.vcs, VcsInfo::None));
    }

    #[test]
    fn scan_rust_project_detects_cargo_and_stack() {
        let dir = setup_dir(&[("Cargo.toml", "[package]\nname = \"x\"")]);
        let s = scan(dir.path());
        assert!(s.markers.iter().any(|m| m.kind == MarkerKind::CargoToml));
        assert_eq!(s.stacks, vec![Stack::Rust]);
        assert!(s.package_mgrs.contains(&PkgMgr::Cargo));
    }

    #[test]
    fn scan_react_project_detects_framework() {
        let dir = setup_dir(&[("package.json", r#"{"dependencies":{"react":"^18"}}"#)]);
        let s = scan(dir.path());
        assert!(s.frameworks.contains(&Framework::React));
        assert!(s.package_mgrs.contains(&PkgMgr::Npm));
    }

    #[test]
    fn scan_with_git_dir_marks_vcs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join(".git/config"),
            "[remote \"origin\"]\n\turl = git@x.com:a/b\n",
        )
        .unwrap();
        let s = scan(dir.path());
        match s.vcs {
            VcsInfo::Git { remote } => assert!(remote.unwrap().contains("a/b")),
            _ => panic!("expected Git"),
        }
    }

    #[test]
    fn scan_docker_marks_containerized() {
        let dir = setup_dir(&[("Dockerfile", "FROM rust:1.80")]);
        let s = scan(dir.path());
        assert!(s.containerized);
    }

    #[test]
    fn scan_truncates_root_tree_to_50() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..100 {
            std::fs::write(dir.path().join(format!("file{i}.txt")), "x").unwrap();
        }
        let s = scan(dir.path());
        assert!(s.root_tree.len() <= 50);
    }

    #[test]
    fn scan_reads_readme_head_2kb_max() {
        let big = "x".repeat(5000);
        let dir = setup_dir(&[("README.md", &big)]);
        let s = scan(dir.path());
        let head = s.readme_head.unwrap();
        assert!(head.len() <= 2048);
    }

    #[test]
    fn scan_signals_hash_changes_when_marker_content_changes() {
        let dir = setup_dir(&[("Cargo.toml", "v1")]);
        let h1 = scan(dir.path()).signals_hash;
        std::fs::write(dir.path().join("Cargo.toml"), "v2").unwrap();
        let h2 = scan(dir.path()).signals_hash;
        assert_ne!(h1, h2);
    }
}
