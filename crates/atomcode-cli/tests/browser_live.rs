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
    acceptance(false, false).await;
}

#[tokio::test]
#[ignore = "requires an explicitly installed browser backend and Chrome; fault injection acceptance"]
async fn real_submitted_browser_click_is_not_replayed_after_its_response_is_lost() {
    acceptance(true, false).await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires installed browser backend and Chrome; verifies owned process disposal"]
async fn real_browser_registry_drop_disposes_its_owned_browser_processes() {
    acceptance(false, true).await;
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

async fn acceptance(drop_response: bool, verify_disposal: bool) {
    let server = std::env::var_os("ATOMCODE_BROWSER_TEST_SERVER")
        .expect("set explicit installed cli.js path");
    let temp = tempfile::tempdir().unwrap();
    let dirs = ProductDirs::new(temp.path().join("user"), ".test-product");
    let project = temp.path().join("project");
    std::fs::create_dir(&project).unwrap();
    browser_cmd::execute(
        &dirs,
        BrowserCli::Enable {
            server: Some(server.into()),
            node: None,
        },
    )
    .await
    .unwrap();
    // Only acceptance is headless; user setup keeps the normal visible browser default.
    let config = dirs.user().join("mcp.json");
    let mut root: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    root["mcpServers"]["browser-use"]["env"]["PLAYWRIGHT_MCP_HEADLESS"] = json!("true");
    let backend_pid_file = temp.path().join("backend.pid");
    if verify_disposal {
        let wrapper = temp.path().join("owned-backend.cjs");
        let server = root["mcpServers"]["browser-use"]["args"][0]
            .as_str()
            .unwrap();
        std::fs::write(
            &wrapper,
            format!(
                "require('node:fs').writeFileSync({}, String(process.pid)); require({});",
                json!(backend_pid_file),
                json!(server)
            ),
        )
        .unwrap();
        root["mcpServers"]["browser-use"]["args"][0] = json!(wrapper);
    }
    if drop_response {
        // Forward the real protocol, but drop exactly one successful click response.
        // The underlying browser has already submitted; reconnect must not replay it.
        let wrapper = temp.path().join("drop-click-reply.cjs");
        let marker = temp.path().join("dropped-once");
        let actual_server = root["mcpServers"]["browser-use"]["args"][0]
            .as_str()
            .unwrap();
        let script = format!(
            r#"
const {{spawn}}=require('node:child_process');
const fs=require('node:fs');
const readline=require('node:readline');
const marker={marker};
const child=spawn(process.execPath,[{server},...process.argv.slice(2)],{{stdio:['pipe','pipe','ignore']}});
let clickId;
readline.createInterface({{input:process.stdin}}).on('line',line=>{{
  const m=JSON.parse(line);
  if(m.method==='tools/call' && m.params.name==='browser_click') clickId=m.id;
  child.stdin.write(line+'\n');
}});
readline.createInterface({{input:child.stdout}}).on('line',line=>{{
  const m=JSON.parse(line);
  if(clickId!==undefined && m.id===clickId && !fs.existsSync(marker)) {{
    fs.writeFileSync(marker,'one response lost'); child.kill(); process.exit(0);
  }}
  process.stdout.write(line+'\n');
}});
child.on('exit',code=>process.exit(code||0));
process.on('SIGTERM',()=>{{child.kill();process.exit(0);}});
"#,
            marker = json!(marker.to_str().unwrap()),
            server = json!(actual_server)
        );
        std::fs::write(&wrapper, script).unwrap();
        root["mcpServers"]["browser-use"]["args"][0] = json!(wrapper.to_str().unwrap());
    }
    std::fs::write(&config, root.to_string()).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let submissions = Arc::new(AtomicUsize::new(0));
    let counter = submissions.clone();
    let web = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut bytes = vec![0; 8192];
                let n = stream.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]);
                let body = if request.starts_with("GET /submit?") {
                    let count = counter.fetch_add(1, Ordering::SeqCst) + 1;
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
    let click = McpToolAdapter::new(registry.clone(), click_info).unwrap();
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
        // Wait on the same recovering registry by making a safe observation.
        invoke(&registry, "browser_snapshot", json!({}), &ctx).await;
        assert_eq!(submissions.load(Ordering::SeqCst), 1);
        invoke(&registry, "browser_close", json!({}), &ctx).await;
        web.abort();
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
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pixels = base64::engine::general_purpose::STANDARD
            .decode(&screenshot.images[0].data)
            .unwrap();
        std::fs::write(dir.join("accepted-record.png"), pixels).unwrap();
        std::fs::write(dir.join("accepted-record.json"), json!({"backend":"@playwright/mcp 0.0.85","submission_count":submissions.load(Ordering::SeqCst),"inline_image_count":screenshot.images.len(),"final_snapshot":fresh.content}).to_string()).unwrap();
    }
    if verify_disposal {
        #[cfg(unix)]
        {
            let pid: u32 = std::fs::read_to_string(backend_pid_file)
                .unwrap()
                .parse()
                .unwrap();
            let owned = owned_process_tree(pid).await;
            drop(click); // The adapter also holds the registry.
            drop(registry);
            let began = std::time::Instant::now();
            loop {
                let mut alive = Vec::new();
                for pid in &owned {
                    if tokio::process::Command::new("/bin/kill")
                        .args(["-0", &pid.to_string()])
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await
                        .unwrap()
                        .success()
                    {
                        alive.push(*pid);
                    }
                }
                if alive.is_empty() {
                    break;
                }
                assert!(
                    began.elapsed() < std::time::Duration::from_secs(5),
                    "owned process IDs still alive after bounded disposal: {alive:?}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    } else {
        invoke(&registry, "browser_close", json!({}), &ctx).await;
    }
    web.abort();
}
