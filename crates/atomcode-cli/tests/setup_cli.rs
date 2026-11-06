//! Smoke test the `atomcode setup` subcommand via the built CLI binary.

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary
// runs, so tests never persist into the developer's real home. isolate_home is a
// inherited shell values are replaced before tests run.
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

#[test]
fn setup_succeeds_in_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_atomcode"))
        .arg("setup")
        .current_dir(dir.path())
        .env("ATOMCODE_HOME", user.path())
        .output()
        .expect("run atomcode setup");
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let ok = stdout.contains("Setup 完成") || stdout.contains("Setup complete");
    assert!(
        ok,
        "stdout did not contain a setup-success marker:\n{stdout}"
    );
}

#[test]
fn capability_check_and_review_do_not_install_and_context7_apply_stays_disabled() {
    let root = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_atomcode"))
            .args(args)
            .current_dir(root.path())
            .env("ATOMCODE_HOME", user.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let checked = run(&["setup", "--check"]);
    assert!(checked.contains("Context7"));
    assert!(checked.contains("Rust LSP"));
    assert!(!root.path().join(".mcp.json").exists());
    assert!(!user.path().join("skills").exists());
    let plan = run(&["setup", "--install", "context7"]);
    assert!(plan.contains("--yes"));
    assert!(!root.path().join(".mcp.json").exists());
    run(&["setup", "--install", "context7", "--yes"]);
    let contents = std::fs::read_to_string(root.path().join(".mcp.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&contents).unwrap();
    assert_eq!(
        value["mcpServers"]["context7"],
        serde_json::json!({"url":"https://mcp.context7.com/mcp","disabled":true})
    );
    let recheck = run(&["setup", "--check", "--force"]);
    assert!(recheck.contains("disabled") || recheck.contains("禁用"));
    run(&["setup", "--skip"]);
}
