use super::*;
use std::sync::Arc;

fn context(path: &std::path::Path) -> ToolContext {
    ToolContext {
        working_dir: path.into(),
        cancel: tokio_util::sync::CancellationToken::new(),
        progress: Default::default(),
        requester: None,
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn framing_checks_lengths_and_utf8_byte_counts() {
    let value = json!({"type":"event","event":"output","body":{"output":"中文"}});
    let bytes = value.to_string();
    let frame = format!("Content-Length: {}\r\n\r\n{}", bytes.len(), bytes);
    assert_eq!(
        client::read_frame(&mut frame.as_bytes()).await.unwrap(),
        value
    );
    for header in [
        "Content-Length: 1048577\r\n\r\n",
        "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
        "Other: 1\r\n\r\n",
        "Content-Length: 2\r\n\r\n[]",
    ] {
        let result = client::read_frame(&mut header.as_bytes()).await;
        if header.ends_with("[]") {
            assert!(result.is_ok());
        } else {
            assert!(result.is_err());
        }
    }
}

#[test]
fn debugging_never_claims_read_only_execution() {
    assert!(Settings::default().enabled);
    let tool = DebugTool::new(Settings::default());
    for action in [
        "launch",
        "evaluate",
        "variables",
        "continue",
        "terminate",
        "status",
    ] {
        assert_eq!(
            tool.risk(&json!({"action":action}).to_string()),
            RiskLevel::Risky
        );
    }
    assert!(!tool.read_only_hint());
    assert!(!tool.parallel_safe("{}"));
}

// External Python fixture needs real scheduler/process I/O. Avoid virtual-clock
// auto-advance racing process startup; timeout behavior is tested below directly.
#[cfg(unix)]
fn fixture(directory: &std::path::Path) -> DebugTool {
    let script = directory.join("adapter.py");
    std::fs::write(&script, include_str!("fixture.py")).unwrap();
    DebugTool::new(Settings {
        enabled: true,
        adapters: [(
            "fixture".into(),
            Adapter {
                command: "python3".into(),
                args: vec!["-u".into(), script.to_string_lossy().into_owned()],
            },
        )]
        .into(),
    })
}
async fn call(tool: &DebugTool, ctx: &ToolContext, args: Value) -> Value {
    let result = tool.execute(&args.to_string(), ctx).await;
    assert!(!result.is_error, "{}", result.content);
    serde_json::from_str(&result.content).unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn launch_inspect_step_and_terminate_rejects_stale_references() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let tool = fixture(dir.path());
    let launch = call(&tool, &ctx, json!({"action":"launch","adapter":"fixture","program":"test.py","breakpoints":[{"file":"test.py","line":2}]})).await;
    let id = launch["session"]["session_id"].clone();
    let wait = call(&tool, &ctx, json!({"action":"wait","session_id":id})).await;
    assert_eq!(wait["session"]["state"], "stopped");
    let stop = wait["session"]["stop_id"].clone();
    let stack = call(
        &tool,
        &ctx,
        json!({"action":"stack_trace","session_id":id,"stop_id":stop}),
    )
    .await;
    assert_eq!(stack["data"]["stackFrames"][0]["id"], 10);
    call(
        &tool,
        &ctx,
        json!({"action":"scopes","session_id":id,"stop_id":stop,"frame_id":10}),
    )
    .await;
    let vars = call(
        &tool,
        &ctx,
        json!({"action":"variables","session_id":id,"stop_id":stop,"variables_reference":20}),
    )
    .await;
    assert_eq!(vars["data"]["variables"][0]["value"], "42");
    assert_eq!(vars["data"]["variables"].as_array().unwrap().len(), 50);
    assert_eq!(vars["truncated"], true);
    call(&tool, &ctx, json!({"action":"evaluate","session_id":id,"stop_id":stop,"frame_id":10,"expression":"answer"})).await;
    call(
        &tool,
        &ctx,
        json!({"action":"step_over","session_id":id,"stop_id":stop}),
    )
    .await;
    let wait = call(&tool, &ctx, json!({"action":"wait","session_id":id})).await;
    assert_ne!(wait["session"]["stop_id"], stop);
    let stale = tool
        .execute(
            &json!({"action":"variables","session_id":id,"stop_id":stop,"variables_reference":20})
                .to_string(),
            &ctx,
        )
        .await;
    assert!(stale.is_error);
    assert_eq!(
        call(&tool, &ctx, json!({"action":"status"})).await["state"],
        "idle"
    );
    call(
        &tool,
        &ctx,
        json!({"action":"launch","adapter":"fixture","program":"test.py"}),
    )
    .await;
    // Old session id cannot terminate the replacement.
    assert!(
        tool.execute(
            &json!({"action":"terminate","session_id":id}).to_string(),
            &ctx
        )
        .await
        .is_error
    );
    let status = call(&tool, &ctx, json!({"action":"status"})).await;
    call(
        &tool,
        &ctx,
        json!({"action":"terminate","session_id":status["session"]["session_id"]}),
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_closes_the_owned_session() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let tool = Arc::new(fixture(dir.path()));
    let launch = call(
        &tool,
        &ctx,
        json!({"action":"launch","adapter":"fixture","program":"running.py"}),
    )
    .await;
    let id = launch["session"]["session_id"].clone();
    let cancel = ctx.cancel.clone();
    let running = tokio::spawn({
        let tool = tool.clone();
        async move {
            tool.execute(&json!({"action":"wait","session_id":id}).to_string(), &ctx)
                .await
        }
    });
    tokio::task::yield_now().await;
    cancel.cancel();
    assert!(running.await.unwrap().is_error);
    let result = call(&tool, &context(dir.path()), json!({"action":"status"})).await;
    assert_eq!(result["state"], "idle");
}

/// Opt-in real adapter test. Install debugpy in an isolated venv and set
/// ATOMCODE_TEST_DEBUGPY_PYTHON to its Python executable; no global dependency.
#[tokio::test]
#[ignore = "requires a real Python environment with debugpy and local socket permission"]
async fn real_debugpy_launch_breakpoint_variables_evaluate_step_continue() {
    let python =
        std::env::var("ATOMCODE_TEST_DEBUGPY_PYTHON").expect("set ATOMCODE_TEST_DEBUGPY_PYTHON");
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("target.py"), "def calculate_fee(balance):\n    fee = balance * 2\n    return fee\n\nresult = calculate_fee(21)\nprint(result)\n").unwrap();
    let ctx = context(dir.path());
    // Use the test environment as the project's .venv. No DAP settings or
    // adapter argument: this must exercise actual automatic selection.
    let venv = std::path::Path::new(&python)
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(venv, dir.path().join(".venv")).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(venv, dir.path().join(".venv")).unwrap();
    let tool = DebugTool::new(Settings::default());
    let status = call(&tool, &ctx, json!({"action":"status"})).await;
    assert!(status["discovery"]["python"]["interpreter"]
        .as_str()
        .unwrap()
        .contains(".venv"));
    let launch = call(&tool,&ctx,json!({"action":"launch","program":"target.py","breakpoints":[{"file":"target.py","line":3}],"timeout_secs":30})).await;
    assert_eq!(
        launch["data"]["breakpoints"][0]["result"]["breakpoints"][0]["verified"],
        true
    );
    let id = launch["session"]["session_id"].clone();
    let stopped = call(
        &tool,
        &ctx,
        json!({"action":"wait","session_id":id,"timeout_secs":30}),
    )
    .await;
    assert_eq!(stopped["session"]["state"], "stopped");
    let epoch = stopped["session"]["stop_id"].clone();
    let stack = call(
        &tool,
        &ctx,
        json!({"action":"stack_trace","session_id":id,"stop_id":epoch}),
    )
    .await;
    assert_eq!(stack["data"]["stackFrames"][0]["line"], 3);
    let frame = stack["data"]["stackFrames"][0]["id"].clone();
    let scopes = call(
        &tool,
        &ctx,
        json!({"action":"scopes","session_id":id,"stop_id":epoch,"frame_id":frame}),
    )
    .await;
    let locals = scopes["data"]["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|scope| scope["name"] == "Locals")
        .unwrap()["variablesReference"]
        .clone();
    let variables = call(
        &tool,
        &ctx,
        json!({"action":"variables","session_id":id,"stop_id":epoch,"variables_reference":locals}),
    )
    .await;
    assert!(variables["data"]["variables"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["name"] == "fee" && v["value"] == "42"));
    let evaluation = call(&tool,&ctx,json!({"action":"evaluate","session_id":id,"stop_id":epoch,"frame_id":frame,"expression":"fee"})).await;
    assert_eq!(evaluation["data"]["result"], "42");
    call(
        &tool,
        &ctx,
        json!({"action":"step_over","session_id":id,"stop_id":epoch}),
    )
    .await;
    let stepped = call(&tool, &ctx, json!({"action":"wait","session_id":id})).await;
    assert_eq!(stepped["session"]["state"], "stopped");
    assert_ne!(stepped["session"]["stop_id"], epoch);
    call(
        &tool,
        &ctx,
        json!({"action":"continue","session_id":id,"stop_id":stepped["session"]["stop_id"]}),
    )
    .await;
    let ended = call(&tool, &ctx, json!({"action":"wait","session_id":id})).await;
    assert_eq!(ended["session"]["state"], "terminated");
    call(&tool, &ctx, json!({"action":"terminate","session_id":id})).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn timeout_closes_session_without_leaving_a_reusable_handle() {
    // Startup depends on an OS child. Run that part in real time, then pause
    // once the idle peer is ready; only the tool timeout is under test.
    tokio::time::resume();
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let tool = fixture(dir.path());
    let launch = call(
        &tool,
        &ctx,
        json!({"action":"launch","adapter":"fixture","program":"running.py"}),
    )
    .await;
    tokio::time::pause();
    let result = tool
        .execute(
            &json!({"action":"wait","session_id":launch["session"]["session_id"],"timeout_secs":1})
                .to_string(),
            &ctx,
        )
        .await;
    assert!(result.is_error);
    assert!(result.content.contains("timeout"));
    assert_eq!(
        call(&tool, &ctx, json!({"action":"status"})).await["state"],
        "idle"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reverse_launch_and_adapter_failure_do_not_leave_live_sessions() {
    for program in ["reverse.py", "failure.py"] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = fixture(dir.path());
        let result = tool
            .execute(
                &json!({"action":"launch","adapter":"fixture","program":program}).to_string(),
                &ctx,
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        assert_eq!(
            call(&tool, &ctx, json!({"action":"status"})).await["state"],
            "idle"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_tool_kills_adapter_and_its_child_process() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = context(dir.path());
    let tool = fixture(dir.path());
    call(
        &tool,
        &ctx,
        json!({"action":"launch","adapter":"fixture","program":"child.py"}),
    )
    .await;
    let pids: Vec<i32> = std::fs::read_to_string(dir.path().join("pids"))
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();
    drop(tool);
    for pid in pids {
        // Real process reaping by Tokio/launchd cannot use virtual time.
        let limit = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < limit,
                "debug process {pid} survived Drop"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn discovery_respects_explicit_overrides_and_gives_actionable_unsupported_target_errors() {
    let settings = Settings {
        enabled: true,
        adapters: [(
            "python".into(),
            Adapter {
                command: "explicit-python".into(),
                args: vec!["-m".into(), "debugpy.adapter".into()],
            },
        )]
        .into(),
    };
    let (id, adapter) = discovery::resolve(
        &settings,
        None,
        "target.py",
        std::path::Path::new("/project"),
    )
    .await
    .unwrap();
    assert_eq!(id, "python");
    assert_eq!(adapter.command, "explicit-python");
    assert!(discovery::resolve(
        &settings,
        Some("missing"),
        "target.py",
        std::path::Path::new("/project")
    )
    .await
    .unwrap_err()
    .contains("not configured"));
    assert!(discovery::resolve(
        &settings,
        None,
        "src/main.rs",
        std::path::Path::new("/project")
    )
    .await
    .unwrap_err()
    .contains("compiled executable"));
    let error = discovery::resolve(
        &settings,
        None,
        "target.js",
        std::path::Path::new("/project"),
    )
    .await
    .unwrap_err();
    assert!(error.contains("No built-in DAP adapter"));
}

#[cfg(unix)]
#[tokio::test]
async fn missing_project_debugpy_reports_install_hint_without_switching_interpreters() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join(".venv/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let python = bin.join("python");
    // A selected interpreter without debugpy. It must not fall back to a
    // different global Python, which could change the target's dependencies.
    std::fs::write(&python, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    let tool = DebugTool::new(Settings::default());
    let result = tool
        .execute(
            &json!({"action":"launch","program":"test.py"}).to_string(),
            &context(dir.path()),
        )
        .await;
    assert!(result.is_error);
    assert!(result.content.contains(python.to_str().unwrap()));
    assert!(result.content.contains("-m pip install debugpy"));
    assert!(result
        .content
        .contains("No package was installed automatically"));
    assert_eq!(
        call(&tool, &context(dir.path()), json!({"action":"status"})).await["state"],
        "idle"
    );
}
