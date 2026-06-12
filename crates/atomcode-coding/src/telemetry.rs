//! Telemetry re-entry for the neutral kernel.
//!
//! The kernel emits NO telemetry by design. It exposes two seams the project's
//! observability rides on, and this module supplies an adapter for each:
//!   - [`TelemetryHook`] — TURN-level ([`LifecycleHooks`], the docs' "telemetry
//!     HOME"): one `LlmChat` per LLM round.
//!   - [`ToolTelemetryMiddleware`] — TOOL-level ([`ToolMiddleware`]): one `ToolCall`
//!     per executed tool.
//!
//! Both are registered by [`assemble`](crate::assemble)/[`prepare`](crate::prepare)
//! ONLY when `CodingAgentConfig::telemetry` is set, so a telemetry-free embedder
//! keeps a zero-telemetry kernel.
//!
//! LlmChat fidelity: token totals (input/output/cached), duration, context window,
//! tool-call count and messages-count are EXACT (from `MessageMeta` + the round's
//! request). `tool_def_tokens` is estimated (bytes/4, as the legacy path does). The
//! per-zone breakdown (`system_tokens` / `message_tokens` / `tool_result_tokens`) is
//! not computed by the kernel and is reported as 0 rather than guessed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use async_trait::async_trait;
use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message};
use atomcode_kernel::middleware::ToolMiddleware;
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall, ToolDef, ToolResult};
use atomcode_telemetry::{CurrentContext, Event, LlmErrorKind, Telemetry, ToolErrorKind};

/// Map a provider/stream error message onto a telemetry `LlmErrorKind`. Mirrors the
/// legacy `classify_llm_error` (which lives in core, off-limits here).
fn classify_llm_error(reason: &str) -> LlmErrorKind {
    let r = reason.to_lowercase();
    if r.contains("401") || r.contains("403") || r.contains("unauthorized") || r.contains("auth") {
        LlmErrorKind::AuthError
    } else if r.contains("429") || r.contains("rate") || r.contains("throttl") {
        LlmErrorKind::RateLimited
    } else if r.contains("500") || r.contains("502") || r.contains("503") {
        LlmErrorKind::ServerError
    } else if r.contains("stream timeout") || r.contains("no event for") {
        LlmErrorKind::StreamTimeout
    } else if r.contains("decode") || r.contains("mid-flight") || r.contains("terminated") {
        LlmErrorKind::StreamInterrupted
    } else if r.contains("context") || r.contains("max_tokens") || r.contains("token limit") {
        LlmErrorKind::ContextOverflow
    } else if r.contains("connect") || r.contains("dns") || r.contains("network") || r.contains("timeout")
    {
        LlmErrorKind::NetworkError
    } else {
        LlmErrorKind::Other
    }
}

/// Shared attribution + emit path for the telemetry adapters. Fixes the
/// provider/host/model envelope at assembly; both adapters track within its scope.
struct Attribution {
    telemetry: Arc<Telemetry>,
    provider: Option<String>,
    provider_host: Option<String>,
    model: String,
}

impl Attribution {
    /// `vendor` is the provider family (e.g. `"openai"`); `base_url` lets the
    /// telemetry envelope resolve a stable provider host.
    fn new(
        telemetry: Arc<Telemetry>,
        vendor: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
    ) -> Self {
        let vendor = vendor.into();
        let provider_host = atomcode_telemetry::resolve_provider_host(&vendor, Some(base_url));
        Self { telemetry, provider: Some(vendor), provider_host, model: model.into() }
    }

    fn scope_ctx(&self) -> CurrentContext {
        CurrentContext {
            provider: self.provider.clone(),
            provider_host: self.provider_host.clone(),
            model: Some(self.model.clone()),
            ..Default::default()
        }
    }

    async fn emit(&self, event: Event) {
        let tel = self.telemetry.clone();
        CurrentContext::scope(self.scope_ctx(), || async move {
            tel.track(event);
        })
        .await;
    }
}

/// Emits `Event::LlmChat` per round. Per-round figures come from the response's
/// `MessageMeta`; `messages_count` / `tool_def_tokens` come from the matching
/// `on_request`.
pub struct TelemetryHook {
    attr: Attribution,
    last_messages_count: AtomicU32,
    last_tool_def_bytes: AtomicU32,
    /// Last error observed via `on_error`, consumed by `turn_complete` to classify a
    /// failed turn. (`on_error` also fires for tool errors, but `turn_complete` only
    /// reads it on a PROVIDER terminal reason — so tool errors never become LlmChat.)
    last_error: StdMutex<Option<String>>,
}

impl TelemetryHook {
    pub fn new(
        telemetry: Arc<Telemetry>,
        vendor: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
    ) -> Self {
        Self {
            attr: Attribution::new(telemetry, vendor, base_url, model),
            last_messages_count: AtomicU32::new(0),
            last_tool_def_bytes: AtomicU32::new(0),
            last_error: StdMutex::new(None),
        }
    }
}

#[async_trait]
impl LifecycleHooks for TelemetryHook {
    async fn on_request(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        _options: &ChatOptions,
        _ctx: &TurnCtx,
    ) {
        self.last_messages_count
            .store(messages.len() as u32, Ordering::Relaxed);
        let bytes: usize = tools
            .iter()
            .map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len())
            .sum();
        self.last_tool_def_bytes.store(bytes as u32, Ordering::Relaxed);
    }

    async fn on_model_response(&self, response: &mut Message) {
        // No meta (e.g. a synthesized/empty response) ⇒ nothing to report.
        let Some(m) = response.meta.as_ref() else { return };
        let event = Event::LlmChat {
            duration_ms: m.elapsed_ms as u32,
            tool_calls_count: response.tool_calls.len() as u32,
            input_tokens: m.tokens.prompt,
            output_tokens: m.tokens.completion,
            cached_tokens: m.tokens.cached,
            had_error: false,
            context_window: m.ctx_window,
            // Not broken down by the kernel — only the totals above are exact.
            system_tokens: 0,
            tool_def_tokens: self.last_tool_def_bytes.load(Ordering::Relaxed) / 4,
            tool_result_tokens: 0,
            message_tokens: 0,
            messages_count: self.last_messages_count.load(Ordering::Relaxed),
            error_kind: None,
            error_data: None,
        };
        self.attr.emit(event).await;
    }

    async fn on_error(&self, error: &str) {
        // Remember the latest error; `turn_complete` decides if it was the cause of
        // a PROVIDER-level turn failure (tool errors terminate normally → ignored).
        if let Ok(mut g) = self.last_error.lock() {
            *g = Some(error.to_string());
        }
    }

    async fn turn_complete(&self, _convo: &Conversation, reason: &StopReason, _ctx: &TurnCtx) {
        let last = self.last_error.lock().ok().and_then(|mut g| g.take());
        // A round that produced a model response already emitted its LlmChat. Emit an
        // extra had_error one ONLY for an LLM/provider terminal failure — not normal
        // stop, cancel, or the round/continuation fuses (those aren't LLM errors).
        let kind = match reason {
            StopReason::ProviderError => {
                last.as_deref().map(classify_llm_error).unwrap_or(LlmErrorKind::Other)
            }
            StopReason::Timeout => LlmErrorKind::StreamTimeout,
            _ => return,
        };
        let event = Event::LlmChat {
            duration_ms: 0,
            tool_calls_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            had_error: true,
            context_window: 0,
            system_tokens: 0,
            tool_def_tokens: 0,
            tool_result_tokens: 0,
            message_tokens: 0,
            messages_count: self.last_messages_count.load(Ordering::Relaxed),
            error_kind: Some(kind),
            error_data: last.map(|e| e.chars().take(200).collect()),
        };
        self.attr.emit(event).await;
    }
}

/// Emits `Event::ToolCall` per executed tool. `before` stamps the start (keyed by
/// call id, so parallel batches don't collide); `after` computes the duration and
/// emits. Observation-only — never blocks or rewrites — so it registers AFTER the
/// approval middleware without touching the approve-what-runs contract.
pub struct ToolTelemetryMiddleware {
    attr: Attribution,
    /// call_id → (tool name, start). `ToolResult` carries no name, so it's stamped
    /// in `before` and looked up in `after`.
    inflight: StdMutex<HashMap<String, (String, Instant)>>,
}

impl ToolTelemetryMiddleware {
    pub fn new(
        telemetry: Arc<Telemetry>,
        vendor: impl Into<String>,
        base_url: &str,
        model: impl Into<String>,
    ) -> Self {
        Self {
            attr: Attribution::new(telemetry, vendor, base_url, model),
            inflight: StdMutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl ToolMiddleware for ToolTelemetryMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> Result<(), String> {
        if let Ok(mut m) = self.inflight.lock() {
            m.insert(call.id.clone(), (call.name.clone(), Instant::now()));
        }
        Ok(())
    }

    async fn after(&self, result: &mut ToolResult) {
        // No stamp ⇒ this middleware's `before` never ran (a prior middleware blocked
        // the call); nothing to attribute.
        let Some((name, started)) = self
            .inflight
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&result.call_id))
        else {
            return;
        };
        let success = !result.is_error;
        // The kernel sets a deterministic `content` for non-executing paths: a
        // middleware block (e.g. approval deny) → "blocked: …"; an unknown tool →
        // "unknown or unmounted tool…". Classify from that, else a real failure.
        let error_kind = if success {
            None
        } else if result.content.starts_with("blocked:") {
            Some(ToolErrorKind::DeniedByUser)
        } else if result.content.starts_with("unknown or unmounted tool") {
            Some(ToolErrorKind::NotFound)
        } else {
            Some(ToolErrorKind::ExecutionFailed)
        };
        let event = Event::ToolCall {
            name,
            success,
            duration_ms: started.elapsed().as_millis() as u32,
            error_kind,
            error_data: None,
        };
        self.attr.emit(event).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::MessageMeta;
    use atomcode_kernel::stream::TokenUsage;

    #[tokio::test]
    async fn emits_one_llm_chat_per_response_with_attribution() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(tel, "openai", "https://api.example.com/v1", "deepseek-v4");

        hook.on_request(
            &[Message::user("hi"), Message::user("there")],
            &[],
            &ChatOptions::default(),
            &TurnCtx::default(),
        )
        .await;

        let mut resp = Message::assistant("answer", vec![]);
        resp.meta = Some(MessageMeta {
            tokens: TokenUsage { prompt: 100, completion: 20, cached: 8 },
            elapsed_ms: 1234,
            ctx_window: 128_000,
            ..Default::default()
        });
        hook.on_model_response(&mut resp).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1, "exactly one LlmChat per response");
        match &records[0].event {
            Event::LlmChat {
                input_tokens,
                output_tokens,
                cached_tokens,
                duration_ms,
                context_window,
                messages_count,
                ..
            } => {
                assert_eq!(*input_tokens, 100);
                assert_eq!(*output_tokens, 20);
                assert_eq!(*cached_tokens, 8);
                assert_eq!(*duration_ms, 1234);
                assert_eq!(*context_window, 128_000);
                assert_eq!(*messages_count, 2, "captured from on_request");
            }
            other => panic!("expected LlmChat, got {other:?}"),
        }
        assert_eq!(records[0].envelope.provider.as_deref(), Some("openai"));
        assert_eq!(records[0].envelope.model.as_deref(), Some("deepseek-v4"));
    }

    #[tokio::test]
    async fn no_meta_emits_nothing() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(tel, "openai", "https://x/v1", "m");
        let mut resp = Message::assistant("hi", vec![]); // meta = None
        hook.on_model_response(&mut resp).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(captured.lock().await.is_empty());
    }

    #[tokio::test]
    async fn provider_error_turn_emits_had_error_llm_chat() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(tel, "openai", "https://x/v1", "m");

        hook.on_error("HTTP 429: rate limited").await;
        hook.turn_complete(&Conversation::default(), &StopReason::ProviderError, &TurnCtx::default())
            .await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        match &records[0].event {
            Event::LlmChat { had_error, error_kind, .. } => {
                assert!(*had_error);
                assert!(matches!(error_kind, Some(LlmErrorKind::RateLimited)));
            }
            other => panic!("expected had_error LlmChat, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn normal_stop_emits_no_extra_llm_chat() {
        let (tel, captured) = Telemetry::in_memory("test".into());
        let hook = TelemetryHook::new(tel, "openai", "https://x/v1", "m");
        // A tool error fired on_error, but the turn stopped normally → no LlmChat.
        hook.on_error("tool failed").await;
        hook.turn_complete(&Conversation::default(), &StopReason::Stopped, &TurnCtx::default())
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(captured.lock().await.is_empty());
    }

    #[tokio::test]
    async fn denied_tool_emits_tool_call_denied() {
        use atomcode_kernel::testkit::EchoTool;
        let (tel, captured) = Telemetry::in_memory("test".into());
        let mw = ToolTelemetryMiddleware::new(tel, "openai", "https://x/v1", "m");

        let tool: Arc<dyn Tool> = Arc::new(EchoTool);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, None);
        let mut call = ToolCall { id: "c9".into(), name: "bash".into(), arguments: "{}".into() };
        mw.before(&mut call, &tool, &rt).await.unwrap();
        // Approval denied upstream → the kernel hands `after` a "blocked: …" result.
        let mut result =
            ToolResult { call_id: "c9".into(), content: "blocked: user denied".into(), is_error: true };
        mw.after(&mut result).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        match &records[0].event {
            Event::ToolCall { name, success, error_kind, .. } => {
                assert_eq!(name, "bash");
                assert!(!success);
                assert!(matches!(error_kind, Some(ToolErrorKind::DeniedByUser)));
            }
            other => panic!("expected denied ToolCall, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_middleware_emits_tool_call() {
        use atomcode_kernel::testkit::EchoTool;
        let (tel, captured) = Telemetry::in_memory("test".into());
        let mw = ToolTelemetryMiddleware::new(tel, "openai", "https://x/v1", "m");

        let tool: Arc<dyn Tool> = Arc::new(EchoTool);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, None);
        let mut call = ToolCall { id: "c1".into(), name: "bash".into(), arguments: "{}".into() };
        mw.before(&mut call, &tool, &rt).await.unwrap();
        let mut result = ToolResult { call_id: "c1".into(), content: "ok".into(), is_error: false };
        mw.after(&mut result).await;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let records = captured.lock().await;
        assert_eq!(records.len(), 1);
        match &records[0].event {
            Event::ToolCall { name, success, .. } => {
                assert_eq!(name, "bash");
                assert!(*success);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert_eq!(records[0].envelope.model.as_deref(), Some("m"));
    }
}
