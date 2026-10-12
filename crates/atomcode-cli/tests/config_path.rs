//! Explicit configuration errors must precede provider startup on headless and ACP entry points.
#[test]
fn missing_explicit_config_is_reported_by_headless_and_acp() {
    for acp in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        let mut command = assert_cmd::Command::new(env!("CARGO_BIN_EXE_atomcode"));
        command
            .env("ATOMCODE_HOME", dir.path().join("state"))
            .env_remove("ATOMCODE_SEED_CONFIG")
            .arg("--config")
            .arg(&missing)
            .timeout(std::time::Duration::from_secs(10));
        if acp {
            command.arg("acp");
        } else {
            command.args(["-p", "hi"]);
        }
        let output = command.assert().code(1).get_output().clone();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("config file not found: {}", missing.display())),
            "{stderr}"
        );
        assert!(!stderr.contains("relative URL without a base"), "{stderr}");
        assert!(
            output.stdout.is_empty(),
            "a startup error must not enter the ACP protocol"
        );
    }
}
