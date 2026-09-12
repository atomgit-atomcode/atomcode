//! Model adapters. Two providers, one slot — the config picks.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::provider::{OpenAiCompatConfig, OpenAiCompatProvider};
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use atomcode_plexus::{Context, Plugin};
use futures::stream::BoxStream;
use serde::Deserialize;
use serde_json::Value;

use crate::seams::{LlmSvc, LlmUtilitySvc};

/// An environment variable, treating empty as unset — an exported-but-blank
/// variable is a mistake, not a value.
fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[derive(Debug, Deserialize)]
struct OpenAiCompatRow {
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// Name of the environment variable holding the key. The key itself never
    /// belongs in a config tree that gets dumped, shared, and committed.
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    context_window: Option<u32>,
    /// Can this model accept images? Left unset, the answer comes from the
    /// model-name heuristic, which is a guess and cannot know about a gateway's
    /// own ids. A gateway route can disagree with it in either direction — a
    /// vision model with an unfamiliar name, or a text-only one whose name
    /// happens to look like a family the heuristic trusts — so this is the one
    /// place to overrule the guess. The patch target is the ROW id, `llm`, not
    /// this adapter's name, and `--patch` takes a file rather than a string:
    /// `[[patch]] id = "llm" config = { supports_vision = true }`.
    #[serde(default)]
    supports_vision: Option<bool>,
}

pub struct OpenAiCompatPlugin;

#[async_trait]
impl Plugin for OpenAiCompatPlugin {
    fn name(&self) -> &'static str {
        "llm-openai-compat"
    }
    fn uses(&self) -> &'static [&'static str] {
        // This row tells `describe_self` how to switch the model.
        &["operations"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn description(&self) -> &'static str {
        "OpenAI-compatible streaming adapter (DeepSeek, GLM, vLLM, gateways)"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: OpenAiCompatRow =
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?;
        let key_env = row
            .api_key_env
            .clone()
            .unwrap_or_else(|| "ATOMCODE_API_KEY".into());
        let base_url = row.base_url.or_else(|| env("ATOMCODE_BASE_URL"));
        let model = row.model.or_else(|| env("ATOMCODE_MODEL"));
        let api_key = env(&key_env);

        // Report every missing piece at once, and name the ways out. A failure
        // at mount time is the first thing a new user sees; telling them about
        // one of three missing variables wastes three attempts.
        let mut missing = Vec::new();
        if base_url.is_none() {
            missing.push("ATOMCODE_BASE_URL");
        }
        if model.is_none() {
            missing.push("ATOMCODE_MODEL");
        }
        if api_key.is_none() {
            missing.push(key_env.as_str());
        }
        if !missing.is_empty() {
            return Err(format!(
                "this row needs {}.\n  \
                 - already configured AtomCode? use the `llm-atomcode-config` row instead, \
                 which reads ~/.atomcode/config.toml\n  \
                 - just trying it out? add --offline for a scripted model that needs no key\n  \
                 - or set them: export {}=…",
                missing.join(", "),
                missing.join("=… ")
            ));
        }
        let (base_url, model, api_key) = (
            base_url.expect("checked"),
            model.expect("checked"),
            api_key.expect("checked"),
        );

        let provider = openai_compat(
            &api_key,
            &base_url,
            &model,
            row.context_window,
            row.supports_vision,
        )?;
        let _ = ctx
            .provide::<LlmSvc>(Arc::new(provider))
            .map_err(|e| e.to_string())?;
        crate::plugins::self_knowledge::describes(
            ctx,
            "model",
            5,
            format!(
                "MODEL. This tree talks to `{model}` at `{base_url}`, through the \
                 `llm-openai-compat` row reading ATOMCODE_BASE_URL / \
                 ATOMCODE_MODEL / {key_env}.\n\
                 To switch model: change ATOMCODE_MODEL and restart. To use the \
                 account already configured in AtomCode instead, drop \
                 `--env-model` so the `llm` row is `llm-atomcode-config`.\n\
                 Whether `{model}` can be sent images is guessed from its name. \
                 That guess cannot know a gateway's own ids, so if it is wrong \
                 either way, overrule it on the `llm` row: put \
                 `[[patch]] id = \"llm\" config = {{ supports_vision = true }}` \
                 in a file and pass it as `--patch <file>`.\n\
                 The model is NOT in the user-settings catalog on purpose — it \
                 is a row in the running tree, not a preference."
            ),
        );
        Ok(())
    }
}

/// A scripted provider: same seam, no network.
///
/// Its reason for existing is not testing convenience — it is the cheapest proof
/// that the slot is real. The loop, the tools, the approval policy and the
/// tracing all run unchanged against it, because none of them can tell which
/// plugin filled `llm`.
fn openai_compat(
    api_key: &str,
    base_url: &str,
    model: &str,
    context_window: Option<u32>,
    supports_vision: Option<bool>,
) -> Result<OpenAiCompatProvider, String> {
    let mut cfg = OpenAiCompatConfig::new(api_key, base_url, model);
    if let Some(window) = context_window {
        cfg.context_window = window;
    }
    // Over the heuristic `new()` just applied, and only when the row said so:
    // an explicit answer about one gateway route beats a guess made from the
    // model's name, in both directions.
    if let Some(vision) = supports_vision {
        cfg.supports_vision = vision;
    }
    OpenAiCompatProvider::new(cfg).map_err(|e| format!("provider init failed: {}", e.message))
}

/// The utility model, as its own row.
///
/// Side calls — a title, a summary, a suggestion — are one-off prompts with
/// no prefix to cache, so the model's unit price is their whole cost, and a
/// cheap one loses nothing. They also must not contend with the conversation:
/// a title request racing the first turn for one gateway's rate limit is a
/// 429 on the answer the person is waiting for. A separate slot, a separate
/// model, a separate budget.
///
/// `model` is required; `base_url` and the key fall back to the environment
/// the main row reads, so "same gateway, smaller model" is one line of config.
#[derive(Debug, Deserialize)]
struct UtilityRow {
    model: String,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key_env: Option<String>,
    #[serde(default)]
    context_window: Option<u32>,
}

pub struct LlmUtilityOpenAiCompatPlugin;

#[async_trait]
impl Plugin for LlmUtilityOpenAiCompatPlugin {
    fn name(&self) -> &'static str {
        "llm-utility-openai-compat"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm-utility"]
    }
    fn description(&self) -> &'static str {
        "a cheaper OpenAI-compatible model for side calls: titles, summaries, suggestions"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: UtilityRow =
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?;
        let key_env = row.api_key_env.unwrap_or_else(|| "ATOMCODE_API_KEY".into());
        let base_url = row
            .base_url
            .or_else(|| env("ATOMCODE_BASE_URL"))
            .ok_or("llm-utility-openai-compat needs `base_url` or ATOMCODE_BASE_URL")?;
        let api_key =
            env(&key_env).ok_or_else(|| format!("llm-utility-openai-compat needs {key_env}"))?;
        // No `supports_vision` here on purpose: a side call is a title or a
        // summary, and no side call carries an image, so this slot has nothing
        // for the flag to decide. A knob that cannot change an outcome is a
        // line of config someone has to think about for no reason.
        let provider = openai_compat(&api_key, &base_url, &row.model, row.context_window, None)?;
        let _ = ctx
            .provide::<LlmUtilitySvc>(Arc::new(provider))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// A scripted utility model, so a test can say what the side call answers
/// without the conversation's script being consumed by it.
pub struct LlmUtilityReplayPlugin;

#[async_trait]
impl Plugin for LlmUtilityReplayPlugin {
    fn name(&self) -> &'static str {
        "llm-utility-replay"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm-utility"]
    }
    fn description(&self) -> &'static str {
        "scripted side-call model — the utility seam with no network"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ReplayRow = if config.is_null() {
            ReplayRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let _ = ctx
            .provide::<LlmUtilitySvc>(Arc::new(ReplayProvider {
                script: row.script,
                cursor: std::sync::atomic::AtomicUsize::new(0),
                // Nothing is attached to a side call, so this slot has no
                // pictures to carry whatever the conversation model can do.
                vision: false,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Default)]
struct ReplayRow {
    /// Each step is one model turn: optional text, optional tool calls.
    #[serde(default)]
    script: Vec<ReplayStep>,
    /// Stand in for a model that can see pictures. FALSE by default, which is
    /// what a scripted stand-in honestly is: nothing here looks at an image.
    /// It is a knob rather than a constant because the alternative is an
    /// image-attachment path no offline tree can exercise in either direction —
    /// and this is the only tree the tests and `--offline` ever mount.
    #[serde(default)]
    supports_vision: bool,
}

#[derive(Debug, Deserialize, Clone, Default)]
struct ReplayStep {
    #[serde(default)]
    text: String,
    #[serde(default)]
    calls: Vec<ReplayCall>,
    /// Fail the request instead, with this message. The dead network, the
    /// missing key and the wrong URL all reach the loop as exactly this, and a
    /// front end owes the person the same two things in every case — the
    /// cause, and a status line that stops. Scripted here so that can be
    /// tested without unplugging anything.
    #[serde(default)]
    fail: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
struct ReplayCall {
    name: String,
    /// Arguments as a JSON object; serialized to the string a real model emits.
    #[serde(default)]
    args: Value,
}

struct ReplayProvider {
    script: Vec<ReplayStep>,
    cursor: std::sync::atomic::AtomicUsize,
    vision: bool,
}

#[async_trait]
impl LlmProvider for ReplayProvider {
    fn model_name(&self) -> &str {
        "replay"
    }

    fn context_window(&self) -> u32 {
        128_000
    }

    fn supports_vision(&self) -> bool {
        self.vision
    }

    async fn chat_stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolDef],
        _options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let index = self
            .cursor
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Past the end of the script, say so rather than answering with
        // nothing: an empty response is now a typed failure, and a fixture
        // running out is not a provider failure.
        let step = self
            .script
            .get(index)
            .cloned()
            .unwrap_or_else(|| ReplayStep {
                text: "(replay script exhausted)".into(),
                ..Default::default()
            });
        if let Some(message) = step.fail {
            return Err(ProviderError {
                retryable: false,
                message,
                ..Default::default()
            });
        }
        let mut events = Vec::new();
        if !step.text.is_empty() {
            // Chunked, so anything downstream that renders a live stream is
            // exercised the same way a real adapter exercises it.
            for word in step.text.split_inclusive(' ') {
                events.push(StreamEvent::TextDelta(word.to_string()));
            }
        }
        for (i, call) in step.calls.iter().enumerate() {
            events.push(StreamEvent::ToolCall(ToolCall {
                id: format!("replay-{index}-{i}"),
                name: call.name.clone(),
                arguments: call.args.to_string(),
            }));
        }
        events.push(StreamEvent::Usage(TokenUsage {
            prompt: 100,
            completion: 20,
            ..Default::default()
        }));
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

pub struct ReplayPlugin;

#[async_trait]
impl Plugin for ReplayPlugin {
    fn name(&self) -> &'static str {
        "llm-replay"
    }
    fn uses(&self) -> &'static [&'static str] {
        // This row tells `describe_self` how to switch the model.
        &["operations"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn description(&self) -> &'static str {
        "scripted model responses — the same seam with no network"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ReplayRow = if config.is_null() {
            ReplayRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let _ = ctx
            .provide::<LlmSvc>(Arc::new(ReplayProvider {
                script: {
                    crate::plugins::self_knowledge::describes(
                        ctx,
                        "model",
                        5,
                        format!(
                            "MODEL. There is no model. The `llm` row is \
                             `llm-replay`, a scripted stand-in with {} canned \
                             answer(s) and no network — used by `--offline` and \
                             by every test. Nothing you say reaches a provider. \
                             To talk to a real one, restart without `--offline`, \
                             or with `--env-model` plus ATOMCODE_BASE_URL / \
                             ATOMCODE_MODEL / ATOMCODE_API_KEY.",
                            row.script.len()
                        ),
                    );
                    row.script
                },
                cursor: std::sync::atomic::AtomicUsize::new(0),
                vision: row.supports_vision,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- the provider the user already configured ---------------------------

/// Reads `~/.atomcode/config.toml` and mounts the model selected there.
///
/// A person running this has usually configured AtomCode already; asking them
/// to restate the same endpoint as three environment variables is asking them
/// to maintain it twice. The config crate is a leaf, so reading it costs the
/// harness nothing architecturally — and the seam is unchanged either way: this
/// row fills `llm` exactly like the env-driven one, and every consumer is
/// unaware of which is mounted.
#[derive(Debug, Deserialize, Default)]
struct ConfigRow {
    /// Which selection to mount (a `[models.*]` key). Omitted means the
    /// selection the user has made their default.
    #[serde(default)]
    model: Option<String>,
    /// Read config from here instead of the default location.
    #[serde(default)]
    path: Option<String>,
}

pub struct AtomcodeConfigPlugin;

#[async_trait]
impl Plugin for AtomcodeConfigPlugin {
    fn name(&self) -> &'static str {
        "llm-atomcode-config"
    }
    fn uses(&self) -> &'static [&'static str] {
        // This row tells `describe_self` how to switch the model.
        &["operations"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn description(&self) -> &'static str {
        "the model AtomCode is already configured to use (~/.atomcode/config.toml)"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ConfigRow = if config.is_null() {
            ConfigRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let path = row
            .path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(atomcode_config::config::Config::default_path);
        if !path.exists() {
            return Err(format!(
                "{} does not exist.\n  \
                 - run AtomCode once to configure a provider, or\n  \
                 - patch the `llm` row to `llm-openai-compat` and set ATOMCODE_BASE_URL / \
                 ATOMCODE_MODEL / ATOMCODE_API_KEY, or\n  \
                 - add --offline for a scripted model that needs no key",
                path.display()
            ));
        }

        let cfg = atomcode_config::config::Config::load(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let resolved = cfg
            .resolve_model(row.model.as_deref())
            .map_err(|e| format!("{}: {e}", path.display()))?;

        let base_url = resolved
            .base_url
            .ok_or_else(|| format!("account `{}` has no base_url", resolved.account_id))?;
        // A managed account may hold its credential elsewhere (an OAuth token
        // store). Say which account, so the fix is obvious.
        let api_key = resolved
            .api_key
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| {
                format!(
                    "account `{}` has no usable api_key in {} — log in with AtomCode, or pick \
                 another model with `config = {{ model = \"…\" }}` on this row",
                    resolved.account_id,
                    path.display()
                )
            })?;

        let mut provider_cfg = OpenAiCompatConfig::new(&api_key, &base_url, &resolved.model);
        provider_cfg.context_window = resolved.context_window as u32;
        provider_cfg.supports_vision = resolved.supports_vision;
        let provider = OpenAiCompatProvider::new(provider_cfg)
            .map_err(|e| format!("provider init failed: {}", e.message))?;
        eprintln!(
            "\x1b[2musing `{}` ({}) from {}\x1b[0m",
            resolved.selection_id,
            resolved.model,
            path.display()
        );
        crate::plugins::self_knowledge::describes(
            ctx,
            "model",
            5,
            format!(
                "MODEL. This tree uses selection `{}` (model `{}`) from `{}`, via \
                 the `llm-atomcode-config` row.\n\
                 To switch model: change the selection in that file (the \
                 `/model` picker writes it), or put `config = {{ model = \"…\" }}` \
                 on the `llm` row.\n\
                 To use an arbitrary OpenAI-compatible endpoint instead, launch \
                 with `--env-model` and set ATOMCODE_BASE_URL / ATOMCODE_MODEL / \
                 ATOMCODE_API_KEY — that swaps this row for `llm-openai-compat`.\n\
                 The model is NOT in the user-settings catalog on purpose — it \
                 is a row in the running tree, not a preference.",
                resolved.selection_id,
                resolved.model,
                path.display()
            ),
        );
        let _ = ctx
            .provide::<LlmSvc>(Arc::new(provider))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The row's `supports_vision` is the one place a person can overrule the
    // name heuristic, so it has to beat it in BOTH directions. The heuristic is
    // a guess about a name; the row is a statement about a route, and a route
    // can be a vision model the heuristic has never heard of, or a text-only
    // one whose name looks like a family it trusts.
    #[test]
    fn the_row_overrules_the_name_heuristic_in_both_directions() {
        let forced_on =
            openai_compat("k", "https://gw/v1", "deepseek-v4", None, Some(true)).expect("build");
        assert!(
            forced_on.supports_vision(),
            "an explicit true must beat a text-only-looking name"
        );

        let forced_off =
            openai_compat("k", "https://gw/v1", "gpt-4o", None, Some(false)).expect("build");
        assert!(
            !forced_off.supports_vision(),
            "an explicit false must beat a vision-looking name"
        );
    }

    #[test]
    fn an_unset_row_leaves_the_heuristic_in_charge() {
        let unset = openai_compat("k", "https://gw/v1", "gpt-4o", None, None).expect("build");
        assert!(unset.supports_vision(), "unset is not the same as false");
    }

    #[test]
    fn the_row_parses_the_override_and_defaults_to_unset() {
        let set: OpenAiCompatRow = serde_json::from_value(serde_json::json!({
            "supports_vision": false
        }))
        .expect("parse with override");
        assert_eq!(set.supports_vision, Some(false));

        // Absent means "ask the heuristic", and it must not be read as `false`.
        let absent: OpenAiCompatRow =
            serde_json::from_value(serde_json::json!({})).expect("parse without override");
        assert_eq!(absent.supports_vision, None);
    }
}
