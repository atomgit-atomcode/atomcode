//! Opt-in real backend acceptance. Uses a local page and a fresh isolated profile only.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use atomcode::browser_cmd::{self, BrowserCli};
use atomcode_capabilities::{
    mcp::{McpRegistry, McpToolAdapter},
    ProductDirs,
};
use atomcode_kernel::tool::{ProgressSink, RiskLevel, Tool, ToolContext, ToolResult};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn invoke(
    registry: &Arc<McpRegistry>,
    name: &str,
    args: Value,
    ctx: &ToolContext,
) -> ToolResult {
    let info = registry
        .listed_tools()
        .into_iter()
        .find(|t| t.tool_name == name)
        .expect("required browser tool");
    let tool = McpToolAdapter::new(registry.clone(), info).unwrap();
    let result = tool.execute(&args.to_string(), ctx).await;
    assert!(!result.is_error, "{name}: {}", result.content);
    result
}

fn element_ref(snapshot: &str, label: &str) -> String {
    let line = snapshot
        .lines()
        .find(|l| l.contains(label) && l.contains("[ref="))
        .expect("element in current snapshot");
    line.split("[ref=")
        .nth(1)
        .unwrap()
        .split(']')
        .next()
        .unwrap()
        .into()
}

#[tokio::test]
#[ignore = "requires an explicitly installed @playwright/mcp 0.0.85 and Chrome; local acceptance only"]
async fn real_browser_navigates_fills_submits_and_returns_inline_screenshot() {
    acceptance(false, false, false).await;
}

#[tokio::test]
#[ignore = "requires an explicitly installed browser backend and Chrome; fault injection acceptance"]
async fn real_submitted_browser_click_is_not_replayed_after_its_response_is_lost() {
    acceptance(true, false, false).await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires installed browser backend and Chrome; verifies owned process disposal"]
async fn real_browser_registry_drop_disposes_its_owned_browser_processes() {
    acceptance(false, true, false).await;
}

#[cfg(unix)]
async fn owned_process_tree(root: u32) -> Vec<u32> {
    let output = tokio::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
    // Inspect only the exact launched backend's tree; never kill by name or
    // emit the user's unrelated process command lines into test output.
    let text = String::from_utf8_lossy(&output.stdout);
    let rows: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((
                fields.next()?.parse::<u32>().ok()?,
                fields.next()?.parse::<u32>().ok()?,
                line,
            ))
        })
        .collect();
    let mut owned = vec![root];
    loop {
        let children: Vec<_> = rows
            .iter()
            .filter(|(pid, parent, _)| owned.contains(parent) && !owned.contains(pid))
            .map(|(pid, _, _)| *pid)
            .collect();
        if children.is_empty() {
            break;
        }
        owned.extend(children);
    }
    assert!(
        rows.iter().any(|(pid, _, line)| owned.contains(pid)
            && (line.contains("Google Chrome") || line.contains("chromium"))),
        "the exact owned backend has a live browser child"
    );
    owned
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires installed browser backend and Chrome; awaited shutdown acceptance"]
async fn real_browser_shutdown_disposes_before_return_and_fences_stale_adapters() {
    acceptance(false, true, true).await;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    LostReply,
    Cancel,
    ForcedDrop,
}

async fn acceptance(drop_response: bool, verify_disposal: bool, await_shutdown: bool) {
    acceptance_backend(
        false,
        if drop_response {
            Fault::LostReply
        } else {
            Fault::None
        },
        verify_disposal,
        await_shutdown,
    )
    .await;
}

async fn acceptance_backend(
    scoped: bool,
    fault: Fault,
    verify_disposal: bool,
    await_shutdown: bool,
) {
    let drop_response = fault == Fault::LostReply;
    let cancellation = matches!(fault, Fault::Cancel | Fault::ForcedDrop);
    let temp = tempfile::tempdir().unwrap();
    let dirs = ProductDirs::new(temp.path().join("user"), ".test-product");
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let scope = if scoped {
        browser_cmd::OriginArgs {
            origin_backend: Some(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../integrations/browser-origin-scope/backend.cjs")
                    .canonicalize()
                    .unwrap(),
            ),
            backend_root: Some(
                std::env::var_os("ATOMCODE_BROWSER_ORIGIN_TEST_ROOT")
                    .expect("explicit installed package root")
                    .into(),
            ),
            allowed_origins: vec![format!("http://{address}")],
            ..Default::default()
        }
    } else {
        Default::default()
    };
    let server = if scoped {
        None
    } else {
        Some(
            std::env::var_os("ATOMCODE_BROWSER_TEST_SERVER")
                .expect("explicit installed cli.js")
                .into(),
        )
    };
    browser_cmd::execute(
        &dirs,
        BrowserCli::Enable {
            server,
            node: None,
            scope,
        },
    )
    .await
    .unwrap();
    // Only acceptance is headless; user setup keeps the normal visible browser default.
    let config = dirs.user().join("mcp.json");
    let mut root: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    root["mcpServers"]["browser-use"]["env"]["PLAYWRIGHT_MCP_HEADLESS"] = json!("true");
    let backend_pid_file = temp.path().join("backend.pid");
    if verify_disposal && !drop_response && !cancellation {
        let preloader = temp.path().join("record-pid.cjs");
        std::fs::write(
            &preloader,
            format!(
                "require('node:fs').writeFileSync({},String(process.pid));",
                json!(backend_pid_file)
            ),
        )
        .unwrap();
        let args = root["mcpServers"]["browser-use"]["args"]
            .as_array_mut()
            .unwrap();
        args.splice(0..0, [json!("--require"), json!(preloader)]);
    }
    let calls = temp.path().join("click-calls");
    let spawns = temp.path().join("backend-spawns");
    let late = temp.path().join("late-click-result");
    if drop_response || cancellation {
        let wrapper = temp.path().join("wire-fixture.cjs");
        let server = root["mcpServers"]["browser-use"]["args"][0]
            .as_str()
            .unwrap()
            .to_string();
        std::fs::write(&wrapper, include_str!("fixtures/browser-fault-proxy.cjs")).unwrap();
        root["mcpServers"]["browser-use"]["args"][0] = json!(wrapper);
        root["mcpServers"]["browser-use"]["env"]["ATOMCODE_BROWSER_FAULT_FIXTURE"] = json!(json!({
            "server":server,"pid":backend_pid_file,"spawns":spawns,"calls":calls,"late":late,
            "marker":temp.path().join("dropped-once"),"drop":drop_response
        })
        .to_string());
    }
    std::fs::write(&config, root.to_string()).unwrap();

    let submissions = Arc::new(AtomicUsize::new(0));
    let counter = submissions.clone();
    let committed = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let effect = committed.clone();
    let response_gate = release.clone();
    let web = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let counter = counter.clone();
            let effect = effect.clone();
            let response_gate = response_gate.clone();
            tokio::spawn(async move {
                let mut bytes = vec![0; 8192];
                let n = stream.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]);
                let body = if request.starts_with("GET /submit?") {
                    let target = request
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap();
                    let url = reqwest::Url::parse(&format!("http://fixture{target}")).unwrap();
                    assert_eq!(
                        url.query_pairs()
                            .find(|(name, _)| name == "name")
                            .map(|(_, value)| value.into_owned())
                            .as_deref(),
                        Some("ATOMCODE_BROWSER_OK")
                    );
                    let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    effect.notify_one();
                    if cancellation {
                        response_gate.notified().await;
                    }
                    format!("<html><body><h1>Saved acceptance record</h1><p>submission-count={count}</p><p>ATOMCODE_BROWSER_OK</p></body></html>")
                } else if request.starts_with("GET /record ") {
                    let count = counter.load(Ordering::SeqCst);
                    format!("<html><body><h1>Saved acceptance record</h1><p>submission-count={count}</p><p>ATOMCODE_BROWSER_OK</p></body></html>")
                } else {
                    "<html><body><h1>AtomCode controlled browser acceptance</h1><form action='/submit'><label>Name<input name='name'></label><button type='submit'>Save acceptance record</button></form></body></html>".into()
                };
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    let registry = McpRegistry::from_config(&project, dirs.user())
        .await
        .share();
    let tools = registry.list_all_tools().await;
    assert!(!tools.is_empty());
    // Real Node process time must pass: Tokio's paused clock cannot advance
    // the external backend's 5s heartbeat timer. Represents a person waiting
    // before approving the first action.
    tokio::time::sleep(std::time::Duration::from_secs(7)).await;
    for forbidden in [
        "browser_evaluate",
        "browser_run_code_unsafe",
        "browser_file_upload",
        "browser_install",
    ] {
        assert!(!tools.iter().any(|t| t.tool_name == forbidden));
        assert!(registry
            .call_tool("browser-use", forbidden, json!({}))
            .await
            .is_err());
    }
    for tool in &tools {
        if matches!(
            tool.tool_name.as_str(),
            "browser_click" | "browser_fill_form" | "browser_navigate"
        ) {
            assert_eq!(
                McpToolAdapter::new(registry.clone(), tool.clone())
                    .unwrap()
                    .risk("{}"),
                RiskLevel::Risky
            );
        }
    }
    let ctx = ToolContext {
        working_dir: project,
        cancel: Default::default(),
        progress: ProgressSink::noop(),
        requester: None,
    };
    invoke(
        &registry,
        "browser_navigate",
        json!({"url":format!("http://{address}/")}),
        &ctx,
    )
    .await;
    let page = invoke(&registry, "browser_snapshot", json!({}), &ctx).await;
    assert!(
        page.content
            .contains("AtomCode controlled browser acceptance"),
        "navigation observation: {}",
        page.content
    );
    let name_ref = element_ref(&page.content, "textbox \"Name\"");
    invoke(&registry, "browser_fill_form", json!({"fields":[{"target":name_ref,"name":"Name","type":"textbox","value":"ATOMCODE_BROWSER_OK"}]}), &ctx).await;
    let fresh = invoke(&registry, "browser_snapshot", json!({}), &ctx).await;
    assert!(fresh.content.contains("ATOMCODE_BROWSER_OK"));
    let submit_ref = element_ref(&fresh.content, "button \"Save acceptance record\"");
    let click_info = registry
        .listed_tools()
        .into_iter()
        .find(|t| t.tool_name == "browser_click")
        .unwrap();
    let click = Arc::new(McpToolAdapter::new(registry.clone(), click_info).unwrap());
    if cancellation {
        cancellation_acceptance(
            fault,
            &registry,
            &click,
            &ctx,
            CancellationFixture {
                config: &config,
                pid: &backend_pid_file,
                calls: &calls,
                spawns: &spawns,
                late: &late,
                submissions: submissions.clone(),
                committed,
                release,
                address,
            },
        )
        .await;
        web.abort();
        let _ = web.await;
        return;
    }
    let result = click
        .execute(&json!({"target":submit_ref}).to_string(), &ctx)
        .await;
    if drop_response {
        assert!(
            result.is_error && result.content.contains("result is unknown"),
            "{}",
            result.content
        );
        assert!(result.content.contains("not replayed"));
        assert_eq!(submissions.load(Ordering::SeqCst), 1);
        // Reconcile the actual external record after recovery; no submit replay.
        invoke(
            &registry,
            "browser_navigate",
            json!({"url":format!("http://{address}/record")}),
            &ctx,
        )
        .await;
        let observed = invoke(&registry, "browser_snapshot", json!({}), &ctx).await;
        assert!(observed.content.contains("submission-count=1"));
        assert_eq!(line_count(&calls), 1);
        assert_eq!(submissions.load(Ordering::SeqCst), 1);
        invoke(&registry, "browser_close", json!({}), &ctx).await;
        registry.shutdown().await;
        save_case(
            scoped,
            fault,
            verify_disposal,
            await_shutdown,
            json!({"passed":true,"submission_count":1,"click_rpc_count":line_count(&calls),"backend_spawns":line_count(&spawns),"successful_reply_lost":true,"mutation_replayed":false}),
        );
        web.abort();
        let _ = web.await;
        return;
    }
    assert!(!result.is_error, "{}", result.content);
    let fresh = invoke(&registry, "browser_snapshot", json!({}), &ctx).await;
    assert!(fresh.content.contains("Saved acceptance record"));
    assert!(fresh.content.contains("submission-count=1"));
    assert_eq!(submissions.load(Ordering::SeqCst), 1);
    let screenshot = invoke(
        &registry,
        "browser_take_screenshot",
        json!({"type":"png","scale":"css"}),
        &ctx,
    )
    .await;
    assert_eq!(
        screenshot.images.len(),
        1,
        "actual backend image passes through AtomCode adapter"
    );
    assert_eq!(screenshot.images[0].media_type, "image/png");
    if let Some(dir) = std::env::var_os("ATOMCODE_BROWSER_TEST_ARTIFACTS") {
        use base64::Engine;
        let dir = std::path::PathBuf::from(dir).join(case_name(
            scoped,
            fault,
            verify_disposal,
            await_shutdown,
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pixels = base64::engine::general_purpose::STANDARD
            .decode(&screenshot.images[0].data)
            .unwrap();
        std::fs::write(dir.join("accepted-record.png"), pixels).unwrap();
        std::fs::write(dir.join("accepted-record.json"), json!({"backend":if scoped {"origin-scope-v1"} else {"@playwright/mcp 0.0.85"},"submission_count":submissions.load(Ordering::SeqCst),"inline_image_count":screenshot.images.len(),"final_snapshot":fresh.content}).to_string()).unwrap();
    }
    if verify_disposal {
        #[cfg(unix)]
        {
            let pid: u32 = std::fs::read_to_string(backend_pid_file)
                .unwrap()
                .parse()
                .unwrap();
            let owned = owned_process_tree(pid).await;
            if await_shutdown {
                registry.shutdown().await;
                let stale = click
                    .execute(&json!({"target":submit_ref}).to_string(), &ctx)
                    .await;
                assert!(stale.is_error && stale.content.contains("shut down"));
            }
            drop(click); // The adapter also holds the registry.
            drop(registry);
            let began = tokio::time::Instant::now();
            loop {
                let alive = live_pids(&owned);
                if alive.is_empty() {
                    break;
                }
                assert!(
                    !await_shutdown,
                    "observed owned processes alive after awaited shutdown: {alive:?}"
                );
                assert!(
                    began.elapsed() < std::time::Duration::from_secs(5),
                    "observed owned processes survived bounded Drop cleanup: {alive:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    } else {
        invoke(&registry, "browser_close", json!({}), &ctx).await;
        registry.shutdown().await;
    }
    save_case(
        scoped,
        fault,
        verify_disposal,
        await_shutdown,
        json!({"passed":true,"submission_count":submissions.load(Ordering::SeqCst),"inline_image_count":screenshot.images.len(),"observed_process_disposal_verified":verify_disposal,"awaited_shutdown":await_shutdown}),
    );
    web.abort();
    let _ = web.await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "requires ATOMCODE_BROWSER_ORIGIN_TEST_ROOT, pinned backend and Chrome; managed form/image acceptance"]
async fn real_scoped_browser_navigates_fills_submits_and_returns_inline_screenshot() {
    tokio::time::resume();
    acceptance_backend(true, Fault::None, false, false).await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "requires ATOMCODE_BROWSER_ORIGIN_TEST_ROOT and Chrome; lost successful response without replay"]
async fn real_scoped_submitted_click_is_not_replayed_after_response_loss() {
    tokio::time::resume();
    acceptance_backend(true, Fault::LostReply, false, false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "requires pinned scoped browser and Chrome; observed owned-process Drop cleanup"]
async fn real_scoped_browser_registry_drop_disposes_owned_processes() {
    tokio::time::resume();
    acceptance_backend(true, Fault::None, true, false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "requires pinned scoped browser and Chrome; awaited shutdown barrier"]
async fn real_scoped_browser_shutdown_disposes_before_return_and_fences_stale_adapter() {
    tokio::time::resume();
    acceptance_backend(true, Fault::None, true, true).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "requires pinned scoped browser and Chrome; committed effect then active cancellation"]
async fn real_scoped_cancel_after_submission_requires_explicit_reconnect_without_replay() {
    tokio::time::resume();
    acceptance_backend(true, Fault::Cancel, false, false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "requires pinned scoped browser and Chrome; committed effect then forced future drop"]
async fn real_scoped_dropped_call_after_submission_rejects_late_success_without_replay() {
    tokio::time::resume();
    acceptance_backend(true, Fault::ForcedDrop, false, false).await;
}

fn line_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .count()
}

#[cfg(unix)]
fn live_pids(owned: &[u32]) -> Vec<u32> {
    owned
        .iter()
        .copied()
        .filter(|pid| {
            // Signal 0 checks the exact captured PID synchronously; it sends no signal.
            if unsafe { libc::kill(*pid as libc::pid_t, 0) } == 0 {
                return true;
            }
            let error = std::io::Error::last_os_error();
            assert_eq!(
                error.raw_os_error(),
                Some(libc::ESRCH),
                "PID check failed: {error}"
            );
            false
        })
        .collect()
}

struct CancellationFixture<'a> {
    config: &'a std::path::Path,
    pid: &'a std::path::Path,
    calls: &'a std::path::Path,
    spawns: &'a std::path::Path,
    late: &'a std::path::Path,
    submissions: Arc<AtomicUsize>,
    committed: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    address: std::net::SocketAddr,
}

async fn cancellation_acceptance(
    fault: Fault,
    registry: &Arc<McpRegistry>,
    click: &Arc<McpToolAdapter>,
    context: &ToolContext,
    fixture: CancellationFixture<'_>,
) {
    let cancel = context.cancel.child_token();
    let call_context = ToolContext {
        working_dir: context.working_dir.clone(),
        cancel: cancel.clone(),
        progress: ProgressSink::noop(),
        requester: None,
    };
    let tool = click.clone();
    let snapshot = invoke(registry, "browser_snapshot", json!({}), context).await;
    let target = element_ref(&snapshot.content, "button \"Save acceptance record\"");
    let arguments = json!({"target":target}).to_string();
    let task = tokio::spawn(async move { tool.execute(&arguments, &call_context).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let observed = fixture.committed.notified();
            if fixture.submissions.load(Ordering::SeqCst) == 1 {
                break;
            }
            observed.await;
        }
    })
    .await
    .expect("actual server recorded the submitted effect before cancellation");
    assert!(
        !task.is_finished(),
        "the response remains withheld while the request is in flight"
    );
    #[cfg(unix)]
    let owned = owned_process_tree(
        std::fs::read_to_string(fixture.pid)
            .unwrap()
            .parse()
            .unwrap(),
    )
    .await;
    if fault == Fault::ForcedDrop {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    } else {
        cancel.cancel();
        let result = task.await.unwrap();
        assert!(
            result.is_error && result.content.contains("unverified"),
            "{}",
            result.content
        );
        assert!(result.content.contains("do not replay"));
    }
    assert!(registry.server_statuses().await.iter().any(|(name,status)|name=="browser-use" && matches!(status,
        atomcode_capabilities::mcp::ServerStatus::Failed(message) if message.contains("cancelled after dispatch"))));
    let blocked = click
        .execute(&json!({"target":target}).to_string(), context)
        .await;
    assert!(
        blocked.is_error && blocked.content.contains("Reconnect"),
        "{}",
        blocked.content
    );
    assert!(registry
        .list_tools_for_server("browser-use")
        .await
        .is_empty());
    assert_eq!(fixture.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(line_count(fixture.calls), 1);
    assert_eq!(line_count(fixture.spawns), 1);
    fixture.release.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !fixture.late.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("wire proxy observed the actual late MCP result");
    let late: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture.late).unwrap()).unwrap();
    assert_eq!(
        late["is_error"], false,
        "a late successful remote result cannot restore the abandoned client"
    );
    assert!(
        click
            .execute(&json!({"target":target}).to_string(), context)
            .await
            .is_error
    );
    assert_eq!(line_count(fixture.calls), 1);
    assert_eq!(line_count(fixture.spawns), 1);
    let config = atomcode_capabilities::mcp::parse_mcp_servers(
        &std::fs::read_to_string(fixture.config).unwrap(),
        atomcode_capabilities::mcp::McpConfigSource::User,
    )
    .unwrap()
    .remove(0);
    registry.reconnect_server(config).await.unwrap();
    #[cfg(unix)]
    assert!(
        live_pids(&owned).is_empty(),
        "explicit reconnect awaited disposal of the old observed process tree"
    );
    assert_eq!(
        line_count(fixture.spawns),
        2,
        "one explicit replacement, no automatic replay"
    );
    // Explicit replacement is followed by the same discovery/remount step a
    // driver performs; the cancelled connection's tool cache was withdrawn.
    assert_eq!(registry.list_all_tools().await.len(), 16);
    invoke(
        registry,
        "browser_navigate",
        json!({"url":format!("http://{}/record",fixture.address)}),
        context,
    )
    .await;
    let observed = invoke(registry, "browser_snapshot", json!({}), context).await;
    assert!(observed.content.contains("submission-count=1"));
    assert_eq!(fixture.submissions.load(Ordering::SeqCst), 1);
    assert_eq!(line_count(fixture.calls), 1);
    registry.shutdown().await;
    assert!(click.execute("{}", context).await.is_error);
    save_case(
        true,
        fault,
        false,
        false,
        json!({"passed":true,"submission_count":fixture.submissions.load(Ordering::SeqCst),"click_rpc_count":line_count(fixture.calls),"backend_spawns":line_count(fixture.spawns),"late_success_did_not_unpoison":true,"explicit_reconnect":true,"mutation_replayed":false}),
    );
}

fn case_name(scoped: bool, fault: Fault, drop_cleanup: bool, awaited: bool) -> String {
    let operation = match fault {
        Fault::LostReply => "lost-reply",
        Fault::Cancel => "cancel",
        Fault::ForcedDrop => "forced-drop",
        Fault::None if awaited => "shutdown",
        Fault::None if drop_cleanup => "drop-cleanup",
        Fault::None => "form-image",
    };
    format!("{}-{operation}", if scoped { "scoped" } else { "stock" })
}
fn save_case(scoped: bool, fault: Fault, drop_cleanup: bool, awaited: bool, evidence: Value) {
    if let Some(root) = std::env::var_os("ATOMCODE_BROWSER_TEST_ARTIFACTS") {
        let directory =
            std::path::PathBuf::from(root).join(case_name(scoped, fault, drop_cleanup, awaited));
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("result.json"),
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();
    }
}
