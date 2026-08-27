//! Conformance harness for the [`ToolMiddleware`] seam.
//!
//! Contract clauses checked (see [`crate::middleware`] for the ordering + panic
//! contract):
//! * `before_returns_result` / `before_no_panic` / `before_terminates` — `before` must
//!   return `Ok(())` (allow) or `Err(reason)` (block) WITHOUT panicking, and must NOT
//!   park forever. A gating middleware that round-trips the driver via `RequestCtx`
//!   MUST honor the request timeout: the harness drives it with a short-timeout
//!   `RequestCtx` and NO answering driver, so a correct gate degrades to a deny (Err)
//!   instead of hanging.
//! * `after_returns` / `after_no_panic` / `after_terminates` — `after` must transform /
//!   observe the result in place and return, bounded, without panicking.

use super::{catch_async, with_timeout, ConformanceReport, DEFAULT_CHECK_TIMEOUT};
use crate::middleware::{BeforeOutcome, ToolMiddleware};
use crate::request::RequestCtx;
use crate::tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolResult};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

/// Internal probe tool, rated **Risky**, so a gating middleware exercises its gate
/// path (a `Safe` tool would let an approval middleware early-return without ever
/// round-tripping — failing to test the load-bearing case).
struct ProbeTool;

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        "conformance_probe"
    }
    fn description(&self) -> &str {
        "conformance probe tool (risky, to exercise gating middleware)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: "probe".into(),
            is_error: false,
            images: vec![],
        }
    }
}

/// Run the middleware conformance suite. The harness supplies its own risky probe
/// tool and a short-timeout `RequestCtx` with no answering driver — so a middleware
/// that round-trips degrades to a bounded deny rather than parking forever.
pub async fn check(mw: Arc<dyn ToolMiddleware>) -> ConformanceReport {
    let mut r = ConformanceReport::new("ToolMiddleware", "<middleware>");
    let tool: Arc<dyn Tool> = Arc::new(ProbeTool);

    // A RequestCtx with a SHORT request timeout: a gating `before` that awaits the
    // driver gets no `Respond`, so the round-trip degrades to `Value::Null` after
    // ~50ms (→ deny) instead of parking. The event channel is drained so it can't
    // back-pressure (it is unbounded regardless).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });

    // before(): bounded, no panic, returns Ok|Err.
    let mut call = ToolCall {
        id: "c1".into(),
        name: "conformance_probe".into(),
        arguments: "{}".into(),
    };
    let before_fut = async { mw.before(&mut call, &tool, &rt).await };
    match with_timeout(DEFAULT_CHECK_TIMEOUT, catch_async(before_fut)).await {
        Ok(Ok(res)) => r.record(
            "before_returns_result",
            true,
            match res {
                BeforeOutcome::Proceed => "Proceed — allowed".to_string(),
                BeforeOutcome::Allow { .. } => "Allow — force-approved".to_string(),
                BeforeOutcome::Ask { .. } => "Ask — approval requested".to_string(),
                BeforeOutcome::Deny { reason } => format!("Deny — blocked: {reason}"),
                BeforeOutcome::DenyTurn { reason } => {
                    format!("DenyTurn — blocked and turn terminated: {reason}")
                }
                BeforeOutcome::DenyTurnWithIntervention { reason, .. } => {
                    format!("DenyTurn — blocked with recovery contract: {reason}")
                }
            },
        ),
        Ok(Err(p)) => r.record(
            "before_no_panic",
            false,
            format!("before() panicked: {p} — to block a call return Err(reason), never panic"),
        ),
        Err(t) => r.record(
            "before_terminates",
            false,
            format!("before() {t} — a middleware that round-trips MUST honor the RequestCtx timeout and not park forever"),
        ),
    }

    // after(): bounded, no panic — distinct clauses so a failure pinpoints panic vs park.
    let mut result = ToolResult {
        call_id: "c1".into(),
        content: "result body".into(),
        is_error: false,
        images: vec![],
    };
    let after_fut = async { mw.after(&mut result, None).await };
    match with_timeout(DEFAULT_CHECK_TIMEOUT, catch_async(after_fut)).await {
        Ok(Ok(_)) => r.record("after_returns", true, ""),
        Ok(Err(p)) => r.record(
            "after_no_panic",
            false,
            format!("after() panicked: {p} — transform/observe the result in place, never panic"),
        ),
        Err(t) => r.record(
            "after_terminates",
            false,
            format!("after() {t} — after() must return, bounded"),
        ),
    }

    drain.abort();
    r
}
