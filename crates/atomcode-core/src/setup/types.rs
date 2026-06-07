//! Data types shared by every layer of setup. No logic — only `pub` shapes.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Kind / Id ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RecKind {
    Skill,
    Command,
    Hook,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RecId {
    pub kind: RecKind,
    pub slug: String,
}

impl RecId {
    pub fn new(kind: RecKind, slug: impl Into<String>) -> Self {
        Self { kind, slug: slug.into() }
    }
}

impl std::fmt::Display for RecId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}:{}", self.kind, self.slug)
    }
}

// ── ProjectSignals ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    pub path: PathBuf,
    pub kind: MarkerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerKind {
    CargoToml,
    PackageJson,
    PomXml,
    BuildGradle,
    PyprojectToml,
    RequirementsTxt,
    GoMod,
    Dockerfile,
    K8sManifest,
    GitDir,
    GhActionsDir,
    EslintConfig,
    RustfmtToml,
    ClippyToml,
    TsConfig,
    PrismaDir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stack {
    Rust,
    Node,
    Java,
    Python,
    Go,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Framework {
    React,
    Vue,
    Next,
    Spring,
    Django,
    Flask,
    Tokio,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PkgMgr {
    Cargo,
    Npm,
    Pnpm,
    Yarn,
    Pip,
    Poetry,
    Maven,
    Gradle,
    GoMod,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcsInfo {
    None,
    Git { remote: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiInfo {
    None,
    GhActions { workflow_count: usize },
    GitLab,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestFw {
    Jest,
    Vitest,
    Pytest,
    JUnit,
    CargoTest,
}

#[derive(Debug, Clone)]
pub struct ProjectSignals {
    pub project_root: PathBuf,
    pub markers: Vec<Marker>,
    pub stacks: Vec<Stack>,
    pub frameworks: Vec<Framework>,
    pub package_mgrs: Vec<PkgMgr>,
    pub vcs: VcsInfo,
    pub ci: CiInfo,
    pub containerized: bool,
    pub test_frameworks: Vec<TestFw>,
    pub root_tree: Vec<PathBuf>,
    pub readme_head: Option<String>,
    pub signals_hash: String,
}

impl ProjectSignals {
    pub fn empty(project_root: PathBuf) -> Self {
        Self {
            project_root,
            markers: vec![],
            stacks: vec![],
            frameworks: vec![],
            package_mgrs: vec![],
            vcs: VcsInfo::None,
            ci: CiInfo::None,
            containerized: false,
            test_frameworks: vec![],
            root_tree: vec![],
            readme_head: None,
            signals_hash: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rec_id_display_includes_kind_and_slug() {
        let id = RecId::new(RecKind::Skill, "rust-best-practices");
        assert_eq!(format!("{id}"), "Skill:rust-best-practices");
    }
}
