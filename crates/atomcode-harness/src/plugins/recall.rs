//! Retrieving past turns — including from sessions that ended long ago.
//!
//! The harness already writes every committed fact to a durable log. Until this
//! row that log was write-only from the agent's point of view: it could be
//! resumed but never *searched*, so "what did we decide about X last week" had
//! no answer other than the user going and finding it.
//!
//! Two deliberate choices:
//!
//! * **It reads through the `session-persistence` seam, not through files.**
//!   `list()` + `load()` is the whole interface it needs, so a deployment that
//!   swaps JSONL for a database gets recall unchanged. A row that walked a
//!   directory would have quietly turned the seam into a lie.
//! * **The ranking is `atomcode_capabilities::search`, the same code `recall`
//!   in L1 ranks with.** Two session stores with different record shapes can
//!   still not have two different answers to the same query.
//!
//! It searches the current session too. That is not an oversight: compaction
//! removes turns from the model's working set but never from the log, so the
//! most valuable thing recall finds is often something this very session said
//! before it was compacted away.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::seams::{SessionPersistenceSvc, SessionSvc};
use crate::session::{LoggedEvent, SessionEvent};

/// One turn, flattened into the text it can be searched by.
struct Turn {
    session: String,
    turn: u64,
    /// Epoch millis parsed from the session id, for recency. Sessions are named
    /// `<millis>-<pid>`, so this needs no second timestamp on every event.
    started: u64,
    /// Already lowercased: the haystack is assembled once and scored many times.
    hay: String,
    /// What a person would recognise the turn by.
    asked: String,
    answered: String,
}

fn started_at(session_id: &str) -> u64 {
    session_id
        .split('-')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Fold one session's events into searchable turns.
fn turns_of(session: &str, events: &[LoggedEvent]) -> Vec<Turn> {
    let started = started_at(session);
    let mut out: Vec<Turn> = Vec::new();
    let find = |turn: u64, out: &mut Vec<Turn>| -> usize {
        match out.iter().position(|t| t.turn == turn) {
            Some(i) => i,
            None => {
                out.push(Turn {
                    session: session.to_string(),
                    turn,
                    started,
                    hay: String::new(),
                    asked: String::new(),
                    answered: String::new(),
                });
                out.len() - 1
            }
        }
    };
    for logged in events {
        // Streaming chunks are deliberately skipped: the assembled
        // `AssistantMessage` carries the same text, and scoring both would count
        // every answer twice.
        let (turn, text) = match &logged.event {
            SessionEvent::UserMessage { turn, text, .. } => (*turn, text.clone()),
            SessionEvent::AssistantMessage {
                turn,
                text,
                reasoning,
                tool_calls,
                ..
            } => {
                let mut all = format!("{text} {reasoning}");
                for call in tool_calls {
                    all.push(' ');
                    all.push_str(&call.name);
                    all.push(' ');
                    all.push_str(&call.arguments);
                }
                (*turn, all)
            }
            SessionEvent::ToolResultLogged { turn, content, .. } => (*turn, content.clone()),
            SessionEvent::Injected { turn, text, .. } => (*turn, text.clone()),
            _ => continue,
        };
        let i = find(turn, &mut out);
        out[i].hay.push(' ');
        out[i].hay.push_str(&text.to_lowercase());
        match &logged.event {
            SessionEvent::UserMessage { .. } if out[i].asked.is_empty() => {
                out[i].asked = text;
            }
            SessionEvent::AssistantMessage { text, .. } if out[i].answered.is_empty() => {
                if !text.trim().is_empty() {
                    out[i].answered = text.clone();
                }
            }
            _ => {}
        }
    }
    out
}

fn clip(s: &str, cells: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= cells {
        return flat;
    }
    flat.chars().take(cells).collect::<String>() + "…"
}

fn when(millis: u64) -> String {
    // No chrono here: the harness is clock-light on purpose and a date is worth
    // exactly one division. Days since epoch is enough for the model to say
    // "three weeks ago" relative to the date it already carries.
    if millis == 0 {
        return "unknown".into();
    }
    let secs = millis / 1000;
    let days = secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's days→y/m/d, the standard branch-free form.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[derive(Debug, Deserialize)]
struct Args {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

const DEFAULT_LIMIT: usize = 6;
/// How many sessions back to read. Newest first, so the cap costs the least
/// relevant end. Without it a long-lived project turns every recall into a full
/// history scan.
const MAX_SESSIONS: usize = 60;

pub struct RecallTool {
    ctx: Context,
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Search this project's past sessions — including earlier turns of the \
         current one that have since been compacted out of view. Use it when the \
         user refers to something you have no record of (\"the thing we decided \
         last week\", \"that bug from yesterday\"), instead of saying you cannot \
         remember. Returns the matching turns with their session id and date."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Words to look for. Works with Chinese without spaces."
                },
                "limit": { "type": "integer", "description": "How many turns to return (default 6)" }
            },
            "required": ["query"]
        })
    }

    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
    }

    fn read_only_hint(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str, _tool_ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return fail(format!("recall: invalid arguments: {e}")),
        };
        let terms = atomcode_capabilities::search::tokenize(&a.query);
        if terms.is_empty() {
            return fail("recall: the query has no searchable terms.");
        }
        let Some(store) = self.ctx.service::<SessionPersistenceSvc>() else {
            return fail(
                "recall: nothing is persisted in this tree, so there is no history to search.",
            );
        };

        let ids = match store.list().await {
            Ok(ids) => ids,
            Err(e) => return fail(format!("recall: cannot list sessions: {e}")),
        };
        // The live session's events are in the log, not yet necessarily on disk,
        // so it is folded in from the log itself rather than re-read.
        let live = crate::agent::scoped(&self.ctx)
            .service::<SessionSvc>()
            .or_else(|| {
                use crate::agent::OnlySession;
                self.ctx.only_session()
            });
        let live_id = live.as_ref().map(|l| l.id().to_string());

        let mut turns: Vec<Turn> = Vec::new();
        if let (Some(log), Some(id)) = (&live, &live_id) {
            turns.extend(turns_of(id, &log.events()));
        }
        for id in ids.into_iter().take(MAX_SESSIONS) {
            if Some(&id) == live_id.as_ref() {
                continue;
            }
            if let Ok(events) = store.load(&id).await {
                turns.extend(turns_of(&id, &events));
            }
        }

        let mut hits: Vec<(atomcode_capabilities::search::Score, &Turn)> = turns
            .iter()
            .filter_map(|t| {
                let s = atomcode_capabilities::search::score(&t.hay, &terms);
                s.hit().then_some((s, t))
            })
            .collect();
        hits.sort_by(|a, b| {
            atomcode_capabilities::search::best_first(&a.0, &b.0)
                .then(b.1.started.cmp(&a.1.started))
                .then(b.1.turn.cmp(&a.1.turn))
        });

        let limit = a.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 30);
        if hits.is_empty() {
            return ok(format!(
                "No past turn in this project matches “{}”. \
                 Say so plainly rather than inventing one.",
                a.query
            ));
        }
        let mut out = format!("{} matching turn(s), best first:\n", hits.len().min(limit));
        for (_, t) in hits.into_iter().take(limit) {
            out.push_str(&format!(
                "\n— {} · session {} · turn {}\n  asked:  {}\n  answered: {}\n",
                when(t.started),
                t.session,
                t.turn,
                clip(&t.asked, 160),
                clip(&t.answered, 240),
            ));
        }
        ok(out)
    }
}

fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: false,
        images: Vec::new(),
    }
}

fn fail(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: true,
        images: Vec::new(),
    }
}

pub struct RecallPlugin;

#[async_trait]
impl Plugin for RecallPlugin {
    fn name(&self) -> &'static str {
        "recall"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Both are read at call time: a tree with no store still mounts the
        // tool, and the tool says there is no history rather than pretending.
        &["session-persistence", "operations"]
    }
    fn description(&self) -> &'static str {
        "search this project's past sessions through the persistence seam"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let toolbox = ctx
            .require::<crate::seams::ToolsSvc>()
            .map_err(|e| e.to_string())?;
        toolbox.register(Arc::new(RecallTool { ctx: ctx.clone() }))?;
        let toolbox = toolbox.clone();
        let _ = ctx.effect(move || toolbox.unregister("recall"));
        crate::plugins::self_knowledge::describes(
            ctx,
            "recall",
            12,
            "RECALL. `recall {query, limit}` searches every past session of this \
             project, plus earlier turns of this one that compaction has removed \
             from view. Chinese works without spaces. When someone refers to \
             something you have no record of, search before saying you do not \
             remember.",
        );
        Ok(())
    }
}
