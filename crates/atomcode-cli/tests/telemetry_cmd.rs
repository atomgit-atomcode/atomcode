use std::process::Command;

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
