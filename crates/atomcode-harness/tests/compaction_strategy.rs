//! A kernel compaction strategy, deciding against the log.
//!
//! The strategy is the one the hand-written chain ran — `OverflowCompaction` over
//! `StubCompaction` — unchanged. What is under test is what the kernel used to
//! guarantee around it, now that the log applies its plan instead: where a cut
//! lands in log terms, what a rewrite is attached to, the protected head, and the
//! refusal of a plan that would not shrink what the model sees.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_capabilities::compaction::{OverflowCompaction, StubCompaction, ANCHOR_SENTINEL};
use atomcode_harness::plugins::compaction::{decide_with_strategy, strategy_would_summarize};
use atomcode_harness::seams::{CompactionAsk, CompactionDecision};
use atomcode_harness::session::{SeqNo, SessionEvent, SessionLog};
use atomcode_kernel::message::{
    CompactTrigger, CompactionPlan, CompactionStrategy, CompactionView, Message, Role,
};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;

/// Writes `SUMMARY <n>` (or whatever it is told to), and keeps every prompt.
struct Summarizer {
    asked: Mutex<Vec<Vec<Message>>>,
    answer: Option<String>,
}

impl Summarizer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            asked: Mutex::new(Vec::new()),
            answer: None,
        })
    }

    fn answering(answer: String) -> Arc<Self> {
        Arc::new(Self {
            asked: Mutex::new(Vec::new()),
            answer: Some(answer),
        })
    }

    fn prompts(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.last().map(|m| m.text.clone()).unwrap_or_default())
            .collect()
    }
}

#[async_trait]
impl LlmProvider for Summarizer {
    fn model_name(&self) -> &str {
        "summarizer"
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let mut asked = self.asked.lock().unwrap();
        asked.push(messages.to_vec());
        let text = self
            .answer
            .clone()
            .unwrap_or_else(|| format!("SUMMARY {}", asked.len()));
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta(text),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

fn strategy(summarizer: Option<Arc<Summarizer>>) -> OverflowCompaction {
    OverflowCompaction::new(
        StubCompaction::default(),
        summarizer.map(|s| s as Arc<dyn LlmProvider>),
    )
}

fn ask(trigger: CompactTrigger, window: u32, used_tokens: u32) -> CompactionAsk {
    CompactionAsk {
        trigger,
        window,
        used_tokens,
    }
}

fn pressure(window: u32, used_tokens: u32) -> CompactionAsk {
    ask(
        CompactTrigger::Auto {
            utilization: used_tokens as f32 / window as f32,
        },
        window,
        used_tokens,
    )
}

fn overflow(attempt: u8, window: u32) -> CompactionAsk {
    ask(CompactTrigger::Overflow { attempt }, window, window)
}

/// A log written the way a turn writes one.
struct Writer {
    log: SessionLog,
    turn: u64,
    round: u32,
    calls: u32,
}

impl Writer {
    fn new() -> Self {
        Self {
            log: SessionLog::new("t"),
            turn: 0,
            round: 0,
            calls: 0,
        }
    }

    fn ask(&mut self, text: &str) -> SeqNo {
        self.turn = self.log.next_turn();
        self.round = 0;
        self.log.append(SessionEvent::TurnStart { turn: self.turn });
        self.log.append(SessionEvent::UserMessage {
            turn: self.turn,
            text: text.into(),
            images: vec![],
        })
    }

    fn answer(&mut self, text: &str) -> SeqNo {
        self.round += 1;
        self.log.append(SessionEvent::AssistantMessage {
            turn: self.turn,
            round: self.round,
            text: text.into(),
            reasoning: String::new(),
            tool_calls: vec![],
            reasoning_blocks: Vec::new(),
            meta: None,
        })
    }

    /// One round that calls `tool` and gets `output` back. Returns the result's seq.
    fn call(&mut self, tool: &str, output: &str) -> SeqNo {
        self.round += 1;
        self.calls += 1;
        let id = format!("call-{}", self.calls);
        self.log.append(SessionEvent::AssistantMessage {
            turn: self.turn,
            round: self.round,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: id.clone(),
                name: tool.into(),
                arguments: "{}".into(),
            }],
            reasoning_blocks: Vec::new(),
            meta: None,
        });
        self.log.append(SessionEvent::ToolResultLogged {
            turn: self.turn,
            round: self.round,
            call_id: id,
            content: output.into(),
            is_error: false,
            images: vec![],
        })
    }

    /// Commit a decision the way `apply_compaction` does.
    fn apply(&self, decision: CompactionDecision) {
        if !decision.rewrites.is_empty() {
            self.log.append(SessionEvent::MessagesRewritten {
                turn: self.turn,
                texts: decision.rewrites,
            });
        }
        if decision.through != 0 {
            self.log.append(SessionEvent::Compacted {
                turn: self.turn,
                through: decision.through,
                summary: decision.summary,
                from: decision.from,
            });
        }
    }

    fn seen(&self) -> Vec<Message> {
        self.log.derive_messages()
    }
}

fn bulk(tag: &str, bytes: usize) -> String {
    let line = format!("{tag} line of output that nobody needs in full any more\n");
    line.repeat(bytes / line.len() + 1)
}

fn no_orphan_results(messages: &[Message]) -> Result<(), String> {
    let mut called = std::collections::HashSet::new();
    for message in messages {
        called.extend(message.tool_calls.iter().map(|c| c.id.clone()));
        if message.role == Role::Tool {
            let id = message.tool_call_id.clone().unwrap_or_default();
            if !called.contains(&id) {
                return Err(format!("a result for `{id}` whose call is gone"));
            }
        }
    }
    Ok(())
}

// ---- pressure -------------------------------------------------------------

/// Below the summary mark, pressure folds settled tool output in place. What was
/// asked and said stays word for word, a file read stays whole (a model that
/// sees a stub of a file it is editing edits it blind), the turn in progress is
/// left alone, and no model is asked anything.
#[tokio::test]
async fn under_pressure_old_tool_output_is_stubbed_in_place_and_a_read_is_kept() {
    let mut w = Writer::new();
    w.ask("look around");
    let searched = w.call("grep", &bulk("match", 2_000));
    let read = w.call("read_file", &bulk("source", 2_000));
    w.answer("found the parser");
    w.ask("now fix it");
    let running = w.call("grep", &bulk("again", 2_000));

    let summarizer = Summarizer::new();
    let strategy = strategy(Some(summarizer.clone()));
    let ask = pressure(100_000, 72_000);
    assert!(!strategy_would_summarize(&strategy, &w.log, &ask));
    let decision = decide_with_strategy(&strategy, &w.log, &ask)
        .await
        .expect("old tool output to fold");

    assert_eq!(decision.through, 0, "nothing is folded away");
    let rewritten: Vec<SeqNo> = decision.rewrites.iter().map(|r| r.seq).collect();
    assert_eq!(rewritten, vec![searched], "only the settled search");
    assert!(
        decision.rewrites[0].text.starts_with("[grep ok"),
        "{}",
        decision.rewrites[0].text
    );
    assert!(!rewritten.contains(&read) && !rewritten.contains(&running));
    assert!(summarizer.prompts().is_empty(), "no model was asked");

    w.apply(decision);
    let texts: Vec<String> = w.seen().into_iter().map(|m| m.text).collect();
    for said in ["look around", "found the parser", "now fix it"] {
        assert!(
            texts.iter().any(|t| t == said),
            "`{said}` is gone: {texts:?}"
        );
    }
    assert!(
        decide_with_strategy(&strategy, &w.log, &ask)
            .await
            .is_none(),
        "a stub is never stubbed again"
    );
}

/// Past the summary mark, older turns are folded into a summary the model
/// writes, keeping the session's first request and as many recent turns as a
/// quarter of the window holds. The next summary is an update of that one, not
/// a second summary of it.
#[tokio::test]
async fn past_the_mark_older_turns_are_summarized_under_the_first_request() {
    // A 16k window keeps 8k tokens of recent turns; each answer here is ~5k.
    let mut w = Writer::new();
    let first = w.ask("the first request");
    w.answer(&bulk("one", 20_000));
    w.ask("the second request");
    w.answer(&bulk("two", 20_000));
    let third = w.ask("the third request");
    w.answer(&bulk("three", 20_000));
    w.ask("the fourth request");

    let summarizer = Summarizer::new();
    let strategy = strategy(Some(summarizer.clone()));
    let ask = pressure(16_000, 13_000);
    assert!(strategy_would_summarize(&strategy, &w.log, &ask));
    let decision = decide_with_strategy(&strategy, &w.log, &ask)
        .await
        .expect("older turns to summarize");

    assert_eq!(decision.from, first, "the first request is kept");
    assert!(
        decision.through < third,
        "the recent turns are kept: {}",
        decision.through
    );
    assert!(decision.summary.starts_with(ANCHOR_SENTINEL));
    assert!(decision.summary.contains("SUMMARY 1"));
    w.apply(decision);
    let seen: Vec<String> = w.seen().into_iter().map(|m| m.text).collect();
    assert!(seen[0].contains("SUMMARY 1"), "{seen:?}");
    assert_eq!(seen[1], "the first request");
    assert!(!seen.iter().any(|t| t == "the second request"));
    assert!(seen.iter().any(|t| t == "the third request"));

    w.answer(&bulk("four", 20_000));
    w.ask("the fifth request");
    w.answer(&bulk("five", 20_000));
    w.ask("the sixth request");
    let decision = decide_with_strategy(&strategy, &w.log, &ask)
        .await
        .expect("more turns to summarize");
    let prompt = summarizer.prompts().pop().unwrap();
    assert!(
        prompt.contains("<previous-summary>\nSUMMARY 1"),
        "the earlier summary is the base being updated: {prompt}"
    );
    w.apply(decision);
    let seen: Vec<Message> = w.seen();
    assert_eq!(
        seen.iter()
            .filter(|m| m.text.starts_with(ANCHOR_SENTINEL))
            .count(),
        1,
        "one summary"
    );
    assert_eq!(seen[1].text, "the first request");
}

/// Without a model to write it, a summary is the model-free list — never a
/// fold with nothing in its place — and the next list still names what the
/// last one did.
#[tokio::test]
async fn without_a_model_the_summary_is_the_list() {
    let mut w = Writer::new();
    w.ask("the first request");
    w.answer(&bulk("one", 20_000));
    w.ask("the second request");
    w.answer(&bulk("two", 20_000));
    w.ask("the third request");
    w.answer(&bulk("three", 20_000));
    w.ask("the fourth request");

    let decision = decide_with_strategy(&strategy(None), &w.log, &pressure(16_000, 13_000))
        .await
        .expect("older turns to fold");
    assert!(
        decision.summary.contains("- the second request"),
        "{}",
        decision.summary
    );
    w.apply(decision);

    w.answer(&bulk("four", 20_000));
    w.ask("the fifth request");
    w.answer(&bulk("five", 20_000));
    w.ask("the sixth request");
    let decision = decide_with_strategy(&strategy(None), &w.log, &pressure(16_000, 13_000))
        .await
        .expect("more turns to fold");
    for asked in ["- the second request", "- the third request"] {
        assert!(
            decision.summary.contains(asked),
            "`{asked}` fell out: {}",
            decision.summary
        );
    }
}

/// A summary longer than what it replaces never lands: the conversation must
/// come out strictly smaller, or nothing is committed.
#[tokio::test]
async fn a_plan_that_would_not_shrink_the_conversation_is_refused() {
    let mut w = Writer::new();
    w.ask("the first request");
    w.answer(&bulk("one", 3_000));
    w.ask("the second request");
    w.answer(&bulk("two", 3_000));
    w.ask("the third request");
    w.answer(&bulk("three", 40_000));
    w.ask("the fourth request");

    let verbose = Summarizer::answering("x".repeat(60_000));
    let decision =
        decide_with_strategy(&strategy(Some(verbose)), &w.log, &pressure(16_000, 13_000)).await;
    assert!(
        decision.as_ref().is_none_or(|d| d.through == 0),
        "a summary longer than the span landed: through {:?}",
        decision.map(|d| d.through)
    );
}

// ---- overflow -------------------------------------------------------------

/// A turn too long for the window climbs the ladder: every long output as a
/// stub, then any single message too large to send cut down, then the turn
/// itself split — its older part summarized — without leaving a result whose
/// call was folded away.
#[tokio::test]
async fn an_overflow_climbs_stub_truncate_and_a_summary_that_splits_the_turn() {
    let mut w = Writer::new();
    let go = w.ask("go");
    let mut outputs = Vec::new();
    for n in 0..8 {
        outputs.push(w.call("bash", &bulk(&format!("run {n}"), 3_000)));
    }
    let monster = w.answer(&bulk("think", 40_000));
    for n in 8..12 {
        outputs.push(w.call("bash", &bulk(&format!("run {n}"), 3_000)));
    }

    let summarizer = Summarizer::new();
    let strategy = strategy(Some(summarizer.clone()));

    let stubbed = decide_with_strategy(&strategy, &w.log, &overflow(0, 16_000))
        .await
        .expect("long output to stub");
    let seqs: Vec<SeqNo> = stubbed.rewrites.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, outputs, "every long output, the running turn's too");
    w.apply(stubbed);

    let truncated = decide_with_strategy(&strategy, &w.log, &overflow(1, 16_000))
        .await
        .expect("a message too large to send");
    assert_eq!(truncated.through, 0);
    assert_eq!(
        truncated.rewrites.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![monster]
    );
    assert!(truncated.rewrites[0].text.contains("[truncated: showing "));
    w.apply(truncated);

    let split = decide_with_strategy(&strategy, &w.log, &overflow(2, 16_000))
        .await
        .expect("the turn's older part to summarize");
    assert_eq!(split.from, go, "the request is kept");
    assert!(split.through > go, "part of the turn is folded");
    w.apply(split);
    let seen = w.seen();
    assert_eq!(seen[1].text, "go");
    no_orphan_results(&seen).unwrap();
}

/// A strategy that cuts between a call and its result: the cut moves up to
/// before the call, rather than leaving a result nobody asked for.
#[tokio::test]
async fn a_cut_never_leaves_a_result_whose_call_it_removed() {
    struct CutAt(usize);
    #[async_trait]
    impl CompactionStrategy for CutAt {
        async fn plan(&self, view: &CompactionView<'_>) -> CompactionPlan {
            CompactionPlan {
                drain_from: view.sacred_floor,
                drain_to: self.0,
                summary: Some("S".into()),
                ..CompactionPlan::default()
            }
        }
    }

    let mut w = Writer::new();
    w.ask("go");
    w.answer(&bulk("plan", 4_000));
    w.call("bash", &bulk("out", 4_000));
    w.ask("more");

    // go | plan | call | result | more — cut just before the result.
    let decision = decide_with_strategy(&CutAt(3), &w.log, &pressure(16_000, 13_000))
        .await
        .expect("the plan still shrinks");
    w.apply(decision);
    let seen = w.seen();
    no_orphan_results(&seen).unwrap();
    assert!(
        seen.iter().any(|m| m.text.contains("out line")),
        "the call and its result are kept together"
    );
}
