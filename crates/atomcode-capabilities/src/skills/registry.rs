//! Discover + index skills from a set of directories. Neutral: the caller supplies the
//! directories (the standard `~/.claude/skills` etc. precedence is a driver concern —
//! see [`standard_skill_dirs`]). Ported from production `skill.rs` `SkillRegistry`.

use super::skill::{parse_skill_dir, parse_skill_file, Skill};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Skills indexed by name. `BTreeMap` for deterministic (sorted) order — the skill list
/// is injected into the system prompt, so a stable order keeps the prompt prefix
/// byte-identical (prompt-prefix caching), same rationale as the kernel ToolRegistry.
pub struct SkillRegistry {
    skills: BTreeMap<String, Arc<Skill>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            skills: BTreeMap::new(),
        }
    }

    /// Load from `dirs` in LOW→HIGH priority order (a later dir's same-named skill wins).
    /// Each dir is scanned for flat `*.md` files AND `*/SKILL.md` subdirectories; parse
    /// failures are skipped.
    pub fn load(dirs: &[PathBuf]) -> Self {
        let mut reg = Self::new();
        for dir in dirs {
            reg.load_dir(dir, None);
        }
        reg
    }

    /// Load one directory, optionally namespacing the skill names (`{ns}:{name}`).
    pub fn load_dir(&mut self, dir: &Path, namespace: Option<&str>) {
        self.scan_skill_dir(dir, namespace, 0);
    }

    /// Scan `dir` for skills, recursing into grouping subdirectories. A subdir holding a
    /// `SKILL.md` is a skill (NOT descended into — its own files are skill resources); a
    /// subdir without one is a grouping directory whose nested skills are discovered
    /// recursively, so `skills/GROUP/SUB/SKILL.md` is still found. Flat `*.md`
    /// slash-commands are TOP-LEVEL only (depth 0) — a skill's own `*.md` are resources,
    /// not separate commands. `depth` is bounded to guard against symlink cycles.
    fn scan_skill_dir(&mut self, dir: &Path, namespace: Option<&str>, depth: usize) {
        const MAX_DEPTH: usize = 8;
        if depth > MAX_DEPTH {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_file() {
                if depth == 0 && p.extension().and_then(|e| e.to_str()) == Some("md") {
                    if let Ok(s) = parse_skill_file(&p, namespace) {
                        self.skills.insert(s.name.clone(), Arc::new(s));
                    }
                }
            } else if p.is_dir() {
                let skill_md = p.join("SKILL.md");
                if skill_md.is_file() {
                    if let Ok(s) = parse_skill_dir(&p, &skill_md, namespace) {
                        self.skills.insert(s.name.clone(), Arc::new(s));
                    }
                } else {
                    self.scan_skill_dir(&p, namespace, depth + 1);
                }
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<Skill>> {
        self.skills.get(name).cloned()
    }
    pub fn len(&self) -> usize {
        self.skills.len()
    }
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
    /// `(name, description)` for every skill, sorted by name.
    pub fn list(&self) -> Vec<(String, String)> {
        self.skills
            .values()
            .map(|s| (s.name.clone(), s.description.clone()))
            .collect()
    }

    /// Clear and reload from the standard home+project skill dirs (the `/skills`
    /// slash-menu source), under the `skills:` namespace — matching the driver's
    /// slash-command convention. Mirrors core `SkillRegistry::reload`.
    ///
    /// Returns per-skill load warnings for signature compatibility with core's
    /// `reload` (driver call sites surface them). Currently always empty: this loader
    /// silently skips unparseable skills — the SAME behavior the runtime skill path
    /// uses — so no parse warnings are collected.
    pub fn reload(&mut self, working_dir: &Path) -> Vec<String> {
        self.skills.clear();
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        for dir in standard_skill_dirs(&home, working_dir) {
            self.load_dir(&dir, Some("skills"));
        }
        Vec::new()
    }

    /// Skills a user can invoke from the `/` menu (frontmatter `user-invocable` not
    /// `false`), sorted by name.
    pub fn user_invocable(&self) -> impl Iterator<Item = &Skill> {
        self.skills
            .values()
            .map(|s| s.as_ref())
            .filter(|s| s.user_invocable)
    }

    /// Insert a skill programmatically (same-name replaces). Mirrors core
    /// `SkillRegistry::register`; used by drivers injecting non-file skills and by tests.
    pub fn register(&mut self, skill: Skill) {
        self.skills.insert(skill.name.clone(), Arc::new(skill));
    }

    /// Every loaded skill, sorted by name.
    pub fn all(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values().map(|s| s.as_ref())
    }

    /// Render the `=== AVAILABLE SKILLS ===` system-prompt section (budget-gated,
    /// source-ranked), or `None` when no skills are installed. See
    /// [`super::render`].
    pub fn render_catalog(&self) -> Option<String> {
        let entries: Vec<super::render::CatalogEntry> = self
            .skills
            .values()
            .map(|s| super::render::CatalogEntry {
                name: s.name.clone(),
                hint: None, // capabilities skills carry no argument hint
                description: s.description.clone(),
                source_rank: super::render::source_rank(&s.source_path),
            })
            .collect();
        super::render::render_skill_catalog(&entries)
    }
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The standard skill directories (LOW→HIGH priority), Claude-Code-compatible. A driver
/// may pass this to [`SkillRegistry::load`] or supply its own. `home` = user home dir;
/// `project` = the workspace root.
pub fn standard_skill_dirs(home: &Path, project: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".claude/commands"),
        home.join(".atomcode/commands"),
        home.join(".claude/skills"),
        // `.agents/skills` — cross-agent shared convention (opencode et al.). Between
        // `.claude` and `.atomcode` so atomcode-native skills win a same-name collision.
        home.join(".agents/skills"),
        home.join(".atomcode/skills"),
        project.join(".claude/commands"),
        project.join(".atomcode/commands"),
        project.join(".claude/skills"),
        project.join(".agents/skills"),
        project.join(".atomcode/skills"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_flat_and_dir_skills_with_precedence() {
        let d = tempfile::tempdir().unwrap();
        // flat command
        std::fs::write(d.path().join("greet.md"), "Hello $ARGUMENTS\n").unwrap();
        // dir-style skill
        std::fs::create_dir_all(d.path().join("review")).unwrap();
        std::fs::write(
            d.path().join("review/SKILL.md"),
            "---\ndescription: review code\n---\nReview the diff.\n",
        )
        .unwrap();

        let reg = SkillRegistry::load(&[d.path().to_path_buf()]);
        assert_eq!(reg.len(), 2);
        assert!(reg.get("greet").is_some());
        let review = reg.get("review").unwrap();
        assert_eq!(review.description, "review code");
    }

    #[test]
    fn later_dir_overrides_same_name() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("x.md"), "from A\n").unwrap();
        std::fs::write(b.path().join("x.md"), "from B\n").unwrap();
        let reg = SkillRegistry::load(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        assert!(
            reg.get("x").unwrap().template.contains("from B"),
            "later dir wins"
        );
    }

    #[test]
    fn missing_dir_is_skipped() {
        let reg = SkillRegistry::load(&[PathBuf::from("/no/such/skills/dir")]);
        assert!(reg.is_empty());
    }

    #[test]
    fn standard_dirs_include_agents_skills_between_claude_and_atomcode() {
        let home = Path::new("/home/u");
        let project = Path::new("/proj");
        let dirs = standard_skill_dirs(home, project);
        // `.agents/skills` is the cross-agent shared convention (opencode et al.) —
        // scanned at BOTH user and project level so shared skills load directly.
        assert!(
            dirs.contains(&home.join(".agents/skills")),
            "user-level ~/.agents/skills"
        );
        assert!(
            dirs.contains(&project.join(".agents/skills")),
            "project-level .agents/skills"
        );
        // Precedence (last-wins): .claude < .agents < .atomcode at each level, so a
        // user's atomcode-native skill still overrides a same-named shared one.
        let pos = |p: PathBuf| dirs.iter().position(|d| *d == p).expect("dir present");
        assert!(pos(home.join(".claude/skills")) < pos(home.join(".agents/skills")));
        assert!(pos(home.join(".agents/skills")) < pos(home.join(".atomcode/skills")));
        assert!(pos(project.join(".claude/skills")) < pos(project.join(".agents/skills")));
        assert!(pos(project.join(".agents/skills")) < pos(project.join(".atomcode/skills")));
    }

    #[test]
    fn discovers_nested_skills_under_grouping_dirs() {
        let d = tempfile::tempdir().unwrap();
        // A grouping dir (no SKILL.md of its own) holding a nested skill two levels deep.
        std::fs::create_dir_all(d.path().join("GROUP/sub")).unwrap();
        std::fs::write(
            d.path().join("GROUP/sub/SKILL.md"),
            "---\ndescription: nested skill\n---\nDo the nested thing.\n",
        )
        .unwrap();
        // A skill dir's OWN nested *.md must NOT be mis-loaded as a separate skill.
        std::fs::create_dir_all(d.path().join("top")).unwrap();
        std::fs::write(
            d.path().join("top/SKILL.md"),
            "---\ndescription: top\n---\nTop.\n",
        )
        .unwrap();
        std::fs::write(
            d.path().join("top/resource.md"),
            "internal resource, not a command\n",
        )
        .unwrap();

        let reg = SkillRegistry::load(&[d.path().to_path_buf()]);
        assert!(
            reg.get("sub").is_some(),
            "nested GROUP/sub/SKILL.md must be discovered"
        );
        assert_eq!(reg.get("sub").unwrap().description, "nested skill");
        assert!(reg.get("top").is_some(), "top-level skill dir loaded");
        assert!(
            reg.get("resource").is_none(),
            "a skill's own resource .md is not a command"
        );
    }
}
