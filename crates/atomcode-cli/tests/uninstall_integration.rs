use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary
// runs, so tests never persist into the developer's real home. isolate_home is a
// inherited shell values are replaced before tests run.
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

/// Build a fake atomcode data dir so the CLI sees something to scan.
///
/// Under unified semantics, `ATOMCODE_HOME` IS the data root (equivalent to
/// `~/.atomcode/`), so we point the env var directly at this dir — no extra
/// `.atomcode` subdir.
fn make_fake_data(tmp: &TempDir) -> std::path::PathBuf {
    let data = tmp.path().join("atomcode-data");
    fs::create_dir(&data).unwrap();
    fs::write(data.join("auth.toml"), b"k=1").unwrap();
    fs::write(data.join("config.toml"), b"x=1").unwrap();
    fs::write(data.join("history"), b"hi").unwrap();
    fs::create_dir(data.join("plugins")).unwrap();
    data
}

#[test]
fn dry_run_makes_no_changes() {
    let tmp = TempDir::new().unwrap();
    let data = make_fake_data(&tmp);
    Command::cargo_bin("atomcode")
        .unwrap()
        .env("ATOMCODE_HOME", &data)
        .args(["uninstall", "--dry-run"])
        .assert()
        .success()
        .stdout(predicates::str::contains("DRY RUN"));
    // All files still present.
    assert!(data.join("auth.toml").exists());
    assert!(data.join("history").exists());
    assert!(data.join("plugins").exists());
}

#[test]
fn no_tty_no_flag_exits_2() {
    let tmp = TempDir::new().unwrap();
    let data = make_fake_data(&tmp);
    Command::cargo_bin("atomcode")
        .unwrap()
        .env("ATOMCODE_HOME", &data)
        .arg("uninstall")
        .write_stdin("")
        .assert()
        .failure()
        .code(2)
        .stderr(predicates::str::contains("refusing to run interactively"));
    // Nothing touched.
    assert!(data.join("auth.toml").exists());
}

#[test]
fn purge_and_keep_data_conflict_exit_2() {
    let tmp = TempDir::new().unwrap();
    let data = make_fake_data(&tmp);
    Command::cargo_bin("atomcode")
        .unwrap()
        .env("ATOMCODE_HOME", &data)
        .args(["uninstall", "--purge", "--keep-data"])
        .assert()
        .failure()
        .code(2);
}

// NOTE: We intentionally don't run --purge / --keep-data as full integration tests
// against a real install — the binary path resolves to the test runner exe (cargo's
// target/debug/atomcode), and deleting it would break subsequent tests in the same
// run. The lower-level deletion paths are covered by the CLI uninstall action tests
// with a no-op self-delete implementation.
