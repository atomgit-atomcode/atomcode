//! Naming a session.
//!
//! Two questions, two kinds of row. *How* a name is made fills the
//! `session-title` seam: from the first prompt with no model call, or by
//! asking the utility model. *When* a name is made and where it lands is a
//! policy row: the first prompt reaches the log, a name is asked for in the
//! background, and the answer is committed as a `Titled` fact — so the log,
//! `session/list` and a resumed session all agree on it.
//!
//! A name the person gave wins. The policy only names a session that has no
//! name, and the newest `Titled` fact is the one that counts.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::StreamEvent;
use atomcode_plexus::{Context, Plugin};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::events::SessionEventCommitted;
use crate::seams::{AgentsSvc, LlmUtilitySvc, ModelsSvc, SessionTitle, SessionTitleSvc};
use crate::session::{Committed, SessionEvent, SessionLog};

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}

/// The first thing the person said.
fn first_prompt(log: &SessionLog) -> Option<String> {
    log.events().into_iter().find_map(|e| match e.event {
        SessionEvent::UserMessage { text, .. } => Some(text),
        _ => None,
    })
}

/// Cut a candidate down to a title: one line, no quotes, bounded.
fn tidy(text: &str, max_words: usize, max_bytes: usize) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let line = line.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '“' | '”' | '「' | '」'));
    let mut title: String = line
        .split_whitespace()
        .take(max_words)
        .collect::<Vec<_>>()
        .join(" ");
    if title.len() > max_bytes {
        let mut end = max_bytes;
        while !title.is_char_boundary(end) {
            end -= 1;
        }
        title.truncate(end);
    }
    // Punctuation the cut left dangling — a comma at word eight, the period a
    // model puts on everything — is not part of a name.
    let title = title
        .trim_end_matches(['.', '。', ',', '，', ':', '：', ';', '；', '!', '！', '、'])
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '“' | '”'))
        .to_string();
    (!title.is_empty()).then_some(title)
}

/// Titles from the first prompt, without a model call.
///
/// A seam because a deployment that wants a model-written title fills the same
/// slot; a headless run should not pay for one.
struct FirstPromptTitle {
    max_words: usize,
    max_bytes: usize,
}

#[async_trait]
impl SessionTitle for FirstPromptTitle {
    fn describe(&self) -> String {
        "first prompt, truncated".into()
    }

    async fn title(&self, log: &crate::session::SessionLog) -> Option<String> {
        tidy(&first_prompt(log)?, self.max_words, self.max_bytes)
    }
}

#[derive(Debug, Deserialize)]
struct TitleRow {
    #[serde(default = "default_words")]
    max_words: usize,
    #[serde(default = "default_bytes")]
    max_bytes: usize,
}

impl Default for TitleRow {
    fn default() -> Self {
        Self {
            max_words: default_words(),
            max_bytes: default_bytes(),
        }
    }
}

fn default_words() -> usize {
    8
}

fn default_bytes() -> usize {
    80
}

pub struct SessionTitlePlugin;

#[async_trait]
impl Plugin for SessionTitlePlugin {
    fn name(&self) -> &'static str {
        "session-title-first-prompt"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-title"]
    }
    fn description(&self) -> &'static str {
        "name a session after its first prompt, with no model call"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: TitleRow = parse(config)?;
        let _ = ctx
            .provide::<SessionTitleSvc>(Arc::new(FirstPromptTitle {
                max_words: row.max_words,
                max_bytes: row.max_bytes,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- the utility model names it -------------------------------------------

const TITLE_SYSTEM: &str = "You name conversations. Given the first message a person sent, \
reply with a short title for the conversation: at most eight words, in the person's own \
language, no quotes, no trailing period, and nothing but the title.";

/// Asks a model, from the first prompt: the utility model when one is
/// mounted, the conversation's own when not (see [`ModelTitle::provider`]). Falls back to the first
/// prompt, truncated, when the call is slow, fails, or answers nothing — a
/// session always gets a name.
///
/// # Why the conversation's model, when there is no utility model
///
/// This row is mounted because the person asked for model-made names
/// (`[ui] ai_session_naming`). The utility slot is filled only when the host
/// can SHOW a model is cheaper (`llm-utility-selected` needs a ranked
/// catalog); an unranked catalog leaves it empty, and the switch then did
/// nothing at all — every name was the first prompt, silently. The price of
/// borrowing is the one ADR 0015 named: the title races the first turn for the
/// same gateway's rate limit. One short request per session is the trade the
/// switch asks for.
///
/// # Why the request carries no output cap
///
/// On openai-compat, thinking and answer come out of ONE `max_tokens`,
/// thinking first. A cap sized for a title (it used to be 32) is a cap the
/// thinking eats: deepseek-flash spent 106–134 characters of it thinking and
/// ended `finish_reason=length` with no text, every time (2026-09-25). The
/// next-prompt suggestion hit the same wall and dropped its cap the same day.
/// What bounds this request instead: the timeout, the 512-byte read below, and
/// [`tidy`]. A row may still set `max_tokens` for a model known not to think.
struct ModelTitle {
    ctx: Context,
    max_words: usize,
    max_bytes: usize,
    max_tokens: Option<u32>,
    timeout: Duration,
}

impl ModelTitle {
    /// The utility model; else the conversation's, as the host's catalog
    /// serves it.
    ///
    /// Through the catalog rather than the `llm` seam: a host with a catalog
    /// is one whose `current` model is something it can hand out again (the
    /// coding host serves its live slot, so it is still one set of
    /// credentials). A tree with no catalog — a fixture driving a scripted
    /// `llm` — has nothing to borrow, so a title request cannot eat a line the
    /// turn was waiting for.
    async fn provider(&self) -> Option<Arc<dyn LlmProvider>> {
        if let Some(utility) = self.ctx.service::<LlmUtilitySvc>() {
            return Some(utility);
        }
        let models = self.ctx.service::<ModelsSvc>()?;
        let current = models.current()?;
        models.provider(&current).await.ok()
    }

    #[tracing::instrument(name = "llm_call", skip_all, fields(purpose = "session_title"))]
    async fn ask(&self, first: &str) -> Option<String> {
        let prompt = vec![
            Message::system(TITLE_SYSTEM),
            Message::user(first.chars().take(2000).collect::<String>()),
        ];
        let options = ChatOptions {
            max_tokens: self.max_tokens,
            ..ChatOptions::default()
        };
        let call = async {
            let provider = self.provider().await?;
            let mut stream = provider.chat_stream(&prompt, &[], &options).await.ok()?;
            let mut out = String::new();
            while let Some(event) = stream.next().await {
                match event {
                    StreamEvent::TextDelta(text) => {
                        // Never accumulate beyond the existing read allowance,
                        // including when one provider chunk is already huge.
                        if text.len() > 512 - out.len() {
                            return None;
                        }
                        out.push_str(&text);
                    }
                    StreamEvent::Done { truncated: false } => {
                        if atomcode_capabilities::provider::contains_native_chat_template_token(
                            &out,
                        ) {
                            return None;
                        }
                        return Some(out);
                    }
                    StreamEvent::Done { truncated: true }
                    | StreamEvent::Error(_)
                    | StreamEvent::Malformed
                    | StreamEvent::ToolCall(_)
                    | StreamEvent::ToolCallDelta { .. } => return None,
                    _ => {}
                }
                // The outer timeout must be polled even for a custom provider
                // that continuously yields ready reasoning/metadata events.
                tokio::task::yield_now().await;
            }
            // EOF or a partial title is not successful model completion.
            None
        };
        tokio::time::timeout(self.timeout, call)
            .await
            .ok()
            .flatten()
    }
}

#[async_trait]
impl SessionTitle for ModelTitle {
    fn describe(&self) -> String {
        "a model, from the first prompt — the utility model, or the conversation's when there \
         is none; the first prompt itself when the call fails or the title is unusable"
            .into()
    }

    async fn title(&self, log: &SessionLog) -> Option<String> {
        let first = first_prompt(log)?;
        let named = self
            .ask(&first)
            .await
            .and_then(|raw| tidy(&raw, self.max_words, self.max_bytes));
        named.or_else(|| tidy(&first, self.max_words, self.max_bytes))
    }
}

#[derive(Debug, Deserialize)]
struct ModelTitleRow {
    #[serde(default = "default_words")]
    max_words: usize,
    #[serde(default = "default_bytes")]
    max_bytes: usize,
    /// Unset by default — see [`ModelTitle`] on why a cap starves a model that
    /// thinks first.
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_timeout() -> u64 {
    15
}

impl Default for ModelTitleRow {
    fn default() -> Self {
        Self {
            max_words: default_words(),
            max_bytes: default_bytes(),
            max_tokens: None,
            timeout_secs: default_timeout(),
        }
    }
}

pub struct ModelTitlePlugin;

#[async_trait]
impl Plugin for ModelTitlePlugin {
    fn name(&self) -> &'static str {
        "session-title-model"
    }
    fn uses(&self) -> &'static [&'static str] {
        // Resolved per call, so a patch that swaps the utility model — or a
        // `/model` that swaps the conversation's — applies to the next title
        // without remounting this row.
        &["llm-utility", "models"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-title"]
    }
    fn description(&self) -> &'static str {
        "name a session by asking a model about its first prompt (the utility model, else the conversation's)"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ModelTitleRow = parse(config)?;
        let _ = ctx
            .provide::<SessionTitleSvc>(Arc::new(ModelTitle {
                ctx: ctx.clone(),
                max_words: row.max_words,
                max_bytes: row.max_bytes,
                max_tokens: row.max_tokens,
                timeout: Duration::from_secs(row.timeout_secs),
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

// ---- when: the first prompt lands ------------------------------------------

/// Name a session as soon as it has something to be named after.
///
/// Triggered by the first `UserMessage` reaching the log — which happens
/// before the turn's model request goes out — so the title is asked for while
/// the model is answering, not after. The answer is committed as a `Titled`
/// fact from a background task; a session that already has a name (the person
/// gave one, or a resume brought one back) is left alone, and that is checked
/// again just before committing, so a name typed while the model was thinking
/// still wins.
pub struct TitleOnFirstPromptPlugin;

#[async_trait]
impl Plugin for TitleOnFirstPromptPlugin {
    fn name(&self) -> &'static str {
        "session-title-on-first-prompt"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["session-title", "agents"]
    }
    fn description(&self) -> &'static str {
        "when the first prompt lands, ask for a title in the background and log it"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let ctx = ctx.clone();
        // One request in flight per session: steering in the first turn
        // commits a second user message before the first title is back.
        let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let _ = ctx
            .clone()
            .on_emit::<SessionEventCommitted>(move |committed: &Committed| {
                if !matches!(committed.event, SessionEvent::UserMessage { .. }) {
                    return;
                }
                let (Some(agents), Some(titler)) =
                    (ctx.service::<AgentsSvc>(), ctx.service::<SessionTitleSvc>())
                else {
                    return;
                };
                let Some(agent) = agents.by_session(&committed.session) else {
                    return;
                };
                let log = agent.session();
                if log.title().is_some() {
                    return;
                }
                if !in_flight
                    .lock()
                    .expect("titles poisoned")
                    .insert(committed.session.clone())
                {
                    return;
                }
                let done = in_flight.clone();
                let id = committed.session.clone();
                let agent_ctx = agent.ctx().clone();
                tokio::spawn(async move {
                    let title = titler.title(&log).await;
                    done.lock().expect("titles poisoned").remove(&id);
                    let Some(title) = title else {
                        return;
                    };
                    // The person may have named it while we were asking.
                    if log.title().is_some() {
                        return;
                    }
                    crate::session::commit(
                        &agent_ctx,
                        &log,
                        // Auto-guessed from the first prompt — not a name the person
                        // chose, so drivers keep it out of any "user named this" chrome.
                        SessionEvent::Titled {
                            turn: log.current_turn(),
                            title,
                            user_set: false,
                        },
                    );
                });
            });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::testkit::RecordingProvider;
    use atomcode_plexus::{App, ConfigTree, Layer, PluginRegistry};

    struct Utility(Arc<dyn LlmProvider>);
    #[async_trait]
    impl Plugin for Utility {
        fn name(&self) -> &'static str {
            "title-test-utility"
        }
        fn provides(&self) -> &'static [&'static str] {
            &["llm-utility"]
        }
        async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
            let _ = ctx
                .provide::<LlmUtilitySvc>(self.0.clone())
                .map_err(|e| e.to_string())?;
            Ok(())
        }
    }

    async fn title_from(
        provider: Arc<dyn LlmProvider>,
        first: &str,
        max_bytes: usize,
    ) -> Option<String> {
        let mut registry = PluginRegistry::new();
        registry
            .register(Arc::new(Utility(provider)))
            .register(Arc::new(ModelTitlePlugin));
        let layer = Layer::from_toml(&format!(
            r#"
[[insert]]
name = "title-test-utility"
[[insert]]
name = "session-title-model"
config = {{ max_bytes = {max_bytes} }}
"#
        ))
        .unwrap();
        let mut app = App::new(registry, ConfigTree::from_layers([layer]).unwrap());
        app.start().await.unwrap();
        let log = SessionLog::new("title-validation");
        log.record(SessionEvent::UserMessage {
            turn: 1,
            text: first.into(),
            images: vec![],
        });
        let title = app
            .context()
            .service::<SessionTitleSvc>()
            .unwrap()
            .title(&log)
            .await;
        app.stop();
        title
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn failed_or_unfinished_titles_fall_back_without_retrying() {
        use atomcode_kernel::stream::ProviderError;
        use atomcode_kernel::tool::ToolCall;
        let candidates = vec![
            vec![StreamEvent::TextDelta("partial".into())],
            vec![
                StreamEvent::TextDelta("partial".into()),
                StreamEvent::Error(ProviderError::default()),
            ],
            vec![
                StreamEvent::TextDelta("partial".into()),
                StreamEvent::Malformed,
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("partial".into()),
                StreamEvent::Done { truncated: true },
            ],
            vec![
                StreamEvent::ToolCall(ToolCall {
                    id: "unused".into(),
                    name: "read_file".into(),
                    arguments: "{}".into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("partial".into()),
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: Some("read_file".into()),
                    arguments: "{".into(),
                },
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::Reasoning("only thinking".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("Good title ]<]mini".into()),
                StreamEvent::TextDelta("max[>[<tool_call>".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("safe first line\n]<]minimax[>[<tool_call>".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("x".repeat(513)),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("x".repeat(511)),
                StreamEvent::TextDelta("🙂".into()),
                StreamEvent::Done { truncated: false },
            ],
        ];
        for (i, events) in candidates.into_iter().enumerate() {
            let provider = Arc::new(RecordingProvider::new(vec![events]));
            assert_eq!(
                title_from(provider.clone(), "Fix Windows build", 80)
                    .await
                    .as_deref(),
                Some("Fix Windows build"),
                "case {i}"
            );
            assert_eq!(provider.recorded().len(), 1, "case {i} must not retry");
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn completed_titles_allow_thinking_and_literal_xml_without_starving_reasoning() {
        let provider = Arc::new(RecordingProvider::new(vec![vec![
            StreamEvent::Reasoning("thinking first".repeat(100)),
            StreamEvent::ReasoningSignature {
                opaque: "signature".into(),
                provider: "signed".into(),
            },
            StreamEvent::TextDelta("\"Understanding <tool_call> tags.\"".into()),
            StreamEvent::Done { truncated: false },
        ]]));
        assert_eq!(
            title_from(provider.clone(), "question", 80)
                .await
                .as_deref(),
            Some("Understanding <tool_call> tags")
        );
        assert!(provider.recorded_options()[0].max_tokens.is_none());
        let limit = Arc::new(RecordingProvider::new(vec![vec![
            StreamEvent::TextDelta("x".repeat(512)),
            StreamEvent::Done { truncated: false },
        ]]));
        assert_eq!(title_from(limit, "question", 512).await.unwrap().len(), 512);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn only_generated_titles_are_rejected_for_native_tokens() {
        let first = "]<]minimax[>[<tool_call> documentation";
        let provider = Arc::new(RecordingProvider::new(vec![vec![
            StreamEvent::TextDelta("]<]minimax[>[bad generated title".into()),
            StreamEvent::Done { truncated: false },
        ]]));
        assert_eq!(
            title_from(provider, first, 80).await.as_deref(),
            Some(first)
        );
    }

    #[test]
    fn byte_limits_keep_the_longest_whole_utf8_prefix() {
        assert_eq!(tidy(&"a".repeat(81), 8, 80), Some("a".repeat(80)));
        assert_eq!(tidy(&"中".repeat(30), 8, 80), Some("中".repeat(26)));
        assert_eq!(tidy("ab中🙂cdef", 8, 8).as_deref(), Some("ab中"));
        assert_eq!(tidy("ab中🙂cdef", 8, 9).as_deref(), Some("ab中🙂"));
        assert_eq!(tidy("中", 8, 2), None);
        assert_eq!(tidy("anything", 8, 0), None);
        assert_eq!(tidy("one two three", 2, 80).as_deref(), Some("one two"));
    }
    struct NeverEnds {
        ready: bool,
        opened: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl LlmProvider for NeverEnds {
        fn model_name(&self) -> &str {
            "never-ending-title"
        }
        async fn chat_stream(
            &self,
            _: &[Message],
            _: &[atomcode_kernel::tool::ToolDef],
            _: &ChatOptions,
        ) -> Result<
            futures::stream::BoxStream<'static, StreamEvent>,
            atomcode_kernel::stream::ProviderError,
        > {
            self.opened.notify_one();
            if self.ready {
                Ok(Box::pin(futures::stream::repeat(StreamEvent::Usage(
                    Default::default(),
                ))))
            } else {
                Ok(Box::pin(futures::stream::pending()))
            }
        }
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn the_existing_timeout_also_bounds_continuously_ready_metadata() {
        for ready in [false, true] {
            let opened = Arc::new(tokio::sync::Notify::new());
            let provider = Arc::new(NeverEnds {
                ready,
                opened: opened.clone(),
            });
            let clock = async move {
                opened.notified().await;
                tokio::time::advance(Duration::from_secs(16)).await;
            };
            let began = tokio::time::Instant::now();
            let (title, ()) = tokio::join!(title_from(provider, "original prompt", 80), clock);
            assert_eq!(title.as_deref(), Some("original prompt"));
            assert_eq!(began.elapsed(), Duration::from_secs(16));
        }
    }
}
