//! Example skills that ship inside the binary.
//!
//! The desktop setup wizard seeds `assets/setup-seeds/skills/` into the user tree
//! (`setup::run`, behind the `setup` feature). A host with no wizard has no such
//! moment: on Android, `/skills` came back empty and nothing showed a new person what
//! a skill — or an MCP server — looks like, so the agent arrived "bare".
//!
//! These are the SAME asset files the wizard seeds, embedded individually instead of
//! in the packed tarball (a phone build does not need `zstd`/`tar` for two markdown
//! files), installed with a different policy: an example is seeded ONCE, recorded in a
//! ledger next to them, and never touched again. The wizard reinstalls when content
//! changes because running it is an explicit request; a start-up hook is not, and it
//! must not overwrite an example somebody edited or resurrect one they deleted.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Which examples this tree has already been given, one name per line.
///
/// A dotfile, not a directory: the skill loader scans `*/SKILL.md` and flat `*.md`, so
/// this can never be mistaken for a skill. It has to live OUTSIDE the example
/// directories — a marker inside one disappears with the directory, and then deleting
/// an example would just make it come back on the next start.
const LEDGER: &str = ".example-seeds";

/// One example skill: the invocation name (its directory) and the `SKILL.md` body.
pub struct ExampleSkill {
    pub name: &'static str,
    pub body: &'static str,
}

/// The examples this build ships.
///
/// `project-check` is useful on its own (verify before claiming success, and the
/// device-specific limits that change how); `add-mcp-server` is the MCP walk-through
/// the phone had no other way to discover, including which commands Android actually
/// allows an MCP stdio server to be.
pub const EXAMPLE_SKILLS: &[ExampleSkill] = &[
    ExampleSkill {
        name: "project-check",
        body: include_str!("../assets/setup-seeds/skills/project-check/SKILL.md"),
    },
    ExampleSkill {
        name: "add-mcp-server",
        body: include_str!("../assets/setup-seeds/skills/add-mcp-server/SKILL.md"),
    },
];

/// What a seeding pass did, so the host can log it once instead of silently writing
/// files into someone's tree.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SeededExamples {
    pub written: Vec<PathBuf>,
    pub kept: Vec<PathBuf>,
}

/// Install the examples under `skills_dir` (the user tree's `skills/`), skipping any
/// already recorded in the ledger. Never overwrites, never deletes, never re-adds.
pub fn seed_missing(skills_dir: &Path) -> SeededExamples {
    let mut seeded = SeededExamples::default();
    let mut ledger = read_ledger(skills_dir);
    let mut ledger_changed = false;
    for example in EXAMPLE_SKILLS {
        let dir = skills_dir.join(example.name);
        if ledger.contains(example.name) {
            // Seeded before: whatever the person did with it since is their call.
            seeded.kept.push(dir);
            continue;
        }
        if dir.join("SKILL.md").exists() {
            // Present but unrecorded (an older build, or someone's own copy):
            // adopt it into the ledger without touching the file.
            ledger.insert(example.name.to_string());
            ledger_changed = true;
            seeded.kept.push(dir);
            continue;
        }
        if let Err(error) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), %error, "cannot create the example skill directory");
            continue;
        }
        match std::fs::write(dir.join("SKILL.md"), example.body) {
            Ok(()) => {
                ledger.insert(example.name.to_string());
                ledger_changed = true;
                seeded.written.push(dir);
            }
            Err(error) => {
                tracing::warn!(
                    path = %dir.join("SKILL.md").display(),
                    %error,
                    "cannot write the example skill"
                );
            }
        }
    }
    if ledger_changed {
        write_ledger(skills_dir, &ledger);
    }
    seeded
}

fn read_ledger(skills_dir: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(skills_dir.join(LEDGER))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn write_ledger(skills_dir: &Path, names: &BTreeSet<String>) {
    let mut body = String::new();
    for name in names {
        body.push_str(name);
        body.push('\n');
    }
    if let Err(error) = std::fs::write(skills_dir.join(LEDGER), body) {
        tracing::warn!(%error, "cannot record the seeded example skills");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn examples_parse_as_skills() {
        // A frontmatter with a description is what the catalog lists; a broken one
        // would load as an unnamed blob and never be offered to the model.
        for example in EXAMPLE_SKILLS {
            assert!(example.body.starts_with("---"), "{}", example.name);
            assert!(
                example.body.contains("description:"),
                "{} has no description",
                example.name
            );
            assert!(
                example.body.contains(&format!("name: {}", example.name)),
                "{} declares another name",
                example.name
            );
        }
    }

    #[test]
    fn a_first_start_writes_every_example_once() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        let first = seed_missing(&skills);
        assert_eq!(first.written.len(), EXAMPLE_SKILLS.len());
        assert!(first.kept.is_empty());
        for example in EXAMPLE_SKILLS {
            let path = skills.join(example.name).join("SKILL.md");
            assert_eq!(std::fs::read_to_string(path).unwrap(), example.body);
        }

        // A second start is a no-op, not a rewrite.
        let second = seed_missing(&skills);
        assert!(second.written.is_empty());
        assert_eq!(second.kept.len(), EXAMPLE_SKILLS.len());
    }

    #[test]
    fn an_edited_or_deleted_example_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        seed_missing(&skills);

        let edited = skills.join("project-check").join("SKILL.md");
        std::fs::write(&edited, "my own version\n").unwrap();
        // Deleting one must stick too: an update may not resurrect it.
        std::fs::remove_dir_all(skills.join("add-mcp-server")).unwrap();

        let again = seed_missing(&skills);
        assert!(again.written.is_empty(), "{:?}", again.written);
        assert_eq!(
            std::fs::read_to_string(&edited).unwrap(),
            "my own version\n",
            "a person's edit must survive a start-up"
        );
    }
}
