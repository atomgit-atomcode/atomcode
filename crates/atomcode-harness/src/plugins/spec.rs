//! User-controlled original task specifications; no model tool can change pins.
use crate::{agent::Agent, seams::LlmSvc};
use async_trait::async_trait;
use atomcode_kernel::session::{retained_events, retention, SessionEvent};
use atomcode_plexus::{Context, Plugin};
use serde_json::{json, Value};
use std::sync::Arc;

/// Conservative original-text admission allowance; shared by pinning and dispatch.
pub(crate) fn allowance(window: u32) -> usize {
    (window as usize / 4).min(65_536)
}

pub struct SpecPlugin;
#[async_trait]
impl Plugin for SpecPlugin {
    fn name(&self) -> &'static str {
        "task-specifications"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        crate::commands::register(ctx, Arc::new(SpecCommand))?;
        super::self_knowledge::describes(ctx, "task-specifications", 55,
            "TASK SPECIFICATIONS — the user can select original messages with TUI /spec or WebUI message pin actions beside Copy (attachment menu manages the full list), then pin or unpin them. Fixed specifications remain verbatim through compaction. Later user corrections still take precedence. Only the user can change these selections; missing or corrupted specifications stop model requests.");
        Ok(())
    }
}
struct SpecCommand;
#[async_trait]
impl crate::commands::CatalogCommand for SpecCommand {
    fn describe(&self) -> atomcode_kernel::agent::CommandDescription {
        atomcode_kernel::agent::CommandDescription {
            name: "spec".into(),
            usage: Some("[list [before]|show <seq>|pin <seq>|unpin <seq>]".into()),
            summary: "Review and retain original task specifications".into(),
            target: atomcode_kernel::agent::CommandTarget::Session,
            ..Default::default()
        }
    }
    async fn run(&self, agent: Arc<Agent>, args: &str) -> Result<String, String> {
        execute(&agent, args).map(|value| value.to_string())
    }
}

/// Shared explicit-user operation, called by the runtime owner and catalog.
/// The selected session's current provider supplies the conservative pin budget.
pub fn execute(agent: &Agent, args: &str) -> Result<Value, String> {
    let mut words = args.split_whitespace();
    let verb = words.next().unwrap_or("list");
    let seq = words
        .next()
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| "Invalid message sequence")?;
    if words.next().is_some() {
        return Err("Usage: /spec [list [before]|show <seq>|pin <seq>|unpin <seq>]".into());
    }
    let log = agent.session();
    let events = log.events();
    let active = retention::active_sources(&events);
    let window = agent
        .ctx()
        .service::<LlmSvc>()
        .map(|p| p.context_window())
        .unwrap_or(0);
    // Bytes are a conservative allowance, not a tokenizer estimate. Reserve
    // most of the window for system instructions, tools, history and output.
    let budget = allowance(window);
    let used: usize = active
        .values()
        .fold(0usize, |sum, (bytes, _)| sum.saturating_add(*bytes));
    match verb {
        "list" => {
            let sources: Vec<_> = retained_events(&events).filter_map(|e| match &e.event {
                SessionEvent::UserMessage { text, turn, .. } if e.seq < seq.unwrap_or(u64::MAX) => Some(json!({"seq":e.seq,"turn":turn,"at":e.at,"bytes":text.len(),"preview":text.chars().take(180).collect::<String>(),"pinned":active.contains_key(&e.seq)})), _=>None,
            }).collect();
            let mut pinned = Vec::new();
            for (source, (bytes, _)) in &active {
                let existing=retained_events(&events).find_map(|e|match &e.event {
                    SessionEvent::UserMessage{text,turn,..} if e.seq==*source=>Some(json!({"seq":source,"turn":turn,"at":e.at,"bytes":bytes,"preview":text.chars().take(180).collect::<String>(),"pinned":true})),_=>None,
                });
                pinned.push(existing.unwrap_or_else(||json!({"seq":source,"turn":0,"at":0,"bytes":bytes,"preview":"Missing source — unpin to recover","pinned":true})));
            }
            let ordinary: Vec<_> = sources
                .into_iter()
                .filter(|v| !v["pinned"].as_bool().unwrap_or(false))
                .collect();
            let total = ordinary.len();
            let recent: Vec<_> = ordinary
                .into_iter()
                .rev()
                .take(32 - pinned.len().min(8))
                .collect();
            let next = if total > recent.len() {
                recent.last().and_then(|v| v["seq"].as_u64())
            } else {
                None
            };
            let shown: Vec<_> = pinned.into_iter().chain(recent).collect();
            Ok(
                json!({"session_id":log.id(),"items":shown,"next_before":next,"pinned_count":active.len(),"pinned_bytes":used,"budget_bytes":budget}),
            )
        }
        "show" | "pin" | "unpin" => {
            let seq = seq.ok_or("Select a message first")?;
            let text = retained_events(&events).find_map(|e| match &e.event {
                SessionEvent::UserMessage { text, .. } if e.seq == seq => Some(text.clone()),
                _ => None,
            });
            if verb == "show" {
                let text=match text { Some(text)=>text,None if active.contains_key(&seq)=>"[Original message is missing or withdrawn. Unpin this specification to recover.]".into(),None=>return Err("Original message is missing or withdrawn".into()) };
                if text.len() > 1_048_576 {
                    return Err("Message exceeds review limit; it cannot be pinned".into());
                }
                return Ok(
                    json!({"seq":seq,"text":text,"pinned":active.contains_key(&seq),"bytes":text.len(),"budget_bytes":budget,"pinned_bytes":used}),
                );
            }
            let enabled = verb == "pin";
            let mut failure = None;
            agent.commit_between_turns(|log| {
                let events=log.events();
                let active=retention::active_sources(&events);
                let text=retained_events(&events).find_map(|e|match &e.event{SessionEvent::UserMessage{text,..} if e.seq==seq=>Some(text.clone()),_=>None});
                if enabled {
                    if let Err(error)=retention::validate(&events) {failure=Some(error);return vec![];}
                    let Some(text)=text.as_ref() else {failure=Some("Original message is missing or withdrawn".into());return vec![];};
                    let total=active.iter().filter(|(key,_)|**key!=seq).fold(text.len(),|sum,(_,value)|sum.saturating_add(value.0));
                    if text.trim().is_empty()||budget==0||total>budget||(!active.contains_key(&seq)&&active.len()>=8) {
                        failure=Some(format!("Task specification budget exceeded ({} / {} UTF-8 bytes; at most 8 messages). Unpin another message or shorten this specification.",total,budget));return vec![];
                    }
                } else if !active.contains_key(&seq) {return vec![];}
                let text=text.unwrap_or_default();
                vec![SessionEvent::ContextRetention{turn:log.current_turn(),source:seq,active:enabled,bytes:text.len(),sha256:retention::digest(&text)}]
            }).map_err(|_|"Stop the running turn before changing task specifications".to_string())?;
            if let Some(error) = failure {
                return Err(error);
            }
            Ok(json!({"seq":seq,"pinned":enabled}))
        }
        _ => Err("Usage: /spec [list [before]|show <seq>|pin <seq>|unpin <seq>]".into()),
    }
}
