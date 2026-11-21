//! Opt-in managed setup and production registry/adapter acceptance for the source-only
//! backend. All pages and artifacts belong to this test; no external website.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use atomcode_capabilities::mcp::config::McpConfigSource;
use atomcode_capabilities::mcp::{McpRegistry, McpToolAdapter};
use atomcode_kernel::tool::{ProgressSink, RiskLevel, Tool, ToolContext};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "requires ATOMCODE_BROWSER_ORIGIN_TEST_ROOT, installed pinned MCP/Playwright, Node 20+ and Chrome"]
async fn real_origin_backend_enforces_policy_through_production_registry_and_adapter() {
    tokio::time::resume(); // Actual browser/subprocess I/O, not a timeout assertion.
    let root = std::env::var_os("ATOMCODE_BROWSER_ORIGIN_TEST_ROOT")
        .expect("set the absolute installed @playwright/mcp directory");
    let backend = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../integrations/browser-origin-scope/backend.cjs")
        .canonicalize()
        .unwrap();
    let node = which::which("node").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let denied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let denied_origin = format!("http://{}", denied.local_addr().unwrap());
    let forbidden_connections = Arc::new(AtomicUsize::new(0));
    let count = forbidden_connections.clone();
    let denied_task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = denied.accept().await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
        }
    });
    let allowed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", allowed.local_addr().unwrap());
    let redirect_target = denied_origin.clone();
    let allowed_task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = allowed.accept().await.unwrap();
            let target = redirect_target.clone();
            tokio::spawn(async move {
                let mut bytes = [0; 8192];
                let n = stream.read(&mut bytes).await.unwrap_or(0);
                let response = if bytes[..n].starts_with(b"GET /redirect ") {
                    format!("HTTP/1.1 302 Found\r\nLocation: {target}/forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                } else {
                    let body = "<h1>ATOMCODE_ORIGIN_REGISTRY_FIXTURE</h1>";
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                };
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    let dirs = atomcode_capabilities::ProductDirs::new(temp.path().join("user"), ".test-product");
    atomcode::browser_cmd::execute(
        &dirs,
        atomcode::browser_cmd::BrowserCli::Enable {
            server: None,
            node: Some(node),
            scope: atomcode::browser_cmd::OriginArgs {
                origin_backend: Some(backend),
                backend_root: Some(root.into()),
                allowed_origins: vec![origin.clone()],
                ..Default::default()
            },
        },
    )
    .await
    .unwrap();
    let source = std::fs::read_to_string(dirs.user().join("mcp.json")).unwrap();
    let configured = atomcode_capabilities::mcp::parse_mcp_servers(&source, McpConfigSource::User)
        .unwrap()
        .remove(0);
    assert_eq!(
        configured
            .required_experimental_capabilities
            .get("atomcode/browser-origin-scope"),
        Some(&1)
    );
    let make_config = |version| {
        let mut config = configured.clone();
        config
            .required_experimental_capabilities
            .insert("atomcode/browser-origin-scope".into(), version);
        config
    };
    let registry = McpRegistry::from_config(temp.path(), dirs.user())
        .await
        .share();
    let catalog = registry.list_all_tools().await;
    assert_eq!(catalog.len(), 16);
    for name in [
        "browser_evaluate",
        "browser_file_upload",
        "browser_run_code",
        "browser_cdp",
    ] {
        assert!(!catalog.iter().any(|tool| tool.tool_name == name));
        assert!(registry
            .call_tool("browser-use", name, json!({}))
            .await
            .is_err());
    }
    assert!(registry
        .call_tool(
            "browser-use",
            "browser_navigate",
            json!({"url":denied_origin})
        )
        .await
        .is_err());
    assert_eq!(forbidden_connections.load(Ordering::SeqCst), 0);
    let context = ToolContext {
        working_dir: temp.path().into(),
        cancel: Default::default(),
        progress: ProgressSink::noop(),
        requester: None,
    };
    let nav = McpToolAdapter::new(
        registry.clone(),
        catalog
            .iter()
            .find(|tool| tool.tool_name == "browser_navigate")
            .unwrap()
            .clone(),
    )
    .unwrap();
    assert_eq!(nav.risk("{}"), RiskLevel::Risky);
    let navigated = nav
        .execute(
            &json!({"url":format!("{origin}/clean")}).to_string(),
            &context,
        )
        .await;
    assert!(!navigated.is_error, "{}", navigated.content);
    let snapshot = registry
        .call_tool("browser-use", "browser_snapshot", json!({}))
        .await
        .unwrap();
    assert!(format!("{snapshot:?}").contains("ATOMCODE_ORIGIN_REGISTRY_FIXTURE"));
    let picture = McpToolAdapter::new(
        registry.clone(),
        catalog
            .iter()
            .find(|tool| tool.tool_name == "browser_take_screenshot")
            .unwrap()
            .clone(),
    )
    .unwrap()
    .execute(r#"{"type":"png"}"#, &context)
    .await;
    assert!(!picture.is_error);
    assert_eq!(picture.images.len(), 1);
    assert!(picture.images[0].data.len() > 100);
    let redirected = nav
        .execute(
            &json!({"url":format!("{origin}/redirect")}).to_string(),
            &context,
        )
        .await;
    assert!(!redirected.is_error, "{}", redirected.content);
    let refusal = registry
        .call_tool("browser-use", "browser_snapshot", json!({}))
        .await
        .unwrap();
    assert!(format!("{refusal:?}").contains("Browser origin policy refused this request"));
    assert_eq!(forbidden_connections.load(Ordering::SeqCst), 0);
    assert!(registry
        .reconnect_server(make_config(2))
        .await
        .unwrap_err()
        .to_string()
        .contains("required experimental capability"));
    assert!(registry.list_all_tools().await.is_empty());
    assert!(
        nav.execute(
            &json!({"url":format!("{origin}/clean")}).to_string(),
            &context
        )
        .await
        .is_error
    );
    registry.shutdown().await;
    denied_task.abort();
    allowed_task.abort();
    let _ = denied_task.await;
    let _ = allowed_task.await;
    assert_eq!(forbidden_connections.load(Ordering::SeqCst), 0);
}
