// crates/atomcode-core/src/config/instructions.rs
//
// Three-tier layered instruction system: global → project → user.
//
// 1. Global:  ~/.atomcode/ATOMCODE.md — personal preferences across all projects
// 2. Project: <project>/.atomcode.md, ATOMCODE.md, AGENTS.md, CLAUDE.md,
//             or claude.md (first match wins; AGENTS.md is the open standard
//             for AI coding agents; CLAUDE.md/claude.md are accepted for
//             compatibility with projects migrating from Claude Code)
// 3. User:    <project>/.atomcode.user.md — personal per-project, in .gitignore

use std::path::{Path, PathBuf};

/// Maximum size for a single instruction file (1 MB).
const MAX_INSTRUCTION_SIZE: usize = 1_048_576;

#[derive(Debug)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub content: String,
    pub level: InstructionLevel,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InstructionLevel {
    Global,
    Project,
    User,
}

impl InstructionLevel {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Global => "GLOBAL",
            Self::Project => "PROJECT",
            Self::User => "USER",
        }
    }
}

impl std::fmt::Display for InstructionLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

pub struct LayeredInstructions {
    pub global: Option<InstructionFile>,
    pub project: Option<InstructionFile>,
    pub user: Option<InstructionFile>,
}

impl LayeredInstructions {
    /// Load all three instruction tiers from disk.
    ///
    /// - Global:  `~/.atomcode/ATOMCODE.md`
    /// - Project: `<project_root>/.atomcode.md`, `ATOMCODE.md`, `AGENTS.md`,
    ///   `CLAUDE.md`, or `claude.md` (first match wins, in that order)
    /// - User:    `<project_root>/.atomcode.user.md`
    pub fn load(project_root: &Path) -> Self {
        let config_dir = crate::config::Config::config_dir();
        let global = Self::try_load(&config_dir.join("ATOMCODE.md"), InstructionLevel::Global);

        // Lookup order: native names first, then AGENTS.md (open standard),
        // then Claude Code names for compatibility.
        let project = [".atomcode.md", "ATOMCODE.md", "AGENTS.md", "CLAUDE.md", "claude.md"]
            .iter()
            .find_map(|name| Self::try_load(&project_root.join(name), InstructionLevel::Project));

        let user =
            Self::try_load(&project_root.join(".atomcode.user.md"), InstructionLevel::User);

        Self {
            global,
            project,
            user,
        }
    }

    fn try_load(path: &Path, level: InstructionLevel) -> Option<InstructionFile> {
        let content = std::fs::read_to_string(path).ok()?;
        if content.trim().is_empty() {
            return None;
        }
        let content = if content.len() > MAX_INSTRUCTION_SIZE {
            let truncated: String = content.chars().take(MAX_INSTRUCTION_SIZE).collect();
            format!("{}\n\n[Truncated — file exceeds 1MB]", truncated)
        } else {
            content
        };
        Some(InstructionFile {
            path: path.to_path_buf(),
            content,
            level,
        })
    }

    /// Merge all levels into prompt text. Low priority first, high last
    /// (recency effect: user > project > global).
    pub fn merged(&self) -> String {
        let mut parts = Vec::new();
        if let Some(ref g) = self.global {
            parts.push(format!(
                "=== {} INSTRUCTIONS ({}) ===\n{}",
                g.level.label(),
                g.path.display(),
                g.content.trim()
            ));
        }
        if let Some(ref p) = self.project {
            parts.push(format!(
                "=== {} INSTRUCTIONS ({}) ===\n{}",
                p.level.label(),
                p.path.display(),
                p.content.trim()
            ));
        }
        if let Some(ref u) = self.user {
            parts.push(format!(
                "=== {} INSTRUCTIONS ({}) ===\n{}",
                u.level.label(),
                u.path.display(),
                u.content.trim()
            ));
        }
        parts.join("\n\n")
    }

    /// Return status for all three levels (loaded path or None).
    pub fn status_lines(&self) -> Vec<(InstructionLevel, Option<&Path>)> {
        vec![
            (
                InstructionLevel::Global,
                self.global.as_ref().map(|f| f.path.as_path()),
            ),
            (
                InstructionLevel::Project,
                self.project.as_ref().map(|f| f.path.as_path()),
            ),
            (
                InstructionLevel::User,
                self.user.as_ref().map(|f| f.path.as_path()),
            ),
        ]
    }

    pub fn has_any(&self) -> bool {
        self.global.is_some() || self.project.is_some() || self.user.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn empty_dir_produces_no_instructions() {
        let tmp = tempfile::tempdir().unwrap();
        let instructions = LayeredInstructions {
            global: None,
            project: LayeredInstructions::try_load(
                &tmp.path().join(".atomcode.md"),
                InstructionLevel::Project,
            ),
            user: LayeredInstructions::try_load(
                &tmp.path().join(".atomcode.user.md"),
                InstructionLevel::User,
            ),
        };
        assert!(!instructions.has_any());
        assert!(instructions.merged().is_empty());
    }

    #[test]
    fn project_atomcode_md_is_found() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".atomcode.md"), "Use tabs.").unwrap();
        let instructions = LayeredInstructions::try_load(
            &tmp.path().join(".atomcode.md"),
            InstructionLevel::Project,
        );
        assert!(instructions.is_some());
        let f = instructions.unwrap();
        assert_eq!(f.level, InstructionLevel::Project);
        assert!(f.content.contains("Use tabs."));
    }

    #[test]
    fn agents_md_used_as_project_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "from AGENTS.md open standard").unwrap();
        let instructions = LayeredInstructions::load(tmp.path());
        let project = instructions
            .project
            .expect("AGENTS.md should be loaded as project tier");
        assert_eq!(project.level, InstructionLevel::Project);
        assert!(project.content.contains("from AGENTS.md open standard"));
        assert!(project.path.ends_with("AGENTS.md"));
    }

    #[test]
    fn claude_md_used_as_project_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "from Claude Code").unwrap();
        let instructions = LayeredInstructions::load(tmp.path());
        let project = instructions
            .project
            .expect("CLAUDE.md should be loaded as project tier");
        assert_eq!(project.level, InstructionLevel::Project);
        assert!(project.content.contains("from Claude Code"));
        assert!(project.path.ends_with("CLAUDE.md"));
    }

    #[test]
    fn lowercase_claude_md_used_as_project_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("claude.md"), "lowercase claude").unwrap();
        let instructions = LayeredInstructions::load(tmp.path());
        let project = instructions
            .project
            .expect("claude.md should be loaded as project tier");
        assert!(project.content.contains("lowercase claude"));
    }

    #[test]
    fn atomcode_md_preferred_over_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".atomcode.md"), "atomcode wins").unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "agents loses").unwrap();
        let instructions = LayeredInstructions::load(tmp.path());
        let project = instructions.project.expect("project tier should load");
        assert!(project.content.contains("atomcode wins"));
    }

    #[test]
    fn atomcode_md_preferred_over_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".atomcode.md"), "atomcode wins").unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "claude loses").unwrap();
        let instructions = LayeredInstructions::load(tmp.path());
        let project = instructions.project.expect("project tier should load");
        assert!(project.content.contains("atomcode wins"));
    }

    #[test]
    fn agents_md_preferred_over_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("AGENTS.md"), "agents wins").unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "claude loses").unwrap();
        let instructions = LayeredInstructions::load(tmp.path());
        let project = instructions.project.expect("project tier should load");
        assert!(project.content.contains("agents wins"));
    }

    #[test]
    fn atomcode_uppercase_preferred_over_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("ATOMCODE.md"), "ATOMCODE wins").unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "claude loses").unwrap();
        let instructions = LayeredInstructions::load(tmp.path());
        let project = instructions.project.expect("project tier should load");
        assert!(project.content.contains("ATOMCODE wins"));
    }

    #[test]
    fn lowercase_preferred_over_uppercase() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".atomcode.md"), "lowercase wins").unwrap();
        fs::write(tmp.path().join("ATOMCODE.md"), "uppercase loses").unwrap();
        let instructions = LayeredInstructions::load(tmp.path());
        let project = instructions.project.expect("project tier should load");
        assert!(project.content.contains("lowercase wins"));
        assert!(project.path.ends_with(".atomcode.md"));
    }

    #[test]
    fn user_instructions_loaded() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".atomcode.user.md"), "my prefs").unwrap();
        let user = LayeredInstructions::try_load(
            &tmp.path().join(".atomcode.user.md"),
            InstructionLevel::User,
        );
        assert!(user.is_some());
        let f = user.unwrap();
        assert_eq!(f.level, InstructionLevel::User);
        assert!(f.content.contains("my prefs"));
    }

    #[test]
    fn empty_file_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".atomcode.md"), "   \n  \n").unwrap();
        let project = LayeredInstructions::try_load(
            &tmp.path().join(".atomcode.md"),
            InstructionLevel::Project,
        );
        assert!(project.is_none());
    }

    #[test]
    fn merged_output_order_is_global_project_user() {
        let tmp = tempfile::tempdir().unwrap();
        let instructions = LayeredInstructions {
            global: Some(InstructionFile {
                path: tmp.path().join("global.md"),
                content: "GLOBAL_CONTENT".to_string(),
                level: InstructionLevel::Global,
            }),
            project: Some(InstructionFile {
                path: tmp.path().join("project.md"),
                content: "PROJECT_CONTENT".to_string(),
                level: InstructionLevel::Project,
            }),
            user: Some(InstructionFile {
                path: tmp.path().join("user.md"),
                content: "USER_CONTENT".to_string(),
                level: InstructionLevel::User,
            }),
        };
        let merged = instructions.merged();
        let global_pos = merged.find("GLOBAL_CONTENT").unwrap();
        let project_pos = merged.find("PROJECT_CONTENT").unwrap();
        let user_pos = merged.find("USER_CONTENT").unwrap();
        assert!(
            global_pos < project_pos,
            "global must come before project"
        );
        assert!(
            project_pos < user_pos,
            "project must come before user"
        );
    }

    #[test]
    fn status_lines_show_all_three_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let instructions = LayeredInstructions {
            global: Some(InstructionFile {
                path: tmp.path().join("g.md"),
                content: "g".to_string(),
                level: InstructionLevel::Global,
            }),
            project: None,
            user: Some(InstructionFile {
                path: tmp.path().join("u.md"),
                content: "u".to_string(),
                level: InstructionLevel::User,
            }),
        };
        let lines = instructions.status_lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].0, InstructionLevel::Global);
        assert!(lines[0].1.is_some());
        assert_eq!(lines[1].0, InstructionLevel::Project);
        assert!(lines[1].1.is_none());
        assert_eq!(lines[2].0, InstructionLevel::User);
        assert!(lines[2].1.is_some());
    }

    #[test]
    fn large_file_is_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let big = "x".repeat(MAX_INSTRUCTION_SIZE + 100);
        fs::write(tmp.path().join("big.md"), &big).unwrap();
        let loaded = LayeredInstructions::try_load(
            &tmp.path().join("big.md"),
            InstructionLevel::Global,
        );
        assert!(loaded.is_some());
        let f = loaded.unwrap();
        assert!(f.content.ends_with("[Truncated — file exceeds 1MB]"));
        // Content should be capped around MAX_INSTRUCTION_SIZE + the suffix.
        assert!(f.content.len() < big.len());
    }

    #[test]
    fn has_any_returns_true_when_any_level_loaded() {
        let instructions = LayeredInstructions {
            global: None,
            project: Some(InstructionFile {
                path: PathBuf::from("/tmp/p.md"),
                content: "p".to_string(),
                level: InstructionLevel::Project,
            }),
            user: None,
        };
        assert!(instructions.has_any());
    }

    #[test]
    fn has_any_returns_false_when_all_none() {
        let instructions = LayeredInstructions {
            global: None,
            project: None,
            user: None,
        };
        assert!(!instructions.has_any());
    }

    #[test]
    fn level_labels_are_correct() {
        assert_eq!(InstructionLevel::Global.label(), "GLOBAL");
        assert_eq!(InstructionLevel::Project.label(), "PROJECT");
        assert_eq!(InstructionLevel::User.label(), "USER");
    }

    #[test]
    fn merged_with_only_project_produces_single_section() {
        let instructions = LayeredInstructions {
            global: None,
            project: Some(InstructionFile {
                path: PathBuf::from("/project/.atomcode.md"),
                content: "Only project rules".to_string(),
                level: InstructionLevel::Project,
            }),
            user: None,
        };
        let merged = instructions.merged();
        assert!(merged.contains("=== PROJECT INSTRUCTIONS"));
        assert!(merged.contains("Only project rules"));
        assert!(!merged.contains("GLOBAL"));
        assert!(!merged.contains("USER"));
    }
}
