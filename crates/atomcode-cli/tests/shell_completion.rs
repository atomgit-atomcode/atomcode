use assert_cmd::Command;

#[test]
fn completion_exits_before_startup_side_effects() {
    let atomcode_home = tempfile::tempdir().unwrap();

    Command::cargo_bin("atomcode")
        .unwrap()
        .env("ATOMCODE_HOME", atomcode_home.path())
        .arg("completion")
        .arg("bash")
        .assert()
        .success()
        .stderr("")
        .stdout(predicates::str::contains("atomcode"));

    assert!(
        atomcode_home.path().read_dir().unwrap().next().is_none(),
        "completion generation must not create logs, config, or telemetry state"
    );
}
