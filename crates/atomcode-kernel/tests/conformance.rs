//! Conformance-kit GATE: proves each seam harness is correct AND has TEETH.
//!
//! For every seam we assert BOTH directions:
//!   * a CONFORMING `testkit` double passes (`assert_conformant`), and
//!   * an ADVERSARIAL double FAILS a SPECIFIC named check.
//!
//! A harness that only ever passes is worthless — the adversarial half is what proves
//! it would actually catch a third-party violation. (L1 runs the SAME provider harness
//! against its real OpenAI-compat adapter; an L2 specialization runs the tool/middleware/
//! hook harnesses against its own extensions.)

use async_trait::async_trait;
use atomcode_kernel::conformance::{self, ConformanceReport};
use atomcode_kernel::hook::{HookChain, LifecycleHooks, NoopHooks};
use atomcode_kernel::message::{Message, MessageMeta};
use atomcode_kernel::middleware::{AfterOutcome, BeforeOutcome, ToolMiddleware};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::testkit::{
    ApprovalMiddleware, ArgRewriteMiddleware, BlockToolMiddleware, BlockUntilCancelTool,
    BudgetReminderHook, ContinueOnceHook, CountingTool, DangerousBashTool, DropToolsHook, EchoTool,
    MockProvider, RecorderHook, RecordingProvider, RedactHook, RejectPromptHook, RiskyWriteTool,
    RoundBudgetHook, ScriptedProvider, SilentStreamProvider, TruncateMiddleware,
    WorkingDirProbeTool,
};
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall, ToolContext, ToolDef, ToolResult};
use futures::stream::BoxStream;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Assert a report is non-conformant AND the failure is the EXPECTED named check.
fn assert_check_failed(report: &ConformanceReport, name: &str) {
    assert!(
        !report.passed(),
        "expected a violation but the report passed: {report:#?}"
    );
    assert!(
        report.failures().iter().any(|c| c.name == name),
        "expected check `{name}` to fail; actual failures = {:?}",
        report.failures().iter().map(|c| c.name).collect::<Vec<_>>(),
    );
}

// ───────────────────────────── Tool seam ─────────────────────────────

#[tokio::test]
async fn tool_doubles_are_conformant() {
    conformance::tool::check(Arc::new(EchoTool), &["{\"text\":\"hi\"}"])
        .await
        .assert_conformant();
    conformance::tool::check(Arc::new(RiskyWriteTool), &["{\"path\":\"/tmp/x\"}"])
        .await
        .assert_conformant();
    // arg-aware risk: drive both a safe AND a dangerous command.
    conformance::tool::check(
        Arc::new(DangerousBashTool),
        &["{\"cmd\":\"ls\"}", "{\"cmd\":\"rm -rf /tmp/x\"}"],
    )
    .await
    .assert_conformant();
    conformance::tool::check(
        Arc::new(CountingTool::new(Arc::new(AtomicUsize::new(0)))),
        &["{}"],
    )
    .await
    .assert_conformant();
    conformance::tool::check(Arc::new(WorkingDirProbeTool), &["{}"])
        .await
        .assert_conformant();
}

struct EmptyNameTool;
#[async_trait]
impl Tool for EmptyNameTool {
    fn name(&self) -> &str {
        ""
    }
    fn description(&self) -> &str {
        "a tool with an (invalid) empty name"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: "x".into(),
            is_error: false,
            images: vec![],
        }
    }
}

struct EmptyDescriptionTool;
#[async_trait]
impl Tool for EmptyDescriptionTool {
    fn name(&self) -> &str {
        "has_name_no_desc"
    }
    fn description(&self) -> &str {
        ""
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: "x".into(),
            is_error: false,
            images: vec![],
        }
    }
}

struct UnstableSchemaTool {
    n: AtomicUsize,
}
#[async_trait]
impl Tool for UnstableSchemaTool {
    fn name(&self) -> &str {
        "unstable_schema"
    }
    fn description(&self) -> &str {
        "schema changes every call (poisons the prompt cache)"
    }
    fn parameters_schema(&self) -> Value {
        let rev = self.n.fetch_add(1, Ordering::SeqCst);
        json!({"type": "object", "rev": rev})
    }
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: "x".into(),
            is_error: false,
            images: vec![],
        }
    }
}

struct FlakyRiskTool {
    n: AtomicUsize,
}
#[async_trait]
impl Tool for FlakyRiskTool {
    fn name(&self) -> &str {
        "flaky_risk"
    }
    fn description(&self) -> &str {
        "risk flips every call for identical args"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn risk(&self, _a: &str) -> RiskLevel {
        if self.n.fetch_add(1, Ordering::SeqCst).is_multiple_of(2) {
            RiskLevel::Safe
        } else {
            RiskLevel::Risky
        }
    }
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: "x".into(),
            is_error: false,
            images: vec![],
        }
    }
}

struct PanicExecuteTool;
#[async_trait]
impl Tool for PanicExecuteTool {
    fn name(&self) -> &str {
        "panic_exec"
    }
    fn description(&self) -> &str {
        "panics in execute (violates the must-not-panic contract)"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        panic!("tool execute blew up");
    }
}

#[tokio::test]
async fn tool_harness_catches_violations() {
    assert_check_failed(
        &conformance::tool::check(Arc::new(EmptyNameTool), &["{}"]).await,
        "name_non_empty",
    );
    assert_check_failed(
        &conformance::tool::check(Arc::new(EmptyDescriptionTool), &["{}"]).await,
        "description_non_empty",
    );
    assert_check_failed(
        &conformance::tool::check(
            Arc::new(UnstableSchemaTool {
                n: AtomicUsize::new(0),
            }),
            &["{}"],
        )
        .await,
        "schema_stable",
    );
    assert_check_failed(
        &conformance::tool::check(
            Arc::new(FlakyRiskTool {
                n: AtomicUsize::new(0),
            }),
            &["{}"],
        )
        .await,
        "risk_deterministic",
    );
    assert_check_failed(
        &conformance::tool::check(Arc::new(PanicExecuteTool), &["{}"]).await,
        "execute_no_panic",
    );
}

struct BlockForeverTool;
#[async_trait]
impl Tool for BlockForeverTool {
    fn name(&self) -> &str {
        "block_forever"
    }
    fn description(&self) -> &str {
        "ignores ctx.cancel and blocks forever"
    }
    fn parameters_schema(&self) -> Value {
        json!({"type": "object"})
    }
    async fn execute(&self, _a: &str, _c: &ToolContext) -> ToolResult {
        futures::future::pending::<()>().await;
        unreachable!()
    }
}

#[tokio::test]
async fn tool_cancel_check_positive_and_negative() {
    // A cooperative tool that bails on cancel passes.
    let observed = Arc::new(AtomicBool::new(false));
    conformance::tool::check_respects_cancel(Arc::new(BlockUntilCancelTool::new(observed)), "{}")
        .await
        .assert_conformant();
    // A tool that ignores cancel and blocks forever fails.
    assert_check_failed(
        &conformance::tool::check_respects_cancel(Arc::new(BlockForeverTool), "{}").await,
        "respects_cancel",
    );
}

// ──────────────────────────── Middleware seam ────────────────────────────

#[tokio::test]
async fn middleware_doubles_are_conformant() {
    conformance::middleware::check(Arc::new(ApprovalMiddleware::new()))
        .await
        .assert_conformant();
    conformance::middleware::check(Arc::new(ArgRewriteMiddleware))
        .await
        .assert_conformant();
    conformance::middleware::check(Arc::new(BlockToolMiddleware))
        .await
        .assert_conformant();
    conformance::middleware::check(Arc::new(TruncateMiddleware))
        .await
        .assert_conformant();
}

struct ParkForeverMiddleware;
#[async_trait]
impl ToolMiddleware for ParkForeverMiddleware {
    async fn before(
        &self,
        _call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        // Ignores the RequestCtx timeout and parks — the kernel turn would hang here.
        futures::future::pending::<()>().await;
        BeforeOutcome::Proceed
    }
}

struct PanicBeforeMiddleware;
#[async_trait]
impl ToolMiddleware for PanicBeforeMiddleware {
    async fn before(
        &self,
        _call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        panic!("before blew up");
    }
}

struct PanicAfterMiddleware;
#[async_trait]
impl ToolMiddleware for PanicAfterMiddleware {
    async fn after(&self, _result: &mut ToolResult) -> AfterOutcome {
        panic!("after blew up");
    }
}

struct ParkAfterMiddleware;
#[async_trait]
impl ToolMiddleware for ParkAfterMiddleware {
    async fn after(&self, _result: &mut ToolResult) -> AfterOutcome {
        futures::future::pending::<AfterOutcome>().await
    }
}

#[tokio::test]
async fn middleware_harness_catches_violations() {
    assert_check_failed(
        &conformance::middleware::check(Arc::new(ParkForeverMiddleware)).await,
        "before_terminates",
    );
    assert_check_failed(
        &conformance::middleware::check(Arc::new(PanicBeforeMiddleware)).await,
        "before_no_panic",
    );
    assert_check_failed(
        &conformance::middleware::check(Arc::new(PanicAfterMiddleware)).await,
        "after_no_panic",
    );
    assert_check_failed(
        &conformance::middleware::check(Arc::new(ParkAfterMiddleware)).await,
        "after_terminates",
    );
}

// ────────────────────────────── Hooks seam ──────────────────────────────

#[tokio::test]
async fn hook_doubles_are_conformant() {
    conformance::hooks::check(Arc::new(NoopHooks))
        .await
        .assert_conformant();
    conformance::hooks::check(Arc::new(RecorderHook::new()))
        .await
        .assert_conformant();
    conformance::hooks::check(Arc::new(BudgetReminderHook))
        .await
        .assert_conformant();
    conformance::hooks::check(Arc::new(RoundBudgetHook))
        .await
        .assert_conformant();
    conformance::hooks::check(Arc::new(RedactHook))
        .await
        .assert_conformant();
    conformance::hooks::check(Arc::new(DropToolsHook))
        .await
        .assert_conformant();
    conformance::hooks::check(Arc::new(ContinueOnceHook::new()))
        .await
        .assert_conformant();
    conformance::hooks::check(Arc::new(RejectPromptHook))
        .await
        .assert_conformant();
    // A composed chain is itself a conforming LifecycleHooks.
    let chain: Arc<dyn LifecycleHooks> = Arc::new(HookChain::new(vec![
        Arc::new(RecorderHook::new()),
        Arc::new(BudgetReminderHook),
    ]));
    conformance::hooks::check(chain).await.assert_conformant();
}

struct PanicDeltaHook;
#[async_trait]
impl LifecycleHooks for PanicDeltaHook {
    async fn on_text_delta(&self, _d: &mut String) {
        panic!("hook on_text_delta blew up");
    }
}

/// Fabricates the kernel-owned `meta` in on_model_response — a contract violation.
struct FabricateMetaHook;
#[async_trait]
impl LifecycleHooks for FabricateMetaHook {
    async fn on_model_response(&self, response: &mut Message) {
        response.meta = Some(MessageMeta {
            round: 999,
            ..Default::default()
        });
    }
}

#[tokio::test]
async fn hook_harness_catches_violations() {
    assert_check_failed(
        &conformance::hooks::check(Arc::new(PanicDeltaHook)).await,
        "on_text_delta",
    );
    assert_check_failed(
        &conformance::hooks::check(Arc::new(FabricateMetaHook)).await,
        "on_model_response_preserves_meta",
    );
}

// ───────────────────────────── Provider seam ─────────────────────────────

#[tokio::test]
async fn provider_doubles_are_conformant() {
    conformance::provider::check(Arc::new(MockProvider::new(vec![vec![
        StreamEvent::TextDelta("hi".into()),
        StreamEvent::Done { truncated: false },
    ]])))
    .await
    .assert_conformant();

    conformance::provider::check(Arc::new(RecordingProvider::new(vec![vec![
        StreamEvent::TextDelta("hi".into()),
        StreamEvent::Done { truncated: false },
    ]])))
    .await
    .assert_conformant();

    conformance::provider::check(Arc::new(ScriptedProvider::events(vec![
        StreamEvent::TextDelta("hi".into()),
        StreamEvent::Done { truncated: false },
    ])))
    .await
    .assert_conformant();

    // A clean FAILED OPEN (Err) is conformant — the provider didn't panic.
    let err = atomcode_kernel::stream::ProviderError {
        retryable: false,
        message: "401 unauthorized".into(),
        http_status: Some(401),
        code: Some("auth".into()),
        retry_after_secs: None,
    };
    conformance::provider::check(Arc::new(ScriptedProvider::open_error(err)))
        .await
        .assert_conformant();
}

struct PanicOnOptionsProvider;
#[async_trait]
impl LlmProvider for PanicOnOptionsProvider {
    fn model_name(&self) -> &str {
        "panic-on-options"
    }
    async fn chat_stream(
        &self,
        _m: &[Message],
        _t: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        if *options != ChatOptions::default() {
            panic!("cannot handle non-default options");
        }
        Ok(Box::pin(futures::stream::iter(vec![StreamEvent::Done {
            truncated: false,
        }])))
    }
}

#[tokio::test]
async fn provider_harness_catches_violations() {
    // Opens OK, then pends forever → the stream never terminates.
    assert_check_failed(
        &conformance::provider::check(Arc::new(SilentStreamProvider::new(vec![]))).await,
        "stream_terminates",
    );
    // Panics when handed non-default ChatOptions (must map or ignore, never panic).
    assert_check_failed(
        &conformance::provider::check(Arc::new(PanicOnOptionsProvider)).await,
        "chat_stream_handles_options",
    );
}

#[test]
fn stream_wellformed_contract() {
    use conformance::provider::check_stream_wellformed;
    // Positive: content then a single trailing Done.
    let good = vec![
        StreamEvent::TextDelta("hi".into()),
        StreamEvent::Done { truncated: false },
    ];
    check_stream_wellformed(&good).assert_conformant();
    // Negative: an event AFTER the Done terminal.
    let after_done = vec![
        StreamEvent::Done { truncated: false },
        StreamEvent::TextDelta("late".into()),
    ];
    assert_check_failed(
        &check_stream_wellformed(&after_done),
        "no_events_after_terminal",
    );
    // Negative: two Done events.
    let two_done = vec![
        StreamEvent::Done { truncated: false },
        StreamEvent::Done { truncated: false },
    ];
    assert_check_failed(&check_stream_wellformed(&two_done), "at_most_one_done");
    // Negative: a mid-stream Error carrying an HTTP status (reserved for OPEN failures).
    let bad_err = vec![StreamEvent::Error(ProviderError {
        retryable: true,
        message: "mid-stream".into(),
        http_status: Some(500),
        code: None,
        retry_after_secs: None,
    })];
    assert_check_failed(
        &check_stream_wellformed(&bad_err),
        "midstream_error_has_no_http_status",
    );
}

#[test]
fn stream_reconstruction_contract() {
    // Positive: two fragments at index 0 reconstruct the complete call byte-for-byte.
    let good = vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("c1".into()),
            name: Some("echo".into()),
            arguments: "{\"x\"".into(),
        },
        StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: ":1}".into(),
        },
        StreamEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: "{\"x\":1}".into(),
        }),
        StreamEvent::Done { truncated: false },
    ];
    conformance::provider::check_stream_reconstruction(&good).assert_conformant();

    // Negative: the concatenated args don't match the complete call.
    let bad_args = vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: None,
            name: Some("echo".into()),
            arguments: "{\"x\":1}".into(),
        },
        StreamEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: "{\"WRONG\":9}".into(),
        }),
    ];
    assert_check_failed(
        &conformance::provider::check_stream_reconstruction(&bad_args),
        "reconstructed_args_match",
    );

    // Negative: a streamed index with NO matching complete call.
    let orphan = vec![StreamEvent::ToolCallDelta {
        index: 0,
        id: Some("c1".into()),
        name: Some("echo".into()),
        arguments: "{}".into(),
    }];
    assert_check_failed(
        &conformance::provider::check_stream_reconstruction(&orphan),
        "delta_indices_have_complete",
    );

    // A stream with NO ToolCallDelta is trivially conformant.
    let no_deltas = vec![
        StreamEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "echo".into(),
            arguments: "{}".into(),
        }),
        StreamEvent::Done { truncated: false },
    ];
    conformance::provider::check_stream_reconstruction(&no_deltas).assert_conformant();
}

// ─────────────────────────── Aggregate smoke ───────────────────────────

#[tokio::test]
async fn all_four_seams_conformant_smoke() {
    conformance::tool::check(Arc::new(EchoTool), &["{}"])
        .await
        .assert_conformant();
    conformance::middleware::check(Arc::new(ApprovalMiddleware::new()))
        .await
        .assert_conformant();
    conformance::hooks::check(Arc::new(RecorderHook::new()))
        .await
        .assert_conformant();
    conformance::provider::check(Arc::new(MockProvider::new(vec![vec![StreamEvent::Done {
        truncated: false,
    }]])))
    .await
    .assert_conformant();
}
