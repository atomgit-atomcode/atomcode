use std::process::Command;

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary
// runs, so tests never persist into the developer's real home. isolate_home is a
// inherited shell values are replaced before tests run.
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_atomcode")
}

#[test]
fn status_runs_without_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["telemetry", "status"])
        .env("ATOMCODE_TELEMETRY", "0")
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Telemetry: disabled"));
    assert!(s.contains("ATOMCODE_TELEMETRY=0"));
}

#[test]
fn clear_on_empty_queue_is_noop() {
    let d = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["telemetry", "clear"])
        .env("HOME", d.path())
        .output()
        .unwrap();
    assert!(out.status.success());
}
