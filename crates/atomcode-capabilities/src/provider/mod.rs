//! Real `LlmProvider` adapters (L1).
//!
//! The kernel's [`LlmProvider`](atomcode_kernel::provider::LlmProvider) trait is the
//! seam; these types implement it against real backends. Three adapters live here:
//!   - [`OpenAiCompatProvider`] — the **OpenAI-compatible** chat/completions surface
//!     (GLM / DeepSeek / any OpenAI-shaped endpoint);
//!   - [`AnthropicProvider`] — the **Anthropic Messages API** (`/v1/messages`, Claude),
//!     including the signed extended-thinking round-trip;
//!   - [`OllamaProvider`] — the **Ollama native** `/api/chat` (local models, NDJSON).
//!
//! Division of labour (mechanism vs policy):
//!   - the kernel owns the *mechanism* — neutral `Message`/`StreamEvent`/`ChatOptions`
//!     and lossless `reasoning` storage;
//!   - this adapter owns the *policy* — how each neutral knob maps onto the wire, how
//!     SSE deltas assemble into whole `ToolCall`s, and whether prior-turn reasoning is
//!     echoed back ([`ReasoningPolicy`]).

mod anthropic;
mod atomgit_sign;
pub mod discovery;
mod ollama;
mod openai_compat;
pub mod probe;
mod reasoning;
mod responses;
mod retry;
mod sign;

pub use anthropic::{AnthropicConfig, AnthropicProvider};
pub use atomgit_sign::{atomgit_request_signer, is_atomgit_gateway, signer_available};
pub use ollama::{OllamaConfig, OllamaProvider};
pub use openai_compat::{
    model_suggests_vision, reason_effort_applicable, OpenAiCompatConfig, OpenAiCompatProvider,
};
pub use reasoning::{ReasoningPolicy, REASONING_PLACEHOLDER};
pub use responses::ResponsesProvider;
pub use retry::RetryPolicy;
pub use sign::{RequestSigner, RequestSigningError, SignedAuth};

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// Fallback User-Agent when a provider config carries no explicit `user_agent`.
/// Bare (no version) on purpose: this crate is versioned independently of the
/// product (`0.0.0`), so a local `CARGO_PKG_VERSION` would be MISLEADING. The
/// host adapter injects the real `atomcode/<version>` via `*Config::user_agent`;
/// this fallback only applies to direct/test construction.
pub(crate) const DEFAULT_USER_AGENT: &str = "atomcode";

/// Process-local sequence so dumps sort in call order even when two land in the same
/// nanosecond (the timestamp alone isn't a tiebreaker under concurrency).
static WIRE_DUMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// BYTE-LEVEL outbound-request dump for wire diagnosis. No-op unless `ATOMCODE_WIRE_DUMP=1`.
/// Writes the EXACT JSON body an adapter built (post-projection, pre-send) to
/// `<dir>/<seq>-<ts>-<model>.req.json`, `dir` being the config's `wire_dump_dir` (the host
/// passes `<user tree>/wire-dump`). Best-effort: any failure (env unset, no dir, unwritable
/// dir) is silently ignored so diagnostics never break a real request.
///
/// This is the ADAPTER-level, provider-SPECIFIC counterpart to the neutral
/// [`WireLogHooks`](crate::hooks::WireLogHooks) (which logs the kernel `Message` view, not
/// these bytes). The kernel has NO byte seam by design — byte framing is intrinsically the
/// adapter's concern (each backend's JSON differs), so every adapter routes its built body
/// through here. Ported from core's v1 `ATOMCODE_WIRE_DUMP` (same env + `wire-dump/` dir).
pub(crate) fn wire_dump_request(dir: Option<&std::path::Path>, model: &str, body: &Value) {
    if std::env::var("ATOMCODE_WIRE_DUMP").ok().as_deref() != Some("1") {
        return;
    }
    if let Some(dir) = dir {
        wire_dump_to(dir, model, body);
    }
}

/// The pure writer behind [`wire_dump_request`] — testable without mutating the
/// process-global `$ATOMCODE_WIRE_DUMP`. Best-effort.
fn wire_dump_to(dir: &std::path::Path, model: &str, body: &Value) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}.{:09}", d.as_secs(), d.subsec_nanos()))
        .unwrap_or_default();
    let seq = WIRE_DUMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let safe_model: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{seq:06}-{ts}-{safe_model}.req.json"));
    if let Ok(s) = serde_json::to_string_pretty(body) {
        let _ = std::fs::write(&path, s);
    }
}

/// Push a `system` wire message, COALESCING it into the previous wire entry when that is
/// also a `system` message (joined with a blank line).
///
/// The kernel's neutral history can carry SEVERAL `Role::System` messages (persona +
/// `memory.md` + any future capability), but many OpenAI-compatible models / chat
/// templates accept only a SINGLE system message — extra ones are rejected outright or
/// silently honor just the first (dropping memory). Both `role:"system"`-on-the-wire
/// adapters use this helper so a model never sees more than one: OpenAI-compatible first
/// lifts every System message into a leading block, while Ollama coalesces the leading
/// contiguous run. (The Anthropic adapter instead lifts+joins all System messages into the
/// top-level `system` field — same guarantee, different wire shape.)
///
/// This helper itself coalesces CONSECUTIVE system entries only; callers that accept legacy
/// late System messages must lift them before calling it. It is pure and deterministic, so
/// the outgoing prefix stays byte-stable across rounds (cache-safe).
pub(crate) fn push_system_coalesced(out: &mut Vec<Value>, text: &str) {
    if let Some(last) = out.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some("system") {
            let prev = last.get("content").and_then(Value::as_str).unwrap_or("");
            let joined = if prev.is_empty() || text.is_empty() {
                format!("{prev}{text}")
            } else {
                format!("{prev}\n\n{text}")
            };
            last["content"] = json!(joined);
            return;
        }
    }
    out.push(json!({ "role": "system", "content": text }));
}

/// Map an HTTP error status to a plain-language headline so the TUI shows the
/// *cause*, not a bare `HTTP 401:` (which, when the server returns an empty
/// body, carried no hint at all). Shared by every provider protocol
/// (openai-compat, Anthropic/Claude, ollama, …) so the wording stays consistent
/// regardless of which wire format hit the error.
///
/// 402 gets a headline and its raw `detail` is deliberately DROPPED — the headline
/// already says it and this short form folds cleanly into the interrupted-turn
/// summary (`✗ 已中断：账户余额不足（HTTP 402）`). 401 gets a headline too but KEEPS
/// its detail: that detail is the only thing separating a genuinely bad key
/// (`invalid_api_key`) from a rejected request signature
/// (`invalid_client_signature`). Those two arrive under the same status and need
/// opposite fixes — re-authenticate, versus a client/gateway signing-path mismatch
/// that no amount of re-login will clear. Dropping it also starves downstream
/// classifiers: the headline is byte-identical for every 401, so anything reading
/// the message can only lump them into a single bucket.
/// One explicit CodingPlan entitlement rejection also gets an actionable `/login`
/// hint. Other 403 responses stay raw because AtomGit reuses that status for
/// session-concurrency conflicts and their structured reason must survive. 429
/// must keep the literal `HTTP 429: ` prefix the kernel rate-limit path
/// (`rate_limit_server_message`) strips. Everything else keeps
/// `HTTP {code}: {detail}` (the detail is the only signal there).
pub(crate) fn friendly_http_error(code: u16, detail: &str) -> String {
    if let Some(blocked) = content_blocked(detail) {
        return blocked;
    }
    if code == 403
        && detail
            .to_ascii_lowercase()
            .contains("user has no codingplan")
    {
        return "CodingPlan 未领取或已失效（HTTP 403）。请运行 /login 重新登录并领取 CodingPlan。"
            .to_string();
    }
    let headline = match code {
        401 => "API key 未授权或已失效",
        402 => "账户余额不足",
        _ => return format!("HTTP {code}: {detail}"),
    };
    // An empty body is exactly the case this headline was invented for, so append
    // nothing rather than leave a dangling separator.
    let detail = detail.trim();
    if code == 401 && !detail.is_empty() {
        return format!("{headline}（HTTP {code}）：{detail}");
    }
    format!("{headline}（HTTP {code}）")
}

/// The provider's content moderation refused this, said in words a person can
/// act on — or `None` when `detail` is not such a refusal.
///
/// A moderation refusal reads like any other provider error
/// (`[data_inspection_failed/data_inspection_failed] Output data may contain
/// inappropriate content.`), in English, with nothing saying it is the
/// provider's own judgement, that a resend often passes, or what else to try.
/// Matched on the provider's words rather than one field, because the same
/// refusal arrives as an HTTP 400 body, an in-band stream error or a 200 JSON
/// error depending on the provider and on which side was flagged. The provider's
/// text is kept at the end, code included, for whoever has to look into it.
pub(crate) fn content_blocked(detail: &str) -> Option<String> {
    const MARKS: &[&str] = &[
        // Alibaba Cloud DashScope / Bailian.
        "data_inspection_failed",
        // OpenAI, Azure OpenAI and the gateways that relay them.
        "content_filter",
        "content_policy_violation",
        "inappropriate content",
        // Zhipu GLM (code 1301).
        "不安全或敏感内容",
    ];
    let lower = detail.to_ascii_lowercase();
    if !MARKS.iter().any(|mark| lower.contains(mark)) {
        return None;
    }
    let output = if lower.contains("output data") || lower.contains("生成内容") {
        Some(true)
    } else if lower.contains("input data") || lower.contains("输入内容") {
        Some(false)
    } else {
        None
    };
    Some(
        atomcode_config::i18n::t(atomcode_config::i18n::Msg::ProviderContentBlocked {
            output,
            detail: detail.trim(),
        })
        .into_owned(),
    )
}

#[cfg(test)]
mod content_blocked_tests {
    use super::{content_blocked, friendly_http_error};

    /// The reported refusal: a DashScope output flag, said as what happened and
    /// what to do, with the provider's own words kept at the end. Checked in a
    /// way that holds in either language (the locale is process-global, and
    /// tests run side by side); the wording itself is `atomcode-i18n`'s.
    #[test]
    fn a_moderation_refusal_says_what_happened_and_what_to_do() {
        let raw = "[data_inspection_failed/data_inspection_failed] Output data may contain inappropriate content.";
        let said = content_blocked(raw).expect("recognised");
        assert!(said.ends_with(raw), "the provider's words are kept: {said}");
        assert!(said.contains("/model"), "it says what else to try: {said}");
        assert!(
            said.len() > raw.len() + 40,
            "it says more than the raw error: {said}"
        );

        // Which side was flagged is said: the reply, the request, or neither.
        let input = content_blocked("Input data may contain inappropriate content.").unwrap();
        let unsaid = content_blocked("[content_filter] filtered").unwrap();
        let strip = |s: &str, tail: &str| s.strip_suffix(tail).unwrap().to_string();
        let heads = [
            strip(&said, raw),
            strip(&input, "Input data may contain inappropriate content."),
            strip(&unsaid, "[content_filter] filtered"),
        ];
        assert!(heads[0] != heads[1] && heads[1] != heads[2] && heads[0] != heads[2]);

        // The same refusal sent as an HTTP error body goes the same way.
        assert_eq!(friendly_http_error(400, raw), said);
        // Other providers' spellings.
        assert!(content_blocked("[content_policy_violation] blocked").is_some());
        assert!(
            content_blocked("[1301] 系统检测到输入或生成内容可能包含不安全或敏感内容").is_some()
        );
    }

    /// Everything else is left exactly as it was.
    #[test]
    fn other_errors_are_not_read_as_moderation() {
        assert_eq!(content_blocked("[rate_limit_error] slow down"), None);
        assert_eq!(content_blocked("Incorrect API key provided"), None);
        assert_eq!(friendly_http_error(500, "boom"), "HTTP 500: boom");
    }
}

#[cfg(test)]
mod coalesce_tests {
    use super::push_system_coalesced;
    use serde_json::json;

    #[test]
    fn merges_runs_and_preserves_non_system_boundaries() {
        let mut out = Vec::new();
        push_system_coalesced(&mut out, "persona");
        push_system_coalesced(&mut out, "memory");
        assert_eq!(
            out,
            vec![json!({"role":"system","content":"persona\n\nmemory"})]
        );
        // A non-system entry breaks the run: a later system would start a fresh block.
        out.push(json!({"role":"user","content":"hi"}));
        push_system_coalesced(&mut out, "late");
        assert_eq!(
            out.len(),
            3,
            "system after a user is NOT merged into the leading block"
        );
        assert_eq!(out[2], json!({"role":"system","content":"late"}));
    }

    #[test]
    fn empty_text_does_not_inject_blank_separator() {
        let mut out = Vec::new();
        push_system_coalesced(&mut out, "");
        push_system_coalesced(&mut out, "real");
        assert_eq!(out, vec![json!({"role":"system","content":"real"})]);
    }
}

#[cfg(test)]
mod wire_dump_tests {
    use super::wire_dump_to;
    use serde_json::json;

    #[test]
    fn writes_body_as_req_json_into_dir() {
        let dir = tempfile::tempdir().unwrap();
        let body = json!({"model": "deepseek-v4", "messages": [{"role": "user", "content": "hi"}]});
        wire_dump_to(dir.path(), "deepseek-v4", &body);

        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 1, "one dump file written: {files:?}");
        let name = &files[0];
        assert!(name.ends_with(".req.json"), "req.json suffix: {name}");
        assert!(name.contains("deepseek-v4"), "model in filename: {name}");

        // The dumped bytes round-trip to the exact body (byte-level content preserved).
        let written = std::fs::read_to_string(dir.path().join(name)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed, body, "dumped JSON equals the outbound body");
    }

    #[test]
    fn model_name_is_filename_sanitized() {
        let dir = tempfile::tempdir().unwrap();
        // A slash / colon in a model id must not escape the dir or break the path.
        wire_dump_to(dir.path(), "org/model:v1", &json!({}));
        let name = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .into_owned();
        assert!(
            !name.contains('/') && !name.contains(':'),
            "unsafe chars stripped: {name}"
        );
        assert!(
            name.contains("org_model_v1"),
            "sanitized model retained: {name}"
        );
    }
}
