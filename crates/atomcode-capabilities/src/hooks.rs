//! Reusable [`LifecycleHooks`] implementations (L1) — provider-agnostic.

use async_trait::async_trait;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::tool::ToolDef;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A provider-**agnostic** wire logger. It rides the kernel's hook seam, so a single
/// instance logs the request/response for ANY provider or agent — not one adapter:
///   - `on_request` (the kernel-documented home for telemetry / datalog / cache-RCA)
///     dumps the FINAL outgoing request: post-projection `messages`, the frozen
///     `tools` block, the sideband `options`, and round/epoch.
///   - `on_model_response` dumps the assembled assistant message (text + tool_calls +
///     reasoning + kernel `meta`).
///
/// It logs the NEUTRAL kernel view (`Message`/`ToolDef`/`ChatOptions`), which is also
/// exactly what the prefix cache keys on — the right level for "what did we send / get"
/// and cache diagnosis. For a specific backend's BYTE-EXACT framing (raw SSE / vendor
/// JSON), an adapter-level dump is the separate, provider-specific tool.
///
/// The output [`sink`](WireLogHooks::with_sink) is injectable (stderr by default; a
/// file writer, a `tracing` bridge, or a test buffer otherwise), so the embedder
/// controls destination and the logger stays unit-testable.
#[derive(Clone)]
pub struct WireLogHooks {
    sink: Arc<dyn Fn(&str) + Send + Sync>,
    pretty: bool,
}

impl WireLogHooks {
    /// Log to stderr as pretty JSON.
    pub fn stderr() -> Self {
        Self { sink: Arc::new(|s| eprintln!("{s}")), pretty: true }
    }

    /// Log via a custom sink (file writer / tracing bridge / test buffer / …).
    pub fn with_sink(sink: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        Self { sink, pretty: true }
    }

    /// Emit compact single-line JSON instead of pretty-printed.
    pub fn compact(mut self) -> Self {
        self.pretty = false;
        self
    }

    /// Append log entries to a FILE (created if missing, opened in append mode). Each
    /// request/response entry is written as a block followed by a blank line. This is
    /// just `with_sink` wired to a file handle — the injectable-sink design means a new
    /// destination costs a few lines, no change to the hook itself.
    pub fn to_file(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path.into())?;
        let file = Arc::new(Mutex::new(file));
        Ok(Self::with_sink(Arc::new(move |s: &str| {
            if let Ok(mut f) = file.lock() {
                let _ = writeln!(f, "{s}\n");
            }
        })))
    }

    fn emit(&self, label: &str, value: &serde_json::Value) {
        let body = if self.pretty {
            serde_json::to_string_pretty(value)
        } else {
            serde_json::to_string(value)
        }
        .unwrap_or_else(|_| value.to_string());
        (self.sink)(&format!("{label}\n{body}"));
    }
}

#[async_trait]
impl LifecycleHooks for WireLogHooks {
    async fn on_request(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
        ctx: &TurnCtx,
    ) {
        let req = serde_json::json!({
            // Arc<str> isn't serde-Serialize by default; serialize the &str deref.
            "session_id": ctx.session_id.as_deref(),
            "turn_id": ctx.turn_id,
            "request_id": ctx.request_id,
            "round": ctx.round,
            "max_rounds": ctx.max_rounds,
            "cache_epoch": ctx.cache_epoch,
            "messages": messages,
            "tools": tools,
            "options": options,
        });
        self.emit(
            &format!(
                ">>> [wire] request — session={:?} turn={} round={} req={}",
                ctx.session_id, ctx.turn_id, ctx.round, ctx.request_id
            ),
            &req,
        );
    }

    async fn on_model_response(&self, response: &mut Message) {
        let resp = serde_json::to_value(&*response).unwrap_or(serde_json::Value::Null);
        self.emit("<<< [wire] response", &resp);
    }

    /// Errors (failed open / mid-stream provider error / tool error) — carries the
    /// provider's error code + reason as formatted by the adapter. Logged so failures
    /// are diagnosable alongside the request/response trail.
    async fn on_error(&self, error: &str) {
        (self.sink)(&format!("<<< [wire] error: {error}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolCall;
    use std::sync::Mutex;

    #[tokio::test]
    async fn logs_request_and_response_to_injected_sink() {
        let buf = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = buf.clone();
        let hooks = WireLogHooks::with_sink(Arc::new(move |s: &str| {
            captured.lock().unwrap().push(s.to_string())
        }));

        let msgs = vec![Message::system("sys"), Message::user("hi there")];
        let tools = vec![ToolDef {
            name: "get_time".into(),
            description: "d".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let ctx = TurnCtx {
            session_id: Some(Arc::from("sess-x")),
            turn_id: 3,
            request_id: 9,
            round: 2,
            max_rounds: Some(5),
            cache_epoch: 7,
            ..Default::default()
        };
        hooks.on_request(&msgs, &tools, &ChatOptions::default(), &ctx).await;

        let mut resp = Message::assistant(
            "calling tool",
            vec![ToolCall { id: "c1".into(), name: "get_time".into(), arguments: "{}".into() }],
        );
        hooks.on_model_response(&mut resp).await;

        let log = buf.lock().unwrap().join("\n");
        // request side
        assert!(log.contains("[wire] request"), "log: {log}");
        assert!(log.contains("sess-x"), "request must include session_id");
        assert!(log.contains("\"turn_id\": 3"), "request must include turn_id");
        assert!(log.contains("\"request_id\": 9"), "request must include request_id");
        assert!(log.contains("\"hi there\""), "request must include the user message");
        assert!(log.contains("\"get_time\""), "request must include the tool name");
        assert!(log.contains("\"cache_epoch\": 7"), "request must include the epoch");
        // response side
        assert!(log.contains("response"));
        assert!(log.contains("\"calling tool\""), "response must include assistant text");
        assert!(log.contains("\"c1\""), "response must include the tool call");
    }

    #[tokio::test]
    async fn to_file_appends_entries() {
        let path = std::env::temp_dir()
            .join(format!("atomcode_wirelog_test_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let hooks = WireLogHooks::to_file(&path).expect("open temp log file");
            let ctx = TurnCtx {
                session_id: None,
                turn_id: 1,
                request_id: 1,
                round: 1,
                max_rounds: None,
                cache_epoch: 0,
                ..Default::default()
            };
            hooks
                .on_request(&[Message::user("file-test-msg")], &[], &ChatOptions::default(), &ctx)
                .await;
        }
        let contents = std::fs::read_to_string(&path).expect("read log file");
        assert!(contents.contains("file-test-msg"), "log must contain request: {contents}");
        assert!(contents.contains("[wire] request"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn logs_errors_to_sink() {
        let buf = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = buf.clone();
        let hooks = WireLogHooks::with_sink(Arc::new(move |s: &str| captured.lock().unwrap().push(s.to_string())));
        hooks.on_error("HTTP 400: [invalid_request_error/model_not_found] no such model").await;
        let log = buf.lock().unwrap().join("\n");
        assert!(log.contains("[wire] error"), "log: {log}");
        assert!(log.contains("model_not_found"), "error code+reason must be logged: {log}");
    }

    #[tokio::test]
    async fn noop_default_hooks_dont_fire_other_methods() {
        // Sanity: WireLogHooks only overrides on_request/on_model_response; the other
        // LifecycleHooks methods stay the trait no-ops (don't block prompts, etc.).
        let hooks = WireLogHooks::with_sink(Arc::new(|_s: &str| {}));
        let mut text = "x".to_string();
        assert!(hooks.user_prompt_submit(&mut text).await.is_ok());
        assert_eq!(text, "x");
    }
}
