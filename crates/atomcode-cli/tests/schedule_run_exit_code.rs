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
    std::fs::write(sched_dir.join("smoke-exit-code.json"), &task_json).unwrap();

    Command::cargo_bin("atomcode")
        .unwrap()
        .args(["schedule", "run", "smoke-exit-code"])
        .env("ATOMCODE_HOME", home.path())
        .assert()
        .code(1);
    let store = atomcode_schedule::Store::new(&sched_dir);
    let events = store.history("smoke-exit-code").unwrap();
    assert!(
        matches!(events.last().unwrap(),atomcode_schedule::RunEvent::Finished {status,error,..} if status=="error" && error.as_deref().unwrap_or("").contains("working directory"))
    );
    assert_eq!(
        std::fs::read_to_string(sched_dir.join("smoke-exit-code.json")).unwrap(),
        task_json,
        "completion must not rewrite the definition"
    );
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

#[test]
fn a_correlated_manual_run_is_not_executed_twice_after_failure() {
    let home = tempfile::tempdir().unwrap();
    let store = atomcode_schedule::Store::new(home.path().join("schedules"));
    let task=serde_json::from_value(serde_json::json!({"id":"request-test","title":"Test","prompt":"Review","cwd":home.path().join("missing"),"schedule":{"kind":"hourly"}})).unwrap();
    let definition = store.put(&task, None).unwrap();
    for _ in 0..2 {
        Command::cargo_bin("atomcode")
            .unwrap()
            .args([
                "schedule",
                "run",
                "request-test",
                "--request-id",
                "request-1",
                "--expected-revision",
                &definition.revision,
            ])
            .env("ATOMCODE_HOME", home.path())
            .assert()
            .code(1);
    }
    let events = store.history("request-test").unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, atomcode_schedule::RunEvent::Started { .. }))
            .count(),
        1
    );
}

#[test]
fn os_style_launch_uses_its_saved_store_without_inheriting_terminal_environment() {
    let unrelated = tempfile::tempdir().unwrap();
    let product = tempfile::tempdir().unwrap();
    let root = product.path().join("schedules");
    let store = atomcode_schedule::Store::new(&root);
    let task=serde_json::from_value(serde_json::json!({"id":"custom-home","title":"Custom","prompt":"Review","cwd":product.path().join("missing"),"schedule":{"kind":"hourly"}})).unwrap();
    store.put(&task, None).unwrap();
    Command::cargo_bin("atomcode")
        .unwrap()
        .args(["schedule", "run", "custom-home", "--store"])
        .arg(&root)
        .env("ATOMCODE_HOME", unrelated.path())
        .assert()
        .code(1);
    assert_eq!(store.history("custom-home").unwrap().len(), 2);
    assert!(!unrelated.path().join("schedules").exists());
}

#[test]
fn malformed_configuration_is_a_recorded_run_failure_not_an_untracked_startup_exit() {
    let home = tempfile::tempdir().unwrap();
    let store = atomcode_schedule::Store::new(home.path().join("schedules"));
    let task=serde_json::from_value(serde_json::json!({"id":"bad-config","title":"Bad config","prompt":"Review","cwd":home.path(),"schedule":{"kind":"hourly"}})).unwrap();
    store.put(&task, None).unwrap();
    std::fs::write(home.path().join("config.toml"), "not valid TOML !!!").unwrap();
    Command::cargo_bin("atomcode")
        .unwrap()
        .args(["schedule", "run", "bad-config"])
        .env("ATOMCODE_HOME", home.path())
        .assert()
        .code(1);
    let events = store.history("bad-config").unwrap();
    assert!(
        matches!(events.last().unwrap(),atomcode_schedule::RunEvent::Finished {status,error,..} if status=="error" && error.as_deref().unwrap_or("").contains("configuration"))
    );
}

#[test]
fn editing_between_admission_and_execution_is_rejected_with_a_terminal_record() {
    let home = tempfile::tempdir().unwrap();
    let store = atomcode_schedule::Store::new(home.path().join("schedules"));
    let mut task: atomcode_config::schedule::ScheduleTask=serde_json::from_value(serde_json::json!({"id":"revision-race","title":"Race","prompt":"Original","cwd":home.path(),"schedule":{"kind":"hourly"}})).unwrap();
    let first = store.put(&task, None).unwrap();
    task.prompt = "Replacement".into();
    store.put(&task, Some(&first.revision)).unwrap();
    Command::cargo_bin("atomcode")
        .unwrap()
        .args([
            "schedule",
            "run",
            "revision-race",
            "--request-id",
            "raced-request",
            "--expected-revision",
            &first.revision,
        ])
        .env("ATOMCODE_HOME", home.path())
        .assert()
        .code(1);
    let events = store.history("revision-race").unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0],atomcode_schedule::RunEvent::Finished {status,..} if status=="rejected")
    );
}

#[test]
fn an_unconfigured_provider_is_visible_in_the_run_ledger() {
    let home = tempfile::tempdir().unwrap();
    let store = atomcode_schedule::Store::new(home.path().join("schedules"));
    let task=serde_json::from_value(serde_json::json!({"id":"no-provider","title":"No provider","prompt":"Review","cwd":home.path(),"schedule":{"kind":"hourly"}})).unwrap();
    store.put(&task, None).unwrap();
    // No config/accounts exist in this isolated home, so provider resolution
    // fails before any model request. This never contacts a real provider.
    Command::cargo_bin("atomcode")
        .unwrap()
        .args(["schedule", "run", "no-provider"])
        .env("ATOMCODE_HOME", home.path())
        .timeout(std::time::Duration::from_secs(15))
        .assert()
        .code(1);
    let events = store.history("no-provider").unwrap();
    assert!(
        matches!(events.last().unwrap(),atomcode_schedule::RunEvent::Finished {status,error,..} if status=="error" && error.is_some())
    );
}
