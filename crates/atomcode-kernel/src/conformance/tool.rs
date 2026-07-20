//! Conformance harness for the [`Tool`] seam.
//!
//! Contract clauses checked (see [`crate::tool`] for the trust + panic contract):
//! * `name_non_empty` / `description_non_empty` — both become the mounted `ToolDef` the
//!   model reads; an empty name is unroutable and an empty description gives no guidance.
//! * `name_stable` / `description_stable` / `schema_stable` — the mounted `ToolDef` is
//!   rendered into the prompt; unstable metadata silently poisons the prefix cache.
//! * `schema_is_object` — `parameters_schema()` must be a JSON object (a tool-parameters
//!   JSON Schema), not a bare array / string / number / null.
//! * `risk_deterministic` — `risk(args)` must be a pure function of its args; a
//!   specialization's approval cache (`name::args` grant) relies on a stable verdict.
//! * `execute_returns` / `execute_no_panic` / `execute_terminates` — `execute` must
//!   complete and return a `ToolResult` (a fallible tool returns `is_error: true`,
//!   never panics) and must not park forever.

use super::{catch_async, catch_sync, with_timeout, ConformanceReport, DEFAULT_CHECK_TIMEOUT};
use crate::tool::{ProgressSink, Tool, ToolContext};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn probe_ctx() -> ToolContext {
    ToolContext {
        working_dir: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        progress: ProgressSink::noop(),
        requester: None,
    }
}

/// Run the tool conformance suite.
///
/// `sample_args`: representative argument strings to drive `risk()` and `execute()`
/// with — the harness cannot invent valid args for an arbitrary tool. If empty, a
/// single `"{}"` is used.
pub async fn check(tool: Arc<dyn Tool>, sample_args: &[&str]) -> ConformanceReport {
    let subject =
        catch_sync(|| tool.name().to_string()).unwrap_or_else(|_| "<name() panicked>".into());
    let mut r = ConformanceReport::new("Tool", subject);
    let args: Vec<String> = if sample_args.is_empty() {
        vec!["{}".into()]
    } else {
        sample_args.iter().map(|s| s.to_string()).collect()
    };

    // name(): non-empty + stable.
    match (
        catch_sync(|| tool.name().to_string()),
        catch_sync(|| tool.name().to_string()),
    ) {
        (Ok(a), Ok(b)) => {
            r.record("name_non_empty", !a.is_empty(), "name() must be non-empty (it is the registry key and the ToolDef name the model sees)");
            r.record(
                "name_stable",
                a == b,
                format!("name() must return the same value across calls; got {a:?} then {b:?}"),
            );
        }
        _ => r.record("name_no_panic", false, "name() panicked"),
    }

    // description(): non-empty + stable.
    match (
        catch_sync(|| tool.description().to_string()),
        catch_sync(|| tool.description().to_string()),
    ) {
        (Ok(a), Ok(b)) => {
            r.record("description_non_empty", !a.is_empty(), "description() must be non-empty — it is rendered into the mounted ToolDef the model reads to decide WHEN to call the tool; an empty description gives the model no guidance");
            r.record(
                "description_stable",
                a == b,
                "description() must be stable across calls (it is rendered into the cached prompt)",
            );
        }
        _ => r.record("description_no_panic", false, "description() panicked"),
    }

    // parameters_schema(): a JSON object + stable.
    match (
        catch_sync(|| tool.parameters_schema()),
        catch_sync(|| tool.parameters_schema()),
    ) {
        (Ok(a), Ok(b)) => {
            r.record("schema_is_object", a.is_object(), format!("parameters_schema() must be a JSON object (a tool-parameters JSON Schema), got: {a}"));
            r.record(
                "schema_stable",
                a == b,
                "parameters_schema() must be stable across calls (prompt-cache stability)",
            );
        }
        _ => r.record("schema_no_panic", false, "parameters_schema() panicked"),
    }

    // risk(args): deterministic per args.
    for a in &args {
        match (catch_sync(|| tool.risk(a)), catch_sync(|| tool.risk(a))) {
            (Ok(x), Ok(y)) => r.record(
                "risk_deterministic",
                x == y,
                format!("risk({a:?}) must be deterministic for identical args (approval grant caching relies on it); got {x:?} then {y:?}"),
            ),
            _ => r.record("risk_no_panic", false, format!("risk({a:?}) panicked")),
        }
    }

    // execute(args): returns a ToolResult, bounded, no panic.
    for a in &args {
        let ctx = probe_ctx();
        let fut = async { tool.execute(a, &ctx).await };
        match with_timeout(DEFAULT_CHECK_TIMEOUT, catch_async(fut)).await {
            Ok(Ok(_result)) => r.record("execute_returns", true, ""),
            Ok(Err(p)) => r.record(
                "execute_no_panic",
                false,
                format!("execute({a:?}) panicked: {p} — a fallible tool must return ToolResult{{ is_error: true, .. }}, never panic"),
            ),
            Err(t) => r.record(
                "execute_terminates",
                false,
                format!("execute({a:?}) {t} — a tool must terminate (poll ctx.cancel for long work)"),
            ),
        }
    }

    r
}

/// OPT-IN check for a tool that claims to honor cooperative cancellation (a
/// long-running tool — see the `ctx.cancel` contract on [`ToolContext`]). Cancels the
/// token, then asserts `execute` returns within the bound. A tool that blocks forever
/// ignoring `ctx.cancel` fails `respects_cancel`; a fast tool passes trivially. Not part
/// of [`check`] (most tools have no long work to cancel).
pub async fn check_respects_cancel(tool: Arc<dyn Tool>, args: &str) -> ConformanceReport {
    let subject =
        catch_sync(|| tool.name().to_string()).unwrap_or_else(|_| "<name() panicked>".into());
    let mut r = ConformanceReport::new("Tool", subject);
    let cancel = CancellationToken::new();
    let ctx = ToolContext {
        working_dir: std::env::temp_dir(),
        cancel: cancel.clone(),
        progress: ProgressSink::noop(),
        requester: None,
    };
    cancel.cancel(); // pre-cancelled: a cooperative tool must observe it and bail.
    let fut = async { tool.execute(args, &ctx).await };
    match with_timeout(Duration::from_secs(2), catch_async(fut)).await {
        Ok(Ok(_)) => r.record("respects_cancel", true, "execute returned promptly under a cancelled token"),
        Ok(Err(p)) => r.record("respects_cancel", false, format!("execute panicked under cancel: {p}")),
        Err(t) => r.record("respects_cancel", false, format!("execute {t} under a cancelled token — a long-running tool must poll ctx.cancel and bail")),
    }
    r
}
