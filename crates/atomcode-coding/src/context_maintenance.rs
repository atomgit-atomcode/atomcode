//! Bounded, durable tool-result cleanup before the next coding request.
//!
//! Original facts remain the UI's diff source. Only the model projection changes,
//! through the same rewrite facts compaction already persists and replays.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};
use atomcode_harness::seams::{AgentsSvc, SessionSvc};
use atomcode_harness::session::{
    commit, derive_traced, Provenance, RewrittenText, SessionEvent, TracedMessage,
};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::tool::ToolCall;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde_json::Value;

// Limit the invalidated suffix in both tokens and message positions. Deep warm
// prefixes wait for the existing pressure compactor instead of churning per call.
const TAIL_TOKENS: u64 = 8_000;
const TAIL_MESSAGES: usize = 12;
const SUPERSEDED: &str = "[Superseded by a newer complete read of the same file]";

fn exact_edit_receipt(text: &str) -> Option<&str> {
    let (header, diff) = text.split_once('\n')?;
    if !header.starts_with("Edited ") || !diff.starts_with("@@ ") {
        return None;
    }
    let (_, count) = header.rsplit_once(" (")?;
    let count = count
        .strip_suffix(" replacement)")
        .or_else(|| count.strip_suffix(" replacements)"))?;
    (count.parse::<usize>().ok()? > 0).then_some(header)
}

// Only the numbered, whole-text form produced by read_file qualifies. Pagination,
// ranges, summaries, binary notices, and truncated lines lack this evidence.
fn complete_read_path(call: &ToolCall, result: &Message) -> Option<String> {
    if call.name != "read_file" || result.is_error || !result.images.is_empty() {
        return None;
    }
    let args: Value = serde_json::from_str(&call.arguments).ok()?;
    if args.get("offset").is_some_and(|v| v.as_u64() != Some(1))
        || args
            .get("ranges")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        || result.text.contains("(line truncated to ")
    {
        return None;
    }
    let mut count = 0;
    for (index, line) in result.text.lines().enumerate() {
        let (number, _) = line.split_once('\t')?;
        if number.parse::<usize>().ok()? != index + 1 {
            return None;
        }
        count += 1;
    }
    (count > 0).then(|| args.get("file_path")?.as_str().map(str::to_owned))?
}

fn rewrites(messages: &[Message]) -> Vec<(usize, String)> {
    // Opaque reasoning can bind a provider continuation to its original prefix.
    // Leave these histories to the provider-aware pressure/recovery path.
    if messages
        .iter()
        .any(|m| m.reasoning_blocks.iter().any(|b| b.opaque.is_some()))
    {
        return Vec::new();
    }
    let calls: HashMap<_, _> = messages
        .iter()
        .flat_map(|m| &m.tool_calls)
        .map(|call| (call.id.as_str(), call))
        .collect();
    let mut newer_reads = HashSet::new();
    let mut tokens = 0u64;
    let mut out = Vec::new();
    for (index, result) in messages.iter().enumerate().rev().take(TAIL_MESSAGES) {
        tokens = tokens.saturating_add(u64::from(result.estimate_tokens()));
        if tokens > TAIL_TOKENS {
            break;
        }
        if matches!(result.role, Role::User | Role::System) {
            // A runtime directory switch or a new input may change what a
            // relative path denotes. Never supersede reads across that boundary.
            newer_reads.clear();
        }
        if result.role != Role::Tool {
            continue;
        }
        let Some(call) = result.tool_call_id.as_deref().and_then(|id| calls.get(id)) else {
            newer_reads.clear();
            continue;
        };
        if !matches!(call.name.as_str(), "read_file" | "edit_file") {
            // Other tools (including bash/change_dir) can move the working
            // directory. Prefer keeping an old read to guessing its identity.
            newer_reads.clear();
        }
        if result.is_error {
            continue;
        }
        if call.name == "edit_file" {
            if let Some(header) = exact_edit_receipt(&result.text) {
                out.push((index, header.to_string()));
            }
        } else if let Some(path) = complete_read_path(call, result) {
            if !newer_reads.insert(path) && result.text.len() > SUPERSEDED.len() {
                out.push((index, SUPERSEDED.to_string()));
            }
        }
    }
    out
}

// A request-only tool result must never be the evidence for a durable rewrite.
// Require the same decision from the logged projection as well as the sent view.
fn verified_rewrites(
    requested: &[Message],
    proposed: Vec<(usize, String)>,
    logged: &[TracedMessage],
) -> (Vec<(usize, String)>, Vec<RewrittenText>) {
    let messages: Vec<_> = logged.iter().map(|item| item.message.clone()).collect();
    let allowed = rewrites(&messages);
    let mut replacements = Vec::new();
    let mut texts = Vec::new();
    for (sent, text) in proposed {
        if let Some((index, _)) = allowed.iter().find(|(index, replacement)| {
            let message = &logged[*index].message;
            replacement == &text
                && message.tool_call_id == requested[sent].tool_call_id
                && message.text == requested[sent].text
        }) {
            let Provenance::Event(seq) = logged[*index].source else {
                continue;
            };
            replacements.push((sent, text.clone()));
            texts.push(RewrittenText { seq, text });
        }
    }
    (replacements, texts)
}

pub(crate) struct ContextMaintenancePlugin;
struct Maintain(Context);

#[async_trait]
impl Waterfall<AgentRequest> for Maintain {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let proposed = rewrites(&req.messages);
        if !proposed.is_empty() {
            let Some(log) = atomcode_harness::agent::scoped(&self.0).service::<SessionSvc>() else {
                return next.run(req).await;
            };
            // Most requests have nothing to retire: only derive another view when
            // the bounded request pass actually found a candidate.
            let traced = derive_traced(&log.events());
            let (replacements, texts) = verified_rewrites(&req.messages, proposed, &traced);
            if !texts.is_empty() {
                commit(
                    &self.0,
                    &log,
                    SessionEvent::MessagesRewritten {
                        turn: req.turn,
                        texts,
                    },
                );
                // session-store cancels the owning agent synchronously on an
                // uncertain append. Never send an uncommitted replacement onward.
                if self
                    .0
                    .service::<AgentsSvc>()
                    .and_then(|agents| agents.by_session(log.id()))
                    .is_some_and(|agent| agent.cancelled())
                {
                    return Err(RequestError::message(
                        "context rewrite cancelled before model dispatch",
                    ));
                }
                let saved_bytes: usize = replacements
                    .iter()
                    .map(|(index, text)| req.messages[*index].text.len() - text.len())
                    .sum();
                tracing::debug!(target: "atomcode::context", rewrites = replacements.len(), saved_text_bytes = saved_bytes, "committed recent context cleanup");
                for (index, text) in replacements {
                    req.messages[index].text = text;
                }
            }
        }
        next.run(req).await
    }
}

#[async_trait]
impl Plugin for ContextMaintenancePlugin {
    fn name(&self) -> &'static str {
        "context-maintenance"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["agents"]
    }
    fn description(&self) -> &'static str {
        "keep original tool facts for presentation, shorten redundant recent model context"
    }
    async fn apply(&self, ctx: &Context, _: &Value) -> Result<(), String> {
        let _ = ctx.on_waterfall::<AgentRequest>(Arc::new(Maintain(ctx.clone())), true);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(id: &str, name: &str, args: Value, text: &str) -> Vec<Message> {
        vec![
            Message::assistant(
                "",
                vec![ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: args.to_string(),
                }],
            ),
            Message::tool_result(id, text, false),
        ]
    }

    #[test]
    fn only_exact_success_receipts_lose_the_duplicate_diff() {
        let exact = "Edited a.rs (1 replacement)\n@@ -1 +1 @@\n-old\n+new";
        let mut messages = pair("e", "edit_file", serde_json::json!({}), exact);
        assert_eq!(
            rewrites(&messages),
            vec![(1, "Edited a.rs (1 replacement)".into())]
        );
        messages[1].text =
            "Edited a.rs (fuzzy whitespace match, 1 replacement)\n@@ -1 +1 @@\n-old\n+new".into();
        assert!(rewrites(&messages).is_empty());
        messages[1].text = exact.into();
        messages[1].is_error = true;
        assert!(rewrites(&messages).is_empty());
        messages[1].is_error = false;
        messages[0]
            .reasoning_blocks
            .push(atomcode_kernel::message::ReasoningBlock {
                text: String::new(),
                opaque: Some("signature".into()),
                provider: Some("signed-provider".into()),
            });
        assert!(
            rewrites(&messages).is_empty(),
            "signed reasoning keeps its original prefix"
        );
    }

    #[test]
    fn a_complete_read_supersedes_only_the_same_complete_path() {
        let old = format!("1\t{}\n", "old ".repeat(30));
        let args = serde_json::json!({"file_path":"a.rs"});
        let mut messages = pair("a", "read_file", args.clone(), &old);
        messages.extend(pair("b", "read_file", args.clone(), "1\tnew\n"));
        assert_eq!(rewrites(&messages), vec![(1, SUPERSEDED.into())]);
        messages[1].text = SUPERSEDED.into();
        assert!(
            rewrites(&messages).is_empty(),
            "already folded results stay folded"
        );
        messages[1].text = old;
        messages[3].text = "1\tnew\n[Showing lines 1-1 of 20. Continue reading]".into();
        assert!(
            rewrites(&messages).is_empty(),
            "a page does not supersede a file"
        );
        messages[3].text = "1\tnew\n".into();
        messages[2].tool_calls[0].arguments =
            serde_json::json!({"file_path":"a.rs","ranges":[{"offset":1,"limit":1}]}).to_string();
        assert!(rewrites(&messages).is_empty());
        messages[2].tool_calls[0].arguments = serde_json::json!({"file_path":"b.rs"}).to_string();
        assert!(rewrites(&messages).is_empty());
        messages[2].tool_calls[0].arguments = args.to_string();
        messages[3].is_error = true;
        assert!(
            rewrites(&messages).is_empty(),
            "a failure cannot supersede success"
        );
    }

    #[test]
    fn relative_reads_never_cross_input_or_directory_change_boundaries() {
        let old = format!("1\t{}\n", "old ".repeat(30));
        let args = serde_json::json!({"file_path":"a.rs"});
        let mut messages = pair("a", "read_file", args.clone(), &old);
        messages.push(Message::user("continue in another directory"));
        messages.extend(pair("b", "read_file", args.clone(), "1\tnew\n"));
        assert!(rewrites(&messages).is_empty());
        messages.remove(2);
        let boundary = pair(
            "cd",
            "change_dir",
            serde_json::json!({"path":"other"}),
            "changed",
        );
        messages.splice(2..2, boundary);
        assert!(rewrites(&messages).is_empty());
        messages[2].tool_calls[0].name = "bash".into();
        assert!(rewrites(&messages).is_empty());
    }

    #[test]
    fn request_only_reads_cannot_authorize_persisted_supersession() {
        let args = serde_json::json!({"file_path":"a.rs"});
        let messages = pair(
            "old",
            "read_file",
            args.clone(),
            &format!("1\t{}\n", "old ".repeat(30)),
        );
        let logged: Vec<_> = messages
            .iter()
            .enumerate()
            .map(|(i, message)| TracedMessage {
                message: message.clone(),
                source: Provenance::Event(i as u64 + 1),
            })
            .collect();
        let mut requested = messages;
        requested.extend(pair("ephemeral", "read_file", args, "1\tnew\n"));
        let proposed = rewrites(&requested);
        assert_eq!(
            proposed.len(),
            1,
            "the ephemeral read would otherwise retire the logged read"
        );
        let (replacements, facts) = verified_rewrites(&requested, proposed, &logged);
        assert!(replacements.is_empty() && facts.is_empty());

        requested.insert(
            2,
            Message::tool_result("unknown", "directory changed", false),
        );
        assert!(
            rewrites(&requested).is_empty(),
            "unpaired tool results are a boundary too"
        );
    }

    #[test]
    fn deep_and_oversized_suffixes_keep_the_cached_prefix() {
        let mut messages = pair(
            "e",
            "edit_file",
            serde_json::json!({}),
            "Edited a.rs (1 replacement)\n@@ -1 +1 @@\n-old\n+new",
        );
        messages.extend((0..TAIL_MESSAGES).map(|_| Message::user("later")));
        assert!(rewrites(&messages).is_empty());
        messages.truncate(2);
        messages.push(Message::user("later ".repeat(40_000)));
        assert!(rewrites(&messages).is_empty());
    }
}
