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

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary
// runs, so tests never persist into the developer's real home. isolate_home is a
// no-op when the var is already set.
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

#[test]
#[serial_test::serial]
fn add_install_reload_flow() {
    // Point ATOMCODE_HOME at a fresh tempdir for this test. We deliberately do
    // NOT unset it afterwards: the isolate_home ctor keeps ATOMCODE_HOME pointed
    // at a temp dir for the whole binary, so unsetting here would leak back to
    // the real ~/.atomcode. Bind the TempDir to keep it (and its files) alive
    // for the duration of the test.
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let _home = home;

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
    atomcode_capabilities::plugin::marketplace::add_marketplace(&url).unwrap();
    atomcode_capabilities::plugin::installer::install(
        "e2e",
        "e2e",
        atomcode_capabilities::plugin::InstallScope::User,
    )
    .unwrap();

    // Verify SkillRegistry sees `e2e:sk`. Load standard dirs then installed-plugin
    // skill dirs — the two layers the retired `core::skill::reload` combined.
    let working = tempfile::tempdir().unwrap();
    let mut reg = atomcode_capabilities::skills::SkillRegistry::new();
    reg.reload(working.path());
    for assets in atomcode_capabilities::plugin::loader::iter_installed_plugin_assets() {
        for sd in assets.skills_dirs() {
            if sd.exists() {
                reg.load_dir(&sd, Some(&assets.plugin));
            }
        }
    }
    assert!(reg.get("e2e:sk").is_some(), "missing skill e2e:sk");

    // Verify CustomCommandRegistry sees `e2e:c`. The registry moved to
    // atomcode-tuix (refactor: driver-only modules out of core); the plugin
    // install + skill reload it exercises still live in atomcode_core.
    let creg = atomcode_tuix::custom_commands::CustomCommandRegistry::load(working.path());
    assert!(creg.get("e2e:c").is_some(), "missing command e2e:c");
}
