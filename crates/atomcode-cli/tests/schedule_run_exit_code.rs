//! `atomcode schedule run <id>` must surface a non-zero exit code so the OS
//! scheduler (launchd/systemd/schtasks) can detect a failed run. Regression test
//! for the wiring that collapsed the executor's `Result<i32>` to process exit 0.

use assert_cmd::Command;

#[test]
fn schedule_run_propagates_nonzero_exit_when_working_dir_missing() {
    // Isolate the schedule store under a throwaway ATOMCODE_HOME.
    let home = tempfile::tempdir().unwrap();
    let sched_dir = home.path().join("schedules");
    std::fs::create_dir_all(&sched_dir).unwrap();

    // A task whose working directory does not exist: run_task returns Ok(1) at the
    // cwd check, before any provider/network bootstrap, so this is fast and offline.
    let missing_cwd = home.path().join("does-not-exist");
    let task_json = format!(
        r#"{{
            "id": "smoke-exit-code",
            "title": "smoke",
            "prompt": "noop",
            "cwd": {cwd},
            "schedule": {{ "kind": "daily", "time": "09:00" }},
            "enabled": true
        }}"#,
        cwd = serde_json_string(&missing_cwd.display().to_string()),
    );
    std::fs::write(sched_dir.join("smoke-exit-code.json"), task_json).unwrap();

    Command::cargo_bin("atomcode")
        .unwrap()
        .args(["schedule", "run", "smoke-exit-code"])
        .env("ATOMCODE_HOME", home.path())
        .assert()
        .code(1);
}

/// Minimal JSON string encoder so the test needs no serde dependency: quote and
/// escape the handful of characters that can appear in a filesystem path.
fn serde_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}
