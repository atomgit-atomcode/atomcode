//! Telemetry for the standalone CLI.
//!
//! `atomcodex` is decoupled from atomcode-core, so it builds its OWN telemetry sink
//! (mirroring atomcode-cli's resolve → init) and wires it into both subcommands:
//!   - `code` sets [`CodingAgentConfig::telemetry`](atomcode_coding::CodingAgentConfig),
//!     lighting up the full host-loop instrumentation (the turn-level TelemetryHook + tool
//!     middleware + the review-slot `MeteredProvider`);
//!   - `review` runs a STANDALONE review agent with NO turn-level hook, so its provider is
//!     wrapped with [`meter_provider`] here — otherwise its LLM rounds emit no telemetry and
//!     the review's token spend is invisible (the whole point of this module).
//!
//! Opt-out is honored by `resolve`: `DO_NOT_TRACK=1`, `ATOMCODE_TELEMETRY=0`,
//! `--no-telemetry`, or `[telemetry] enabled = false` in config.toml. A disabled sink makes
//! every `track` a no-op, so callers wire telemetry UNCONDITIONALLY and the sink self-gates.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use atomcode_kernel::provider::LlmProvider;
use atomcode_telemetry::config::{resolve, CliOverride, ProcessEnv, TelemetryConfig};
use atomcode_telemetry::Telemetry;
use serde::Deserialize;

/// How long to wait for the disk queue to drain when flushing on exit.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(3);

const NOTICE: &str = "atomcodex is sending anonymous usage telemetry (token counts, \
    tool/latency stats — no source code or prompt text). Opt out with DO_NOT_TRACK=1, \
    ATOMCODE_TELEMETRY=0, --no-telemetry, or `[telemetry] enabled = false` in config.toml.";

/// `~/.atomcode` (honors `$ATOMCODE_HOME`, else `$HOME` / `%USERPROFILE%`). This is the
/// telemetry queue root — the SAME dir the main app uses, so clix events land in the shared
/// on-disk queue and ride its uploader. Falls back to `./.atomcode` when no home is known.
fn atomcode_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("ATOMCODE_HOME") {
        return PathBuf::from(home);
    }
    match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        Some(h) => PathBuf::from(h).join(".atomcode"),
        None => PathBuf::from(".atomcode"),
    }
}

#[derive(Deserialize, Default)]
struct TelFile {
    #[serde(default)]
    telemetry: TelemetryConfig,
}

/// Read the `[telemetry]` section from the config file (default `~/.atomcode/config.toml`,
/// or `config_override`). Any read/parse failure → defaults — telemetry is non-fatal, exactly
/// as atomcode-cli treats it (a broken config must not break `review`/`code`).
fn load_telemetry_config(config_override: Option<&Path>) -> TelemetryConfig {
    let path = match config_override {
        Some(p) => p.to_path_buf(),
        None => match crate::default_config_path() {
            Some(p) => p,
            None => return TelemetryConfig::default(),
        },
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return TelemetryConfig::default(),
    };
    toml::from_str::<TelFile>(&text).map(|f| f.telemetry).unwrap_or_default()
}

/// Build the telemetry sink: resolve the 4-level opt-out, then init the queue. ALWAYS returns
/// a sink — a DISABLED one when opted out (every `track` is then a hard no-op), so callers
/// wire it unconditionally and the sink itself enforces the opt-out.
pub fn build_sink(config_override: Option<&Path>, no_telemetry: bool) -> Arc<Telemetry> {
    let cfg = load_telemetry_config(config_override);
    let resolved = resolve(&cfg, &CliOverride { disabled: no_telemetry }, atomcode_dir(), &ProcessEnv);
    Telemetry::init(resolved, env!("CARGO_PKG_VERSION").into())
}

/// Wrap a provider so each LLM round emits one `LlmChat` (token spend) to `sink`. The
/// standalone `review` agent has no turn-level TelemetryHook, so without this its rounds are
/// invisible. `"openai"` is the only provider family atomcodex builds (OpenAI-compatible); the
/// review run is sessionless, so attribution carries no session id. A disabled sink no-ops.
pub fn meter_provider(
    inner: Arc<dyn LlmProvider>,
    sink: &Arc<Telemetry>,
    base_url: &str,
    model: &str,
) -> Arc<dyn LlmProvider> {
    Arc::new(
        atomcode_coding::telemetry::MeteredProvider::new(
            inner,
            sink.clone(),
            "openai",
            base_url,
            model,
            None,
        )
        // Tag with surface="code_review" so the standalone reviewer's spend is
        // attributable in telemetry, matching the in-session `code_review` tool.
        .with_surface("code_review"),
    )
}

/// Build the OpenAI-compatible provider for the standalone `review` agent (mirrors
/// `atomcode_review::build_review_agent`'s internal construction, but lets us wrap it with
/// telemetry before handing it to `build_review_agent_with`).
pub fn build_review_provider(
    cfg: &atomcode_review::ReviewAgentConfig,
) -> Result<Arc<dyn LlmProvider>, String> {
    use atomcode_capabilities::provider::{OpenAiCompatConfig, OpenAiCompatProvider};
    let mut pc = OpenAiCompatConfig::new(&cfg.api_key, &cfg.base_url, &cfg.model);
    pc.context_window = cfg.context_window;
    OpenAiCompatProvider::new(pc)
        .map(|p| Arc::new(p) as Arc<dyn LlmProvider>)
        .map_err(|e| e.message)
}

/// One-time consent notice: the FIRST time telemetry is active, print [`NOTICE`] to stderr
/// and drop a marker dotfile in the queue dir so later runs stay quiet. No-op when disabled.
pub fn maybe_show_notice(enabled: bool) {
    if enabled && notice_once(&atomcode_dir()) {
        eprintln!("{NOTICE}");
    }
}

/// Returns true exactly once per `dir`: shows-and-marks. A write failure returns false (don't
/// claim we notified when we couldn't persist the mark — better a repeat than a silent enable).
fn notice_once(dir: &Path) -> bool {
    let marker = dir.join(".clix_telemetry_notice");
    if marker.exists() {
        return false;
    }
    let _ = std::fs::create_dir_all(dir);
    std::fs::write(&marker, b"shown\n").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::provider::ChatOptions;
    use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
    use atomcode_kernel::tool::ToolDef;
    use futures::stream::{BoxStream, StreamExt};

    /// A canned provider that reports usage then ends — enough for the metering decorator to
    /// fold a `TokenUsage` and emit one `LlmChat`.
    struct CannedProvider;
    #[async_trait::async_trait]
    impl LlmProvider for CannedProvider {
        fn model_name(&self) -> &str {
            "m"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[ToolDef],
            _: &ChatOptions,
        ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
            Ok(Box::pin(futures::stream::iter(vec![
                StreamEvent::TextDelta("ok".into()),
                StreamEvent::Usage(TokenUsage { prompt: 400, completion: 20, cached: 0 }),
                StreamEvent::Done { truncated: false },
            ])))
        }
    }

    #[tokio::test]
    async fn meter_provider_emits_llm_chat_for_review_rounds() {
        let (sink, captured) = Telemetry::in_memory("test".into());
        let metered = meter_provider(Arc::new(CannedProvider), &sink, "https://x/v1", "m");
        let mut s = metered
            .chat_stream(&[Message::user("hi")], &[], &ChatOptions::default())
            .await
            .unwrap();
        while s.next().await.is_some() {}

        tokio::time::sleep(Duration::from_millis(50)).await;
        let n = captured
            .lock()
            .await
            .iter()
            .filter(|r| matches!(r.event, atomcode_telemetry::Event::LlmChat { .. }))
            .count();
        assert_eq!(n, 1, "the standalone review provider must meter each LLM round");
    }

    #[test]
    fn parses_telemetry_opt_out_from_config() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "default_provider=\"x\"\n[telemetry]\nenabled=false\n").unwrap();
        assert_eq!(load_telemetry_config(Some(&p)).enabled, Some(false));
    }

    #[test]
    fn missing_telemetry_section_defaults_to_unset() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        std::fs::write(&p, "default_provider=\"x\"\n[providers.x]\nmodel=\"m\"\n").unwrap();
        // None ⇒ `resolve` leaves it Enabled (the built-in default).
        assert_eq!(load_telemetry_config(Some(&p)).enabled, None);
    }

    #[test]
    fn notice_shows_once_then_marks() {
        let d = tempfile::tempdir().unwrap();
        assert!(notice_once(d.path()), "first time → show");
        assert!(!notice_once(d.path()), "subsequent → suppressed");
    }
}
