// crates/atomcode-core/tests/plugin_integration.rs
//
// End-to-end smoke test for the plugin marketplace pipeline:
// add_marketplace → install → SkillRegistry::reload + CustomCommandRegistry::load.
// Verifies that newly-installed plugin assets are visible to the in-process
// registries that the TUI consults on `/plugin` reload.
//
// Mutates the process-wide `ATOMCODE_HOME` env var, so we serialise via
// `#[serial_test::serial]` to avoid colliding with other tests that read
// the same variable.

use std::process::Command;

#[test]
#[serial_test::serial]
fn add_install_reload_flow() {
    // Set + unset on drop via this guard mirrors the in-tree
    // `plugin::test_support::isolated_home`. Test files outside the crate
    // can't see `pub(crate)` items so we inline the small guard here.
    // Held only for its `Drop` (removes ATOMCODE_HOME); the TempDir field is
    // never read directly.
    struct Guard(#[allow(dead_code)] tempfile::TempDir);
    impl Drop for Guard {
        fn drop(&mut self) {
            std::env::remove_var("ATOMCODE_HOME");
        }
    }
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let _guard = Guard(home);

    // Build a minimal plugin repo with a skill and a command.
    let workspace = tempfile::tempdir().unwrap();
    let repo = workspace.path().join("e2e");
    std::fs::create_dir_all(repo.join("skills/sk")).unwrap();
    std::fs::write(
        repo.join("skills/sk/SKILL.md"),
        "---\nname: sk\ndescription: e2e skill\n---\nbody",
    )
    .unwrap();
    std::fs::create_dir_all(repo.join("commands")).unwrap();
    std::fs::write(
        repo.join("commands/c.md"),
        "---\nname: c\ndescription: e2e cmd\n---\necho",
    )
    .unwrap();
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.email", "t@t"])
        .current_dir(&repo)
        .status()
        .unwrap();
    Command::new("git")
        .args(["config", "user.name", "t"])
        .current_dir(&repo)
        .status()
        .unwrap();
    Command::new("git")
        .args(["add", "-A"])
        .current_dir(&repo)
        .status()
        .unwrap();
    Command::new("git")
        .args(["commit", "-q", "-m", "init"])
        .current_dir(&repo)
        .status()
        .unwrap();

    let url = format!("file://{}", repo.display());
    atomcode_core::plugin::marketplace::add_marketplace(&url).unwrap();
    atomcode_core::plugin::installer::install("e2e", "e2e", atomcode_core::plugin::InstallScope::User).unwrap();

    // Verify SkillRegistry sees `e2e:sk`.
    let working = tempfile::tempdir().unwrap();
    let mut reg = atomcode_core::skill::SkillRegistry::new();
    reg.reload(working.path());
    assert!(reg.get("e2e:sk").is_some(), "missing skill e2e:sk");

    // Verify CustomCommandRegistry sees `e2e:c`. The registry moved to
    // atomcode-tuix (refactor: driver-only modules out of core); the plugin
    // install + skill reload it exercises still live in atomcode_core.
    let creg = atomcode_tuix::custom_commands::CustomCommandRegistry::load(working.path());
    assert!(creg.get("e2e:c").is_some(), "missing command e2e:c");
}
