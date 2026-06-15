//! Conformance harness for the [`LlmProvider`] seam.
//!
//! Two entry points:
//! * [`check`] — drives an arbitrary provider through a minimal request and verifies the
//!   contract clauses that hold REGARDLESS of scenario: stable model identity, a stream
//!   that OPENS (`Ok`) or FAILS CLEANLY (`Err(ProviderError)`) without panicking, and —
//!   crucially — a stream that TERMINATES (the kernel's turn loop consumes it to
//!   completion each turn; a non-terminating stream parks the agent forever).
//! * [`check_stream_reconstruction`] — a PURE checker over a `Vec<StreamEvent>` for the
//!   streaming-tool-call contract: concatenating `ToolCallDelta.arguments` per `index`
//!   must reproduce the complete `StreamEvent::ToolCall` the kernel actually EXECUTES.
//!   An adapter feeds its DECODED events (e.g. an SSE fixture run through its decoder)
//!   into this — so the kernel's stream contract is verified against the real decoder
//!   without a live backend.

use super::{catch_async, catch_sync, with_timeout, ConformanceReport, DEFAULT_CHECK_TIMEOUT};
use crate::message::Message;
use crate::provider::{ChatOptions, LlmProvider, ReasoningEffort, ToolChoice};
use crate::stream::StreamEvent;
use crate::tool::{ToolCall, ToolDef};
use std::sync::Arc;

/// Run the scenario-independent provider conformance suite.
pub async fn check(provider: Arc<dyn LlmProvider>) -> ConformanceReport {
    let subject =
        catch_sync(|| provider.model_name().to_string()).unwrap_or_else(|_| "<model_name() panicked>".into());
    let mut r = ConformanceReport::new("LlmProvider", subject);

    // model_name(): non-empty + stable (it labels stored meta / telemetry).
    match (catch_sync(|| provider.model_name().to_string()), catch_sync(|| provider.model_name().to_string())) {
        (Ok(a), Ok(b)) => {
            r.record("model_name_non_empty", !a.is_empty(), "model_name() must be non-empty");
            r.record("model_name_stable", a == b, format!("model_name() must be stable across calls; got {a:?} then {b:?}"));
        }
        _ => r.record("model_name_no_panic", false, "model_name() panicked"),
    }

    // context_window(): stable (0 = unknown is allowed).
    match (catch_sync(|| provider.context_window()), catch_sync(|| provider.context_window())) {
        (Ok(a), Ok(b)) => r.record("context_window_stable", a == b, format!("context_window() must be stable across calls; got {a} then {b}")),
        _ => r.record("context_window_no_panic", false, "context_window() panicked"),
    }

    // chat_stream(): a minimal request opens (Ok) or fails cleanly (Err), no panic; an
    // opened stream must TERMINATE within the bound.
    let messages = vec![Message::user("ping")];
    let tools: Vec<ToolDef> = vec![];
    let options = ChatOptions::default();
    let open_fut = async { provider.chat_stream(&messages, &tools, &options).await };
    match with_timeout(DEFAULT_CHECK_TIMEOUT, catch_async(open_fut)).await {
        Ok(Ok(Ok(stream))) => {
            r.record("chat_stream_opens", true, "");
            let drain = async move {
                use futures::StreamExt;
                let mut stream = stream;
                let mut n = 0usize;
                while let Some(_ev) = stream.next().await {
                    n += 1;
                    if n > 100_000 {
                        break; // runaway guard
                    }
                }
                n
            };
            match with_timeout(DEFAULT_CHECK_TIMEOUT, catch_async(drain)).await {
                Ok(Ok(_n)) => r.record("stream_terminates", true, ""),
                Ok(Err(p)) => r.record("stream_no_panic", false, format!("the stream panicked while producing events: {p}")),
                Err(t) => r.record(
                    "stream_terminates",
                    false,
                    format!("the stream {t} — a provider stream MUST terminate (the kernel drains it to completion each turn; a half-open / silent stream parks the agent forever)"),
                ),
            }
        }
        Ok(Ok(Err(e))) => r.record("open_failed_cleanly", true, format!("chat_stream returned Err — a clean failed open is conformant: {}", e.message)),
        Ok(Err(p)) => r.record("chat_stream_no_panic", false, format!("chat_stream panicked on open: {p} — a failed open must return Err(ProviderError), never panic")),
        Err(t) => r.record("chat_stream_opens", false, format!("chat_stream {t} on open — opening the stream must not block unboundedly")),
    }

    // chat_stream must HANDLE non-default ChatOptions — the adapter MAPS each neutral
    // knob onto its wire format or IGNORES it, but must never panic (the kernel forwards
    // options verbatim). Open with a fully-populated request; the stream is then dropped
    // (we only assert the open itself doesn't panic on the options).
    let loud = ChatOptions {
        reasoning_effort: Some(ReasoningEffort::High),
        max_tokens: Some(256),
        temperature: Some(0.2),
        tool_choice: ToolChoice::Required,
    };
    let opts_fut = async { provider.chat_stream(&messages, &tools, &loud).await };
    match with_timeout(DEFAULT_CHECK_TIMEOUT, catch_async(opts_fut)).await {
        Ok(Ok(Ok(_stream))) => r.record("chat_stream_handles_options", true, ""),
        Ok(Ok(Err(_e))) => r.record("chat_stream_handles_options", true, "returned Err on non-default options (clean — an adapter may reject an option it cannot honor)"),
        Ok(Err(p)) => r.record("chat_stream_handles_options", false, format!("chat_stream panicked on non-default ChatOptions: {p} — an adapter must MAP or IGNORE each neutral knob, never panic")),
        Err(t) => r.record("chat_stream_handles_options", false, format!("chat_stream {t} on non-default options — opening must not block unboundedly")),
    }

    r
}

/// PURE checker for STREAM WELL-FORMEDNESS over a captured `Vec<StreamEvent>` — the
/// terminal-event + error-shape invariants the kernel's drain loop relies on. Like
/// [`check_stream_reconstruction`], an adapter feeds its DECODED events (e.g. an SSE
/// fixture run through its decoder) in, so these hold against the real decoder without a
/// live backend. Checks:
/// * `no_events_after_terminal` — a `Done`/`Error` terminal is the LAST event (an event
///   after the turn has ended is processed as if mid-turn → undefined behavior).
/// * `at_most_one_done` — `Done` (end-of-stream) is emitted at most once.
/// * `midstream_error_has_no_http_status` — a `StreamEvent::Error` (mid-stream) carries
///   `http_status == None`; `http_status` is reserved for an OPEN failure's non-2xx code.
pub fn check_stream_wellformed(events: &[StreamEvent]) -> ConformanceReport {
    let mut r = ConformanceReport::new("LlmProvider stream", "<events>");

    let terminal = events
        .iter()
        .position(|e| matches!(e, StreamEvent::Done { .. } | StreamEvent::Error(_)));
    match terminal {
        Some(i) => r.record(
            "no_events_after_terminal",
            i + 1 == events.len(),
            format!("a Done/Error terminal must be the LAST event; {} event(s) follow it", events.len().saturating_sub(i + 1)),
        ),
        None => r.record("no_events_after_terminal", true, "no Done/Error terminal present (stream ends via end-of-stream)"),
    }

    let dones = events.iter().filter(|e| matches!(e, StreamEvent::Done { .. })).count();
    r.record("at_most_one_done", dones <= 1, format!("a stream must emit Done at most once; found {dones}"));

    let bad_status = events.iter().any(|e| matches!(e, StreamEvent::Error(err) if err.http_status.is_some()));
    r.record(
        "midstream_error_has_no_http_status",
        !bad_status,
        "a mid-stream StreamEvent::Error must have http_status == None (http_status is reserved for an OPEN failure's non-2xx response)",
    );

    r
}

/// PURE checker for the streaming-tool-call reconstruction contract over a captured
/// `Vec<StreamEvent>` (e.g. an adapter's decoded SSE fixture). The contract: for each
/// tool-call `index` the provider streamed as `ToolCallDelta` fragments, concatenating
/// the `arguments` chunks (and taking the first non-empty `name`) must reproduce the
/// complete `StreamEvent::ToolCall` the kernel EXECUTES. Complete calls are correlated
/// to delta indices BY ORDER (the i-th streamed index ↔ the i-th complete call), the
/// same way an index-buffering decoder flushes at finish.
///
/// A stream with NO `ToolCallDelta` is trivially conformant — an adapter that cannot
/// stream tool calls simply emits the complete `ToolCall` and the path is unchanged.
pub fn check_stream_reconstruction(events: &[StreamEvent]) -> ConformanceReport {
    use std::collections::BTreeMap;
    let mut r = ConformanceReport::new("LlmProvider stream", "<events>");

    // (first id, first name, concatenated args) per index, in index order.
    let mut groups: BTreeMap<u32, (Option<String>, Option<String>, String)> = BTreeMap::new();
    let mut complete: Vec<ToolCall> = Vec::new();
    for ev in events {
        match ev {
            StreamEvent::ToolCallDelta { index, id, name, arguments } => {
                let g = groups.entry(*index).or_default();
                if g.0.is_none() {
                    if let Some(i) = id {
                        g.0 = Some(i.clone());
                    }
                }
                if g.1.is_none() {
                    if let Some(n) = name {
                        g.1 = Some(n.clone());
                    }
                }
                g.2.push_str(arguments);
            }
            StreamEvent::ToolCall(c) => complete.push(c.clone()),
            _ => {}
        }
    }

    if groups.is_empty() {
        r.record(
            "no_delta_stream_ok",
            true,
            "no ToolCallDelta emitted — a provider that cannot stream tool calls is conformant (the complete-ToolCall path is unchanged)",
        );
        return r;
    }

    let indices: Vec<u32> = groups.keys().copied().collect();
    r.record(
        "delta_indices_have_complete",
        indices.len() == complete.len(),
        format!(
            "each streamed tool-call index must have a matching complete StreamEvent::ToolCall (the kernel executes the complete call); {} delta group(s) vs {} complete call(s)",
            indices.len(),
            complete.len()
        ),
    );

    let pairs = indices.len().min(complete.len());
    let mut args_ok = true;
    let mut name_ok = true;
    for k in 0..pairs {
        let (_id, name, args) = &groups[&indices[k]];
        let c = &complete[k];
        if *args != c.arguments {
            args_ok = false;
        }
        if let Some(n) = name {
            if n != &c.name {
                name_ok = false;
            }
        }
    }
    r.record(
        "reconstructed_args_match",
        args_ok,
        "concatenating ToolCallDelta.arguments per index must reproduce the complete ToolCall.arguments byte-for-byte",
    );
    r.record(
        "reconstructed_name_match",
        name_ok,
        "the streamed tool name (first non-empty delta name per index) must equal the complete ToolCall.name",
    );

    r
}
