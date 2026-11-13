use super::*;
use std::collections::VecDeque;
use std::sync::Mutex;

type Reply = Result<Vec<StreamEvent>, ProviderError>;

#[derive(Default)]
struct Probe {
    window: u32,
    max_prompt_bytes: Option<usize>,
    replies: Mutex<VecDeque<Reply>>,
    sent: Mutex<Vec<Vec<Message>>>,
}

fn answer(text: &str) -> Reply {
    Ok(vec![
        StreamEvent::TextDelta(text.into()),
        StreamEvent::Done { truncated: false },
    ])
}
fn failure(code: &str, retryable: bool) -> ProviderError {
    ProviderError {
        code: Some(code.into()),
        retryable,
        ..Default::default()
    }
}

#[async_trait]
impl LlmProvider for Probe {
    fn model_name(&self) -> &str {
        "summary-probe"
    }
    fn context_window(&self) -> u32 {
        self.window
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        _: &[atomcode_kernel::tool::ToolDef],
        options: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        assert!(
            options.request_recovery.is_some(),
            "caller must own retries"
        );
        assert_eq!(options.rate_limit_retry_owner, RateLimitRetryOwner::Kernel);
        assert_eq!(
            options.reasoning_effort, None,
            "do not silently lower effort"
        );
        let count = {
            let mut sent = self.sent.lock().unwrap();
            sent.push(messages.to_vec());
            sent.len()
        };
        if self
            .max_prompt_bytes
            .is_some_and(|limit| messages.iter().map(|m| m.text.len()).sum::<usize>() > limit)
        {
            return Err(failure("context_length_exceeded", false));
        }
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| answer(&format!("summary-{count}")));
        Ok(Box::pin(futures::stream::iter(reply?)))
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn every_segment_including_middle_dialogue_reaches_the_model_and_carries_the_summary() {
    let provider = Probe {
        window: 32_000,
        ..Default::default()
    };
    let span = vec![Message::user(format!(
        "HEAD{}MIDDLE{}TAIL",
        "字".repeat(80_000),
        "z".repeat(160_000)
    ))];
    let result = summarize_span(&provider, &span, None).await.unwrap();
    let sent = provider.sent.lock().unwrap();
    assert!(sent.len() > 2);
    let mut transcript = String::new();
    for (i, prompt) in sent.iter().enumerate() {
        assert!(prompt.iter().map(|m| m.estimate_tokens()).sum::<u32>() <= 22_400);
        let user = &prompt[1].text;
        transcript.push_str(
            user.split_once("Conversation history to summarize:\n\n")
                .unwrap()
                .1,
        );
        if i > 0 {
            assert!(user.contains(&format!("<previous-summary>\nsummary-{i}\n")));
        }
    }
    assert_eq!(
        transcript,
        render_transcript(&span),
        "no byte of dialogue may be discarded"
    );
    assert!(result.ends_with(&format!("summary-{}", sent.len())));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn context_rejection_halves_the_actual_input_and_still_covers_the_whole_transcript() {
    let provider = Probe {
        window: 512_000,
        max_prompt_bytes: Some(30_000),
        ..Default::default()
    };
    let span = vec![Message::user("detail 字\n".repeat(30_000))];
    summarize_span(&provider, &span, None).await.unwrap();
    let sent = provider.sent.lock().unwrap();
    assert!(sent[1][1].text.len() < sent[0][1].text.len());
    let mut accepted = String::new();
    for prompt in sent
        .iter()
        .filter(|p| p.iter().map(|m| m.text.len()).sum::<usize>() <= 30_000)
    {
        accepted.push_str(
            prompt[1]
                .text
                .split_once("Conversation history to summarize:\n\n")
                .unwrap()
                .1,
        );
    }
    assert_eq!(accepted, render_transcript(&span));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn transient_partial_output_is_discarded_before_one_bounded_retry() {
    let provider = Probe {
        replies: Mutex::new(VecDeque::from([
            Ok(vec![
                StreamEvent::TextDelta("BAD PARTIAL".into()),
                StreamEvent::Error(failure("stream_transport_interrupted", false)),
            ]),
            answer("COMPLETE"),
        ])),
        ..Default::default()
    };
    let result = summarize_span(&provider, &[Message::user("design")], None)
        .await
        .unwrap();
    assert!(result.ends_with("COMPLETE"));
    assert!(!result.contains("BAD PARTIAL"));
    let sent = provider.sent.lock().unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0][1].text, sent[1][1].text);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn repeated_transient_failure_stops_after_two_calls_and_preserves_the_reason() {
    let provider = Probe {
        replies: Mutex::new(VecDeque::from([
            Err(failure(STREAM_IDLE_TIMEOUT_CODE, true)),
            Err(failure(STREAM_IDLE_TIMEOUT_CODE, true)),
        ])),
        ..Default::default()
    };
    let error = summarize_span(&provider, &[Message::user("design")], None)
        .await
        .unwrap_err();
    assert_eq!(
        error.diagnostic(),
        atomcode_kernel::message::CompactionDiagnostic::SummaryTimedOut
    );
    assert_eq!(provider.sent.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn terminal_failures_are_not_retried() {
    for reply in [
        Err(failure("unauthorized", false)),
        Ok(vec![StreamEvent::Done { truncated: true }]),
        answer(""),
    ] {
        let provider = Probe {
            replies: Mutex::new(VecDeque::from([reply])),
            ..Default::default()
        };
        assert!(summarize_span(&provider, &[Message::user("design")], None)
            .await
            .is_err());
        assert_eq!(provider.sent.lock().unwrap().len(), 1);
    }
    let provider = Probe {
        replies: Mutex::new(VecDeque::from([Err(ProviderError {
            retryable: true,
            retry_after_secs: Some(60),
            ..Default::default()
        })])),
        ..Default::default()
    };
    assert!(summarize_span(&provider, &[Message::user("design")], None)
        .await
        .is_err());
    assert_eq!(
        provider.sent.lock().unwrap().len(),
        1,
        "never shorten Retry-After to fit the allowance"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_later_window_failure_keeps_the_entire_original_history_in_every_mode() {
    for trigger in [
        CompactTrigger::Manual { focus: None },
        CompactTrigger::Auto { utilization: 1.0 },
        CompactTrigger::Overflow { attempt: 2 },
    ] {
        let provider = Arc::new(Probe {
            window: 32_000,
            replies: Mutex::new(VecDeque::from([
                answer("FIRST WINDOW ONLY"),
                Err(failure("unauthorized", false)),
            ])),
            ..Default::default()
        });
        let strategy = OverflowCompaction::new(StubCompaction::default(), Some(provider.clone()))
            .with_auto_token_limit(Some(1));
        let mut conversation = atomcode_kernel::message::Conversation::new();
        conversation.messages = vec![
            Message::system("persona"),
            Message::user("keep the design"),
            Message::assistant("detail".repeat(100_000), vec![]),
            Message::user("continue"),
        ];
        let before = conversation.messages.clone();
        let floor = conversation.sacred_floor();
        let plan = strategy
            .plan(&CompactionView {
                messages: &conversation.messages,
                trigger,
                ctx_window: 32_000,
                used_tokens: 0,
                utilization: 1.0,
                sacred_floor: floor,
            })
            .await;
        assert_eq!(provider.sent.lock().unwrap().len(), 2);
        assert!(plan.summary.is_none());
        assert_eq!(plan.drain_to, 0);
        conversation.apply_plan(plan, floor);
        assert_eq!(conversation.messages, before);
    }
}

struct LongThinking;
#[async_trait]
impl LlmProvider for LongThinking {
    fn model_name(&self) -> &str {
        "long-thinking-summary"
    }
    async fn chat_stream(
        &self,
        _: &[Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &ChatOptions,
    ) -> Result<futures::stream::BoxStream<'static, StreamEvent>, ProviderError> {
        Ok(Box::pin(futures::stream::unfold(0, |step| async move {
            match step {
                0..=4 => {
                    tokio::time::sleep(Duration::from_secs(240)).await;
                    Some((StreamEvent::Reasoning("progress".into()), step + 1))
                }
                5 => Some((StreamEvent::TextDelta("complete design".into()), 6)),
                6 => Some((StreamEvent::Done { truncated: false }, 7)),
                _ => None,
            }
        })))
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn twenty_minutes_of_progress_can_complete_in_all_compaction_modes() {
    for trigger in [
        CompactTrigger::Manual { focus: None },
        CompactTrigger::Auto { utilization: 1.0 },
        CompactTrigger::Overflow { attempt: 2 },
    ] {
        let strategy =
            OverflowCompaction::new(StubCompaction::default(), Some(Arc::new(LongThinking)))
                .with_auto_token_limit(Some(1));
        let messages = vec![
            Message::system("persona"),
            Message::user("design"),
            Message::assistant("details".repeat(1_000), vec![]),
            Message::user("continue"),
        ];
        let start = tokio::time::Instant::now();
        let plan = strategy
            .plan(&CompactionView {
                messages: &messages,
                trigger,
                ctx_window: 8_000,
                used_tokens: 0,
                utilization: 1.0,
                sacred_floor: 2,
            })
            .await;
        assert!(plan.summary.unwrap().ends_with("complete design"));
        assert!(plan.diagnostic.is_none());
        assert!(plan.resume_note.is_none(), "no long-thinking reminders");
        assert_eq!(start.elapsed(), Duration::from_secs(1_200));
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn dropping_an_active_summary_cancels_it_without_a_partial_result() {
    let result = tokio::time::timeout(
        Duration::from_secs(500),
        summarize_span(&LongThinking, &[Message::user("design")], None),
    )
    .await;
    assert!(
        result.is_err(),
        "caller cancellation drops the active summary future"
    );
}
