//! Explicit opt-in diagnosis with synthetic history and a real configured model.
//! No tools execute; credentials remain in memory. Reports contain only fixture
//! measurements and visible answers, never configuration, headers or reasoning.
use super::*;
use async_trait::async_trait;
use atomcode_capabilities::ProductDirs;
use atomcode_kernel::{
    hook::{LifecycleHooks, TurnCtx},
    provider::{ChatOptions, ReasoningEffort, ToolChoice},
    stream::{ProviderError, StreamEvent},
    tool::{ToolCall, ToolDef},
};
use futures::{stream::BoxStream, StreamExt};
use serde_json::json;
use std::sync::Mutex;

fn fixture(long: bool) -> Vec<Message> {
    let todos: Vec<_> = (1..=18).map(|n| json!({
        "content":format!("ROUND2_ITEM_{n:02}: optimize diagnostic renderer allocation path {n} and verify its tests"),
        "status":if n == 2 { "in_progress" } else { "pending" },
    })).collect();
    let mut messages = vec![
        Message::user("请实施第二轮诊断渲染器的18项优化，并执行验证。第一轮此前通过2050个测试不代表第二轮已完成。"),
        Message::assistant("建立第二轮计划。", vec![ToolCall { id:"plan".into(),name:"todowrite".into(),arguments:json!({"todos":todos}).to_string() }]),
        Message::tool_result("plan", "Updated todo list: 0 completed, 1 in progress, 17 pending.", false),
    ];
    if long {
        for i in 0..30 {
            let id = format!("read-{i}");
            messages.push(Message::assistant(
                "核对代码。",
                vec![ToolCall {
                    id: id.clone(),
                    name: "read_file".into(),
                    arguments: json!({"path":format!("src/fixture_{i}.rs")}).to_string(),
                }],
            ));
            messages.push(Message::tool_result(
                id,
                format!(
                    "Synthetic read-only fixture; no files modified. {}",
                    "renderer allocation path remains under review. ".repeat(20)
                ),
                false,
            ));
        }
    }
    messages.push(Message::user("继续第二轮工作。"));
    messages.push(Message::assistant("All items are complete. The full suite passed (2050 ok, 0 failed). Nothing is pending; waiting on your input.", vec![]));
    messages
}
fn rows(messages: &[Message]) -> usize {
    (1..=18)
        .filter(|n| {
            messages
                .iter()
                .any(|m| m.text.contains(&format!("ROUND2_ITEM_{n:02}")))
        })
        .count()
}
#[derive(Clone)]
struct Observed {
    provider: Arc<dyn LlmProvider>,
    calls: Arc<Mutex<Vec<usize>>>,
}
#[async_trait]
impl LlmProvider for Observed {
    fn model_name(&self) -> &str {
        self.provider.model_name()
    }
    fn context_window(&self) -> u32 {
        self.provider.context_window()
    }
    fn effort_levels(&self) -> Vec<String> {
        self.provider.effort_levels()
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        assert!(
            tools.is_empty(),
            "the diagnostic never exposes executable tools"
        );
        self.calls.lock().unwrap().push(rows(messages));
        let mut bounded = options.clone();
        bounded.max_tokens = Some(4096);
        bounded.reasoning_effort = Some(ReasoningEffort::Low);
        self.provider.chat_stream(messages, tools, &bounded).await
    }
}

#[tokio::test]
#[ignore = "explicit paid live-model diagnosis; requires TODO_EVAL_CONFIG, TODO_EVAL_MODEL and a new TODO_EVAL_REPORT"]
async fn compare_primary_todo_context_and_recap_on_conflicting_history() {
    use crate::{CodingProviderFactory, CodingRuntimeConfig, DefaultCodingProviderFactory};
    let config_path = std::env::var_os("TODO_EVAL_CONFIG").expect("set explicit config path");
    let selection = std::env::var("TODO_EVAL_MODEL").expect("set explicit model selection");
    let report = std::path::PathBuf::from(
        std::env::var_os("TODO_EVAL_REPORT").expect("set new report path"),
    );
    assert!(!report.exists(), "do not overwrite earlier measurements");
    let config = atomcode_config::config::Config::load(std::path::Path::new(&config_path))
        .unwrap_or_else(|_| panic!("config unavailable; details withheld"));
    assert!(config.resolve_model(Some(&selection)).is_ok());
    let temp = tempfile::tempdir().unwrap();
    let cfg = CodingRuntimeConfig::from_config(
        &config,
        temp.path(),
        ProductDirs::new(temp.path().join("isolated-home"), "todo-eval"),
        Some(&selection),
        None,
        false,
        false,
    )
    .agent_config();
    let provider = DefaultCodingProviderFactory::new("atomcode-todo-eval")
        .build(&cfg, None)
        .unwrap_or_else(|_| panic!("provider unavailable; details withheld"));
    let mut results = Vec::new();
    for long in [false, true] {
        for repetition in 1..=3 {
            let history = fixture(long);
            let mut request = history.clone();
            request.push(Message::user(
                "请只汇报第二轮的真实任务状态和下一步，不执行命令，不修改文件或清单。",
            ));
            crate::todo::TodoHook::default()
                .pre_request(&mut request, &TurnCtx::default())
                .await;
            let canonical = atomcode_capabilities::tools::todo::derive_current_todos(&history);
            assert_eq!(canonical.len(), 18);
            let injected_rows = rows(
                &request
                    .iter()
                    .filter(|m| m.synthetic)
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            assert_eq!(
                injected_rows, 18,
                "the normal Todo hook must supply all rows"
            );
            let calls = Arc::new(Mutex::new(Vec::new()));
            let observed = Arc::new(Observed {
                provider: provider.clone(),
                calls: calls.clone(),
            });
            let options = ChatOptions {
                tool_choice: ToolChoice::None,
                ..Default::default()
            };
            let direct = tokio::time::timeout(Duration::from_secs(75), async {
                let mut stream = observed
                    .chat_stream(&request, &[], &options)
                    .await
                    .map_err(|_| "open_error")?;
                let mut text = String::new();
                let mut truncated = false;
                while let Some(event) = stream.next().await {
                    match event {
                        StreamEvent::TextDelta(delta) => text.push_str(&delta),
                        StreamEvent::Error(_) => return Err("stream_error"),
                        StreamEvent::Done { truncated: cut } => {
                            truncated = cut;
                            break;
                        }
                        _ => {}
                    }
                }
                Ok((text, truncated))
            })
            .await;
            let recap = generate_recap(
                Some(observed),
                &history,
                crate::todo::recap_todos(&history, None),
            )
            .await;
            let audit = calls.lock().unwrap().clone();
            let primary = match direct {
                Ok(Ok((text, truncated))) => json!({"text":text,"truncated":truncated}),
                Ok(Err(kind)) => json!({"error":kind}),
                Err(_) => json!({"error":"timeout"}),
            };
            let recap_matches_task_counts = recap.as_ref().is_some_and(|text| {
                text.starts_with(&*atomcode_config::i18n::t(
                    atomcode_config::i18n::Msg::RecapTaskCounts {
                        completed: 0,
                        active: 1,
                        pending: 17,
                    },
                ))
            });
            let item = json!({"recap_matches_task_counts":recap_matches_task_counts,"recap_model_requests":audit.len().saturating_sub(1),"long_history":long,"repetition":repetition,"canonical_completed":0,"canonical_in_progress":1,"canonical_pending":17,"injected_task_rows":injected_rows,"request_task_rows":audit,"primary":primary,"recap":recap});
            println!("todo-live-eval long={long} repetition={repetition} injected={injected_rows} recap_task_rows={:?}", audit.get(1));
            results.push(item);
            std::fs::write(&report, serde_json::to_vec_pretty(&json!({"model":selection,"effort":"low","output_cap":4096,"cases":results,"limitations":"Synthetic conflicting history; real TodoHook pre_request and recap generator, real configured provider. No tools or full autonomous agent loop executed; no original user session or Linux terminal reproduced. No saved reasoning/auth/config."})).unwrap()).unwrap();
        }
    }
    assert!(
        results
            .iter()
            .all(|case| case["recap_matches_task_counts"] == true
                && case["recap_model_requests"] == 0)
    );
}
