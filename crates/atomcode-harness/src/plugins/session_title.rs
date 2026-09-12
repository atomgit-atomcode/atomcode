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
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::stream::StreamEvent;
use atomcode_plexus::{Context, Plugin};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;

use crate::events::SessionEventCommitted;
use crate::seams::{AgentsSvc, LlmUtilitySvc, SessionTitle, SessionTitleSvc};
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
        title = title.chars().take(max_bytes / 4).collect();
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

/// Asks the utility model, from the first prompt. Falls back to the first
/// prompt, truncated, when there is no utility model, it is slow, it fails,
/// or it answers nothing — a session always gets a name.
///
/// Deliberately does **not** fall back to the conversation's `llm` row: a
/// side call on the same adapter would race the first turn for one gateway's
/// rate limit, and in a test it would eat the next line of the script.
struct ModelTitle {
    ctx: Context,
    max_words: usize,
    max_bytes: usize,
    max_tokens: u32,
    timeout: Duration,
}

impl ModelTitle {
    async fn ask(&self, first: &str) -> Option<String> {
        let provider = self.ctx.service::<LlmUtilitySvc>()?;
        let prompt = vec![
            Message::system(TITLE_SYSTEM),
            Message::user(first.chars().take(2000).collect::<String>()),
        ];
        let options = ChatOptions {
            max_tokens: Some(self.max_tokens),
            ..ChatOptions::default()
        };
        let call = async {
            let mut stream = provider.chat_stream(&prompt, &[], &options).await.ok()?;
            let mut out = String::new();
            while let Some(event) = stream.next().await {
                if let StreamEvent::TextDelta(text) = event {
                    out.push_str(&text);
                    if out.len() > 512 {
                        break;
                    }
                }
            }
            Some(out)
        };
        tokio::time::timeout(self.timeout, call).await.ok().flatten()
    }
}

#[async_trait]
impl SessionTitle for ModelTitle {
    fn describe(&self) -> String {
        "the utility model, from the first prompt; the first prompt itself when there is none".into()
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
    #[serde(default = "default_tokens")]
    max_tokens: u32,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
}

fn default_tokens() -> u32 {
    32
}

fn default_timeout() -> u64 {
    15
}

impl Default for ModelTitleRow {
    fn default() -> Self {
        Self {
            max_words: default_words(),
            max_bytes: default_bytes(),
            max_tokens: default_tokens(),
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
        // Resolved per call, so a patch that swaps the utility model applies
        // to the next title without remounting this row.
        &["llm-utility"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["session-title"]
    }
    fn description(&self) -> &'static str {
        "name a session by asking the utility model about its first prompt"
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
                let (Some(agents), Some(titler)) = (
                    ctx.service::<AgentsSvc>(),
                    ctx.service::<SessionTitleSvc>(),
                ) else {
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
                        SessionEvent::Titled {
                            turn: log.current_turn(),
                            title,
                        },
                    );
                });
            });
        Ok(())
    }
}
