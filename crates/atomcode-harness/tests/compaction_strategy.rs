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
use atomcode_harness::plugins::compaction::{
    decide_with_strategy, strategy_would_summarize, stub_on_overflow,
};
use atomcode_harness::seams::{CompactionAsk, CompactionDecision};
use atomcode_harness::session::{SeqNo, SessionEvent, SessionLog};
use atomcode_kernel::message::{
    CompactTrigger, CompactionPlan, CompactionStrategy, CompactionView, Message, Role,
};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::session::InjectionOrigin;
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
        // Pin keep=1: these tests build 2-turn histories and assert the OLDER turn's tool
        // output stubs. The default now keeps 2 recent turns full, which would leave a
        // 2-turn history entirely full and make the gentle-stub assertions vacuous.
        StubCompaction::new(1, true),
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

    /// A turn a team member's lead starts: the work arrives as another agent's
    /// message, never as the person's.
    fn delegated(&mut self, text: &str) -> SeqNo {
        self.turn = self.log.next_turn();
        self.round = 0;
        self.log.append(SessionEvent::TurnStart { turn: self.turn });
        self.log.append(SessionEvent::Injected {
            turn: self.turn,
            text: text.into(),
            origin: InjectionOrigin::Peer {
                from: "lead".into(),
                source_turn: None,
                outside: false,
            },
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

#[test]
fn harness_overflow_only_rewrites_new_tool_output() {
    let mut writer = Writer::new();
    writer.ask("inspect output");
    writer.call("bash", &format!("{}\nexit: 1", "输出\n".repeat(1_000)));
    let overflow = ask(CompactTrigger::Overflow { attempt: 0 }, 1_000, 0);
    let first = stub_on_overflow(&writer.log, &overflow).expect("large output is compressed");
    assert_eq!(first.rewrites.len(), 1);
    assert!(first.rewrites[0].text.contains("[omitted "));
    assert!(first.rewrites[0].text.ends_with("exit: 1]"));
    writer.apply(first);
    let next_rung = ask(CompactTrigger::Overflow { attempt: 1 }, 1_000, 0);
    assert!(stub_on_overflow(&writer.log, &next_rung).is_none());

    let fresh = writer.call("bash", &"x".repeat(3_000));
    let rewrites = stub_on_overflow(&writer.log, &next_rung).unwrap().rewrites;
    assert_eq!(rewrites.len(), 1);
    assert_eq!(rewrites[0].seq, fresh);
}

#[test]
fn harness_overflow_compresses_raw_output_with_stub_like_headers() {
    let mut writer = Writer::new();
    writer.ask("inspect output");
    let raw_preview = writer.call(
        "bash",
        &format!(
            "[runner ok: 3000 lines, head: starting\n{}]",
            "body\n".repeat(1_000)
        ),
    );
    let raw_first_line = writer.call(
        "bash",
        &format!("[runner ok: 1 lines, first: {}]", "x".repeat(1_000)),
    );
    let overflow = ask(CompactTrigger::Overflow { attempt: 0 }, 1_000, 0);
    let rewrites = stub_on_overflow(&writer.log, &overflow).unwrap().rewrites;
    assert_eq!(
        rewrites
            .iter()
            .map(|rewrite| rewrite.seq)
            .collect::<Vec<_>>(),
        vec![raw_preview, raw_first_line]
    );
}

/// An already-stubbed result is not raw output worth stubbing: the builder
/// returns it unchanged (idempotent early exit) and the strict-shrink check
/// drops it, so a second overflow pass yields nothing for it and the rung
/// stands down (`None`) to let the compactor fold the settled history.
#[test]
fn harness_overflow_skips_already_stubbed_results() {
    let mut writer = Writer::new();
    writer.ask("inspect output");
    // A kernel-shaped head+tail stub (~1.9KB) — well over `WORTH_STUBBING`,
    // so the old size-only filter kept selecting it every pass. Built by the
    // real generator, so kernel budget changes cannot drift the fixture.
    let stub_text = atomcode_kernel::session::build_compact_stub(
        "bash",
        &format!("{}TAIL-MARKER exit: 0", "x".repeat(3_000)),
        true,
    );
    assert!(stub_text.len() > 500, "the stub clears the size filter");
    let _stubbed = writer.call("bash", &stub_text);
    let overflow = ask(CompactTrigger::Overflow { attempt: 1 }, 1_000, 0);
    assert!(
        stub_on_overflow(&writer.log, &overflow).is_none(),
        "a log of only stubs has nothing for this rung"
    );

    // Mixed log: only the raw result is rewritten, the stub is left alone.
    let raw = writer.call("grep", &"z".repeat(2_000));
    let rewrites = stub_on_overflow(&writer.log, &overflow).unwrap().rewrites;
    assert_eq!(
        rewrites
            .iter()
            .map(|rewrite| rewrite.seq)
            .collect::<Vec<_>>(),
        vec![raw],
        "only the not-yet-stubbed result is rewritten"
    );
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
    assert_eq!(
        seen[1],
        format!(
            "{}the first request",
            atomcode_kernel::message::COMPACTION_RETAINED_FRAMING
        )
    );
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
    assert_eq!(seen[1].display_text(), "the first request");
    assert!(seen[1]
        .text
        .starts_with(atomcode_kernel::message::COMPACTION_RETAINED_FRAMING));
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
    w.answer(&bulk("think", 40_000));
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

    assert!(
        decide_with_strategy(&strategy, &w.log, &overflow(1, 16_000))
            .await
            .is_none(),
        "dialogue must reach the summarizer intact"
    );
    assert!(w
        .seen()
        .iter()
        .any(|m| m.text.starts_with("think") && m.text.len() > 40000));

    let split = decide_with_strategy(&strategy, &w.log, &overflow(2, 16_000))
        .await
        .expect("the turn's older part to summarize");
    assert_eq!(split.from, go, "the request is kept");
    assert!(split.through > go, "part of the turn is folded");
    w.apply(split);
    let seen = w.seen();
    assert_eq!(seen[1].display_text(), "go");
    assert!(seen[1]
        .text
        .starts_with(atomcode_kernel::message::COMPACTION_RETAINED_FRAMING));
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
                diagnostic: None,
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

/// Protect agreed assistant-side decisions through the actual native log projection,
/// not merely the strategy's returned plan.
struct UnavailableSummary {
    partial: bool,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl LlmProvider for UnavailableSummary {
    fn model_name(&self) -> &str {
        "unavailable-summary"
    }
    async fn chat_stream(
        &self,
        _: &[Message],
        _: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let error = ProviderError {
            http_status: Some(403),
            message: "summary unavailable".into(),
            ..Default::default()
        };
        if !self.partial {
            return Err(error);
        }
        Ok(Box::pin(futures::stream::iter(vec![
            StreamEvent::TextDelta("incomplete checkpoint".into()),
            StreamEvent::Error(error),
        ])))
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failed_summaries_keep_agreed_design_in_the_native_session_projection() {
    const DECISION: &str = "DeviceNo and PipingNo remain separate columns. TubeSeq is the pipe-local test tube sequence, never a device sequence.";
    for partial in [false, true] {
        for mode in 0..3 {
            let mut writer = Writer::new();
            writer.ask("Design two tables; preserve confirmed decisions.");
            writer.answer(&format!("{DECISION}\n{}", bulk("archive", 20_000)));
            writer.call("grep", &bulk("old result", 2_000));
            writer.ask("Rename only the two time columns.");
            writer.answer(&bulk("second archive", 20_000));
            writer.ask("Keep other facts unchanged.");
            writer.answer(&bulk("third archive", 20_000));
            writer.ask("Return the final tables.");
            let provider = Arc::new(UnavailableSummary {
                partial,
                calls: std::sync::atomic::AtomicUsize::new(0),
            });
            let strategy =
                OverflowCompaction::new(StubCompaction::new(1, true), Some(provider.clone()));
            let request = match mode {
                0 => ask(CompactTrigger::Manual { focus: None }, 16_000, 13_000),
                1 => pressure(16_000, 13_000),
                _ => overflow(2, 16_000),
            };
            assert!(strategy_would_summarize(&strategy, &writer.log, &request));
            if let Some(decision) = decide_with_strategy(&strategy, &writer.log, &request).await {
                assert_eq!(
                    decision.through, 0,
                    "failure must not replace conversation history"
                );
                writer.apply(decision);
            }
            assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            let visible = writer.seen();
            assert!(visible.iter().any(
                |message| message.role == Role::Assistant && message.text.starts_with(DECISION)
            ));
            assert!(!writer
                .log
                .events()
                .iter()
                .any(|fact| matches!(fact.event, SessionEvent::Compacted { .. })));
            no_orphan_results(&visible).unwrap();
        }
    }
}

/// The default coding strategy keeps a first-turn memory injection across the
/// summary: memory stands for the session, not for the history it folds.
#[tokio::test]
async fn a_memory_injection_survives_the_summary() {
    let mut w = Writer::new();
    w.turn = 1;
    w.log.append(SessionEvent::TurnStart { turn: 1 });
    let memory_seq = w.log.append(SessionEvent::Injected {
        turn: 1,
        text: "=== MEMORY ===\n- prefers tabs".into(),
        origin: InjectionOrigin::Memory,
    });
    w.log.append(SessionEvent::UserMessage {
        turn: 1,
        text: "first".into(),
        images: vec![],
    });
    w.answer(&bulk("one", 20_000));

    w.turn = 2;
    w.round = 0;
    w.log.append(SessionEvent::TurnStart { turn: 2 });
    w.log.append(SessionEvent::UserMessage {
        turn: 2,
        text: "second".into(),
        images: vec![],
    });
    w.answer(&bulk("two", 20_000));

    w.turn = 3;
    w.round = 0;
    w.log.append(SessionEvent::TurnStart { turn: 3 });
    w.log.append(SessionEvent::UserMessage {
        turn: 3,
        text: "third".into(),
        images: vec![],
    });
    w.answer(&bulk("three", 20_000));

    let strategy = strategy(Some(Summarizer::new()));
    let decision = decide_with_strategy(&strategy, &w.log, &pressure(16_000, 13_000))
        .await
        .expect("a summary");
    assert_ne!(decision.through, 0, "the plan must actually fold history");
    assert!(
        decision.through >= memory_seq,
        "the fold must cover the memory turn: {} < {}",
        decision.through,
        memory_seq
    );
    w.apply(decision);
    assert!(
        w.seen().iter().any(|m| m.text.contains("prefers tabs")),
        "memory must survive the summary"
    );
}

#[tokio::test]
async fn a_late_explicit_specification_survives_model_and_model_free_compaction() {
    use atomcode_kernel::session::retention;
    for model in [Some(Summarizer::new()), None] {
        let mut w = Writer::new();
        w.ask("initial task");
        w.answer(&bulk("first", 20_000));
        let spec = (1..=20)
            .map(|n| format!("{n}. exact constraint\n"))
            .collect::<String>()
            + &(1..=5)
                .map(|n| format!("MUST NOT violate negative constraint {n}\r\n"))
                .collect::<String>()
            + "```rust\nlet exact = 42;\n```";
        let source = w.ask(&spec);
        w.answer(&bulk("second", 20_000));
        w.ask("continue");
        w.answer(&bulk("third", 20_000));
        w.log.append(SessionEvent::ContextRetention {
            turn: w.turn,
            source,
            active: true,
            bytes: spec.len(),
            sha256: retention::digest(&spec),
        });
        let decision = decide_with_strategy(&strategy(model), &w.log, &pressure(16_000, 15_000))
            .await
            .expect("ordinary history can be condensed");
        assert!(
            decision.through >= source,
            "the protected source must lie inside the folded span"
        );
        w.apply(decision);
        retention::validate_request(&w.log.events(), &w.seen()).unwrap();
        assert_eq!(
            w.seen().iter().filter(|m| m.display_text() == spec).count(),
            1
        );
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_failed_summary_keeps_its_reason_after_switching_to_a_larger_window() {
    use atomcode_harness::plugins::compaction::attempt_with_strategy;
    use atomcode_kernel::message::CompactionDiagnostic;
    let mut writer = Writer::new();
    writer.ask("keep the agreed design");
    writer.answer(&"design details ".repeat(160_000));
    writer.ask("continue");
    let before = writer.log.events();
    let provider = Arc::new(UnavailableSummary {
        partial: false,
        calls: Default::default(),
    });
    let policy = OverflowCompaction::new(StubCompaction::default(), Some(provider));
    for window in [512_000, 1_000_000] {
        let result = attempt_with_strategy(
            &policy,
            &writer.log,
            &ask(CompactTrigger::Manual { focus: None }, window, 0),
        )
        .await;
        assert!(result.decision.is_none());
        assert_eq!(
            result.diagnostic,
            Some(CompactionDiagnostic::SummaryAuthenticationFailed)
        );
        assert_eq!(
            writer.log.events(),
            before,
            "display diagnostics must not become session facts"
        );
    }
    let healthy = strategy(Some(Summarizer::new()));
    let result = attempt_with_strategy(
        &healthy,
        &writer.log,
        &ask(CompactTrigger::Manual { focus: None }, 1_000_000, 0),
    )
    .await;
    assert!(result.diagnostic.is_none());
    assert!(result.decision.is_some());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_nonshrinking_proposal_is_distinct_from_nothing_to_compact() {
    use atomcode_harness::plugins::compaction::attempt_with_strategy;
    use atomcode_kernel::message::CompactionDiagnostic;
    struct Growing;
    #[async_trait]
    impl CompactionStrategy for Growing {
        async fn plan(&self, view: &CompactionView<'_>) -> CompactionPlan {
            CompactionPlan {
                drain_from: view.sacred_floor,
                drain_to: view.messages.len(),
                summary: Some("x".repeat(100_000)),
                ..Default::default()
            }
        }
    }
    let mut writer = Writer::new();
    writer.ask("keep");
    writer.answer("brief answer");
    writer.ask("next");
    let before = writer.log.events();
    let result = attempt_with_strategy(
        &Growing,
        &writer.log,
        &ask(CompactTrigger::Manual { focus: None }, 512_000, 0),
    )
    .await;
    assert!(result.decision.is_none());
    assert_eq!(result.diagnostic, Some(CompactionDiagnostic::NotSmaller));
    assert_eq!(writer.log.events(), before);
    let result = attempt_with_strategy(
        &atomcode_kernel::message::NoCompaction,
        &writer.log,
        &ask(CompactTrigger::Manual { focus: None }, 512_000, 0),
    )
    .await;
    assert!(result.decision.is_none());
    assert!(result.diagnostic.is_none());
}

/// A team member is only ever spoken to by its lead, so its log has no request
/// from the person. Its first delegation is its request: kept, and everything
/// after it compacts like any other conversation — before, every compaction
/// of a member was a silent no-op until it overflowed for good.
#[tokio::test]
async fn a_member_spoken_to_only_by_its_lead_still_compacts() {
    let mut w = Writer::new();
    let task = w.delegated("the task the lead delegated");
    let mut outputs = Vec::new();
    for n in 0..4 {
        outputs.push(w.call("bash", &bulk(&format!("run {n}"), 3_000)));
    }
    w.answer(&bulk("one", 20_000));
    w.delegated("the lead's follow-up");
    w.answer(&bulk("two", 20_000));
    w.delegated("the lead's next follow-up");
    w.answer(&bulk("three", 20_000));
    w.delegated("the lead's latest message");

    let summarizer = Summarizer::new();
    let strategy = strategy(Some(summarizer.clone()));

    let stubbed = decide_with_strategy(&strategy, &w.log, &overflow(0, 16_000))
        .await
        .expect("an overflowing member has tool output to stub");
    let seqs: Vec<SeqNo> = stubbed.rewrites.iter().map(|r| r.seq).collect();
    assert_eq!(seqs, outputs);

    let ask = pressure(16_000, 13_000);
    assert!(strategy_would_summarize(&strategy, &w.log, &ask));
    let decision = decide_with_strategy(&strategy, &w.log, &ask)
        .await
        .expect("past the mark, a member's older turns are summarized");
    assert_eq!(decision.from, task, "the delegated task is kept");
    w.apply(decision);
    let seen: Vec<String> = w
        .seen()
        .iter()
        .map(|m| m.display_text().to_owned())
        .collect();
    assert!(seen[0].contains("SUMMARY 1"), "{seen:?}");
    assert!(
        seen[1].contains("the task the lead delegated"),
        "the task stays right under the summary: {seen:?}"
    );
    assert!(!seen.iter().any(|t| t.contains("the lead's follow-up")));
    assert!(seen.iter().any(|t| t.contains("the lead's latest message")));
    // Kept under a summary, the delegation is history like a person's first
    // request, and is framed as such: unframed, a member reads its original
    // task as a fresh instruction and starts it over.
    let sent = w.seen();
    assert!(
        sent[1]
            .text
            .starts_with(atomcode_kernel::message::COMPACTION_RETAINED_FRAMING),
        "{:?}",
        sent[1].text
    );
    let latest = sent.last().unwrap();
    assert!(
        !latest
            .text
            .contains(atomcode_kernel::message::COMPACTION_RETAINED_FRAMING),
        "the lead's latest message is a live instruction: {:?}",
        latest.text
    );
}

/// The person may speak to a member directly, long after its lead delegated.
/// That later request must not become the protected head and wall off the
/// whole history before it.
#[tokio::test]
async fn a_person_speaking_to_a_member_later_does_not_protect_its_history() {
    let mut w = Writer::new();
    let task = w.delegated("the task the lead delegated");
    w.answer(&bulk("one", 20_000));
    w.delegated("the lead's follow-up");
    w.answer(&bulk("two", 20_000));
    w.ask("the person speaking to the member");
    w.answer(&bulk("three", 20_000));
    w.delegated("the lead's latest message");

    let strategy = strategy(Some(Summarizer::new()));
    let decision = decide_with_strategy(&strategy, &w.log, &pressure(16_000, 13_000))
        .await
        .expect("the member's older turns are summarized");
    assert_eq!(decision.from, task, "the delegated task is the head");
    w.apply(decision);
    let seen: Vec<String> = w
        .seen()
        .iter()
        .map(|m| m.display_text().to_owned())
        .collect();
    assert!(
        !seen.iter().any(|t| t.contains("the lead's follow-up")),
        "{seen:?}"
    );
}
