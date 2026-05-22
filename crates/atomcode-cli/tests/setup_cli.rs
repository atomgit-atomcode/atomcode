//! Smoke test the `atomcode setup` subcommand via the built CLI binary.

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
    assert!(ok, "stdout did not contain a setup-success marker:\n{stdout}");
}
