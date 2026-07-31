//! Smoke test the `atomcode setup` subcommand via the built CLI binary.

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary
// runs, so tests never persist into the developer's real home. isolate_home is a
// no-op when the var is already set.
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
