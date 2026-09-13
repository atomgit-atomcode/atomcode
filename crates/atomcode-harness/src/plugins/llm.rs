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

use crate::model_source;
use crate::seams::{LlmSvc, LlmUtilitySvc};

/// The half of a model description that is a measurement rather than a knob:
/// how large a window the mounted provider reports, and where that number came
/// from.
///
/// Shared by every row that can fill `llm`, because whoever asks how big the
/// context is must get the same sentence and the same number whichever adapter
/// happens to be mounted. The number is the provider's own `context_window()`,
/// not the row's config, so it cannot disagree with what compaction divides by.
///
/// `origin` is not decoration. A window someone configured and a fallback that
/// happens to look configured are the same `u32`; only the row knows which one
/// it handed over, and a reader who cannot tell them apart will trust a
/// default as if it were a fact about the model.
/// See [`OpenAiCompatRow::supports_reasoning_effort`] for why this is `true`.
fn default_supports_effort() -> bool {
    true
}

fn context_line(window: u32, origin: &str) -> String {
    format!(
        "Context window: {window} tokens ({origin}). Auto-compaction fires as a \
         fraction of this window, so a window smaller than the model's real one \
         compacts early and a larger one overruns the provider."
    )
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
    /// `thinking.type` in the request body — the reasoning switch, for a
    /// gateway that has one. `"disabled"` is what a reasoning model is told to
    /// skip its chain of thought, and it is the only lever that turns reasoning
    /// *off* rather than down: `reasoning_effort` has no `none` step, and the
    /// ladder is not what starves a short side call.
    ///
    /// Opaque on purpose. Gateways disagree on the vocabulary and on whether
    /// they accept the object at all, so the row passes the string through
    /// rather than this crate guessing — and the default stays `None`, which
    /// omits the whole object, because a gateway that does not know `thinking`
    /// may reject the request outright. Setting this opts into that risk; it is
    /// never on by itself.
    #[serde(default)]
    thinking_type: Option<String>,
    /// `thinking.keep` — Kimi K2.6 preserved thinking. Omitted unless set.
    #[serde(default)]
    thinking_keep: Option<String>,
    /// Does this route accept a top-level `reasoning_effort`? Endpoint
    /// capability, not a level — and unlike [`AtomcodeConfigPlugin`], this row is
    /// hand-written and names an arbitrary OpenAI-compatible gateway, so there
    /// is no `[models.*]` entry to read it from. Default `true`: an endpoint a
    /// person wired up by hand is assumed to take the field the reasoning-effort
    /// row offers, and if it does not, the adapter remembers the rejection for
    /// the session and stops sending it. Defaulting to `false` instead would
    /// make a configured level silently do nothing, which is the one failure
    /// this whole feature exists to avoid.
    #[serde(default = "default_supports_effort")]
    supports_reasoning_effort: bool,
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
        // One source, resolved in one place. A row may state its own endpoint;
        // what it leaves out comes from the environment, with the fallbacks and
        // the error text owned by `model_source` rather than re-derived here.
        // One implementation of "where a model comes from", asked rather than
        // re-derived: this row says only what it knows, and `ConfigAndEnv` owns
        // the config file, the environment and how they rank.
        let source = model_source::ConfigAndEnv {
            home: crate::home(),
        };
        let want = match (row.base_url.as_deref(), row.model.as_deref()) {
            (Some(base_url), Some(model)) => model_source::Want::Explicit {
                base_url,
                model,
                // What the row said, and nothing more: which variable to use
                // when a row names none is a policy, and it lives in
                // `model_source` with the rest of the resolution.
                api_key_env: row.api_key_env.as_deref(),
            },
            _ => model_source::Want::Environment {
                api_key_env: row.api_key_env.as_deref(),
            },
        };
        let resolved = source.resolve(want)?;
        let (base_url, model, api_key) = (
            resolved.base_url.clone(),
            resolved.model.clone(),
            resolved.api_key.clone(),
        );
        let provider = openai_compat(
            &api_key,
            &base_url,
            &model,
            AdapterKnobs {
                // The row's own value wins; the environment source has none, and
                // the row is where it is stated.
                context_window: row.context_window.or(resolved.context_window),
                supports_vision: row.supports_vision.or(resolved.supports_vision),
                thinking_type: row.thinking_type.as_deref(),
                thinking_keep: row.thinking_keep.as_deref(),
                supports_reasoning_effort: Some(
                    row.supports_reasoning_effort || resolved.supports_reasoning_effort,
                ),
            },
        )?;
        // Read the window off the provider, not off the row: the row is an
        // `Option`, the provider is the resolved number, and the resolved number
        // is what the compaction trigger divides by. Reported from the raw
        // config, the two could disagree exactly when the config is silent —
        // which is the case a reader most needs told apart.
        let window = provider.context_window();
        let window_origin = match row.context_window {
            Some(_) => "stated on the `llm` row".to_string(),
            None => "the adapter's fallback, because this row states no \
                     `context_window` — nothing in this tree knows what this \
                     model really holds"
                .to_string(),
        };
        let _ = ctx
            .provide::<LlmSvc>(Arc::new(provider))
            .map_err(|e| e.to_string())?;
        // For the sentence only: which variable holds the key, so the person can
        // go set it. The *decision* is `DEFAULT_API_KEY_ENV`'s; this just shows
        // whichever name the resolution used.
        let key_var = row
            .api_key_env
            .as_deref()
            .unwrap_or(model_source::DEFAULT_API_KEY_ENV);
        crate::plugins::self_knowledge::describes(
            ctx,
            "model",
            5,
            format!(
                "MODEL. This tree talks to `{model}` at `{base_url}`, through the \
                 `llm-openai-compat` row reading ATOMCODE_BASE_URL / \
                 ATOMCODE_MODEL / {key_var}.\n\
                 {}\n\
                 To set it, the patch target is the ROW id, `llm`, and `--patch` \
                 takes a file rather than a string: \
                 `[[patch]] id = \"llm\" config = {{ context_window = 1000000 }}`.\n\
                 To switch model: change ATOMCODE_MODEL and restart. To use the \
                 account already configured in AtomCode instead, drop \
                 `--env-model` so the `llm` row is `llm-atomcode-config`.\n\
                 Whether `{model}` can be sent images is guessed from its name. \
                 That guess cannot know a gateway's own ids, so if it is wrong \
                 either way, overrule it on the `llm` row: put \
                 `[[patch]] id = \"llm\" config = {{ supports_vision = true }}` \
                 in a file and pass it as `--patch <file>`.\n\
                 The model is NOT in the user-settings catalog on purpose — it \
                 is a row in the running tree, not a preference.",
                context_line(window, &window_origin)
            ),
        );
        Ok(())
    }
}

/// The row-level knobs that reach the adapter, gathered so a caller adding one
/// does not have to re-thread a positional list through every construction site
/// and test. `None` on any field means "the row said nothing", which is what
/// leaves the adapter's own default standing.
#[derive(Debug, Default)]
struct AdapterKnobs<'a> {
    context_window: Option<u32>,
    supports_vision: Option<bool>,
    thinking_type: Option<&'a str>,
    thinking_keep: Option<&'a str>,
    /// Whether this route accepts a `reasoning_effort` at all — endpoint
    /// capability, not a level. `None` means "the row did not say", and what
    /// that defaults to differs by row: see the call sites.
    supports_reasoning_effort: Option<bool>,
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
    knobs: AdapterKnobs<'_>,
) -> Result<OpenAiCompatProvider, String> {
    let mut cfg = OpenAiCompatConfig::new(api_key, base_url, model);
    if let Some(window) = knobs.context_window {
        cfg.context_window = window;
    }
    // Over the heuristic `new()` just applied, and only when the row said so:
    // an explicit answer about one gateway route beats a guess made from the
    // model's name, in both directions.
    if let Some(vision) = knobs.supports_vision {
        cfg.supports_vision = vision;
    }
    // Straight through, unresolved: these are the gateway's vocabulary, not
    // ours. Left unset, the adapter omits the `thinking` object entirely, which
    // is the only form every gateway is known to accept.
    if let Some(kind) = knobs.thinking_type {
        cfg.thinking_type = Some(kind.to_string());
    }
    if let Some(keep) = knobs.thinking_keep {
        cfg.thinking_keep = Some(keep.to_string());
    }
    if let Some(accepts) = knobs.supports_reasoning_effort {
        cfg.supports_reasoning_effort = accepts;
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
    /// Same two knobs, same pass-through, as on the conversation row — spelled
    /// out rather than shared, because a side call is exactly where the switch
    /// earns its keep: a title asked of a reasoning model spends its whole
    /// budget thinking and comes back with no answer at all.
    #[serde(default)]
    thinking_type: Option<String>,
    #[serde(default)]
    thinking_keep: Option<String>,
    /// Does this route accept a top-level `reasoning_effort`? Same field, same
    /// default and the same reasoning as on the conversation row — and it
    /// matters here for the same reason: the simple team roles put their tier on
    /// requests that this slot serves, so without it a role's `effort: low`
    /// would be set and then silently dropped at the adapter. Whether it has an
    /// effect in a given tree is the tree's business (with `thinking_type`
    /// `disabled` the provider passes no thinking at all), but being *dropped*
    /// is not something a row should do quietly.
    #[serde(default = "default_supports_effort")]
    supports_reasoning_effort: bool,
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
        // The same one source as the conversation row, so "same gateway,
        // smaller model" falls back identically in both. Only the model is
        // required here; the endpoint and the key are the ones the tree is
        // already using unless this row overrides them.
        let source = model_source::ConfigAndEnv {
            home: crate::home(),
        };
        let want = match row.base_url.as_deref() {
            Some(base_url) => model_source::Want::Explicit {
                base_url,
                model: &row.model,
                api_key_env: row.api_key_env.as_deref(),
            },
            None => model_source::Want::EnvironmentWithModel {
                api_key_env: row.api_key_env.as_deref(),
                model: &row.model,
            },
        };
        let endpoint = source.resolve(want)?;
        // No `supports_vision` here on purpose: a side call is a title or a
        // summary, and no side call carries an image, so this slot has nothing
        // for the flag to decide. A knob that cannot change an outcome is a
        // line of config someone has to think about for no reason.
        let provider = openai_compat(
            &endpoint.api_key,
            &endpoint.base_url,
            &endpoint.model,
            AdapterKnobs {
                context_window: row.context_window.or(endpoint.context_window),
                thinking_type: row.thinking_type.as_deref(),
                thinking_keep: row.thinking_keep.as_deref(),
                supports_reasoning_effort: Some(row.supports_reasoning_effort),
                ..AdapterKnobs::default()
            },
        )?;
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
                // A side call has no conversation to compact, so no consumer
                // reads this slot's window.
                context_window: row.context_window.unwrap_or(128_000),
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
    /// How large a window the stand-in claims. `None` keeps the built-in
    /// 128k. Same reasoning as `supports_vision` above: the window is what the
    /// compaction trigger divides by, so without a knob here no offline tree
    /// can exercise compaction at a window other than 128k — including the
    /// case that matters, a long-context model where the default would compact
    /// far too early.
    #[serde(default)]
    context_window: Option<u32>,
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
    context_window: u32,
}

#[async_trait]
impl LlmProvider for ReplayProvider {
    fn model_name(&self) -> &str {
        "replay"
    }

    fn context_window(&self) -> u32 {
        self.context_window
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
        // One binding feeds both the provider and the description. There is no
        // adapter `new()` in between to resolve anything, so the number the
        // provider reports IS this binding — described and enforced cannot
        // drift.
        let window = row.context_window.unwrap_or(128_000);
        let window_origin = match row.context_window {
            Some(_) => "stated on the `llm` row",
            None => "the built-in default, because this row states no `context_window`",
        };
        crate::plugins::self_knowledge::describes(
            ctx,
            "model",
            5,
            format!(
                "MODEL. There is no model. The `llm` row is \
                 `llm-replay`, a scripted stand-in with {} canned \
                 answer(s) and no network — used by `--offline` and \
                 by every test. Nothing you say reaches a provider.\n\
                 {}\n\
                 To talk to a real one, restart without `--offline`, \
                 or with `--env-model` plus ATOMCODE_BASE_URL / \
                 ATOMCODE_MODEL / ATOMCODE_API_KEY.",
                row.script.len(),
                context_line(window, window_origin)
            ),
        );
        let _ = ctx
            .provide::<LlmSvc>(Arc::new(ReplayProvider {
                script: row.script,
                cursor: std::sync::atomic::AtomicUsize::new(0),
                vision: row.supports_vision,
                context_window: window,
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

        // Resolution lives in `model_source`; this row only says *which* source
        // it wants and which `[models.*]` selection. The file itself, and the
        // account/key rules around it, are read in one place.
        let source = model_source::ConfigAndEnv {
            home: crate::home(),
        };
        let endpoint = source.resolve(model_source::Want::UserConfig {
            selection: row.model.as_deref(),
        })?;

        let mut provider_cfg =
            OpenAiCompatConfig::new(&endpoint.api_key, &endpoint.base_url, &endpoint.model);
        if let Some(window) = endpoint.context_window {
            provider_cfg.context_window = window;
        }
        if let Some(vision) = endpoint.supports_vision {
            provider_cfg.supports_vision = vision;
        }
        provider_cfg.thinking_type = endpoint.thinking_type.clone();
        provider_cfg.thinking_keep = endpoint.thinking_keep.clone();
        provider_cfg.supports_reasoning_effort = endpoint.supports_reasoning_effort;
        let provider = OpenAiCompatProvider::new(provider_cfg)
            .map_err(|e| format!("provider init failed: {}", e.message))?;
        eprintln!(
            "\x1b[2musing `{}` from {}\x1b[0m",
            endpoint.model, endpoint.origin
        );
        crate::plugins::self_knowledge::describes(
            ctx,
            "model",
            5,
            format!(
                "MODEL. This tree uses selection `{}` (model `{}`) from `{}`, via \
                 the `llm-atomcode-config` row.\n\
                 {}\n\
                 To change it, put `context_window` in that model's `[models.*]` \
                 entry; this row reads the window from there and reports it.\n\
                 To switch model: change the selection in that file (the \
                 `/model` picker writes it), or put `config = {{ model = \"…\" }}` \
                 on the `llm` row.\n\
                 To use an arbitrary OpenAI-compatible endpoint instead, launch \
                 with `--env-model` and set ATOMCODE_BASE_URL / ATOMCODE_MODEL / \
                 ATOMCODE_API_KEY — that swaps this row for `llm-openai-compat`.\n\
                 The model is NOT in the user-settings catalog on purpose — it \
                 is a row in the running tree, not a preference.",
                row.model.as_deref().unwrap_or("(the configured default)"),
                endpoint.model,
                endpoint.origin,
                context_line(
                    provider.context_window(),
                    "from this model's `[models.*]` entry in that file"
                )
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
        let forced_on = openai_compat(
            "k",
            "https://gw/v1",
            "deepseek-v4",
            AdapterKnobs {
                supports_vision: Some(true),
                ..AdapterKnobs::default()
            },
        )
        .expect("build");
        assert!(
            forced_on.supports_vision(),
            "an explicit true must beat a text-only-looking name"
        );

        let forced_off = openai_compat(
            "k",
            "https://gw/v1",
            "gpt-4o",
            AdapterKnobs {
                supports_vision: Some(false),
                ..AdapterKnobs::default()
            },
        )
        .expect("build");
        assert!(
            !forced_off.supports_vision(),
            "an explicit false must beat a vision-looking name"
        );
    }

    #[test]
    fn an_unset_row_leaves_the_heuristic_in_charge() {
        let unset =
            openai_compat("k", "https://gw/v1", "gpt-4o", AdapterKnobs::default()).expect("build");
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

    /// Both rows take the reasoning switch, and on both an absent field must
    /// stay absent — `None` is what makes the adapter omit the `thinking`
    /// object, which is the only shape every gateway is known to accept. A
    /// default that filled something in would send it to gateways that have
    /// never heard of the key.
    #[test]
    fn both_rows_take_the_thinking_switch_and_leaving_it_out_stays_out() {
        let conversation: OpenAiCompatRow = serde_json::from_value(serde_json::json!({
            "thinking_type": "disabled"
        }))
        .expect("parse with the switch");
        assert_eq!(conversation.thinking_type.as_deref(), Some("disabled"));

        let utility: UtilityRow = serde_json::from_value(serde_json::json!({
            "model": "m",
            "thinking_type": "disabled",
            "thinking_keep": "all"
        }))
        .expect("parse the utility row with the switch");
        assert_eq!(utility.thinking_type.as_deref(), Some("disabled"));
        assert_eq!(utility.thinking_keep.as_deref(), Some("all"));

        let absent: UtilityRow = serde_json::from_value(serde_json::json!({ "model": "m" }))
            .expect("parse without the switch");
        assert_eq!(absent.thinking_type, None, "absent must not become a value");
        assert_eq!(absent.thinking_keep, None);
    }

    /// The end of the chain that matters: the number a person writes on the row
    /// is the number `context_window()` reports, which is what the compaction
    /// trigger divides by. A window that stops somewhere short of the provider
    /// leaves compaction firing against the wrong budget.
    #[test]
    fn the_configured_window_reaches_the_provider() {
        let provider = openai_compat(
            "k",
            "https://gw/v1",
            "some-1m-model",
            AdapterKnobs {
                context_window: Some(1_000_000),
                ..AdapterKnobs::default()
            },
        )
        .expect("build");
        assert_eq!(
            provider.context_window(),
            1_000_000,
            "an explicit window must be what the provider reports"
        );
    }

    /// What the agent is told must be the number the provider holds. The two
    /// used to be able to disagree in the silent case, so this pins them to one
    /// another rather than checking each separately.
    #[test]
    fn the_description_carries_the_providers_own_window() {
        let provider = openai_compat(
            "k",
            "https://gw/v1",
            "m",
            AdapterKnobs {
                context_window: Some(262_144),
                ..AdapterKnobs::default()
            },
        )
        .expect("build");
        let line = context_line(provider.context_window(), "stated on the `llm` row");
        assert!(
            line.contains("262144"),
            "the reported window must be the provider's:\n{line}"
        );
    }

    /// A fallback that reads like a fact is worse than no number: a reader
    /// cannot tell the two apart, so they trust a default as a property of the
    /// model. The origin is what separates them, and it has to survive.
    #[test]
    fn a_fallback_window_is_not_reported_as_if_it_were_known() {
        let built_in = context_line(128_000, "the built-in default");
        assert!(built_in.contains("128000"), "{built_in}");
        assert!(built_in.contains("default"), "{built_in}");

        // …and the configured phrasing is distinguishable from it.
        let stated = context_line(1_000_000, "stated on the `llm` row");
        assert!(stated.contains("stated"), "{stated}");
        assert!(!stated.contains("default"), "{stated}");
    }
}
