//! Opt-in live-provider evaluation. Credentials stay in memory; only isolated,
//! read-only fixtures and sanitized measurements are sent/saved.
use async_trait::async_trait;
use atomcode_capabilities::ProductDirs;
use atomcode_coding::{CodingProviderFactory, CodingRuntimeConfig, DefaultCodingProviderFactory};
use atomcode_harness::{
    agent::OnlySession,
    session::{InjectionOrigin, SessionEvent},
};
use atomcode_kernel::{
    event::{AgentCommand, AgentEvent},
    message::Message,
    provider::{ChatOptions, LlmProvider, ReasoningEffort},
    stream::{ProviderError, StreamEvent},
    tool::ToolDef,
};
use futures::stream::BoxStream;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
mod support;

#[derive(Clone)]
struct Observed {
    provider: Arc<dyn LlmProvider>,
    calls: Arc<Mutex<Vec<Value>>>,
}
#[async_trait]
impl LlmProvider for Observed {
    fn model_name(&self) -> &str {
        self.provider.model_name()
    }
    fn context_window(&self) -> u32 {
        self.provider.context_window()
    }
    fn supports_vision(&self) -> bool {
        self.provider.supports_vision()
    }
    fn effort_levels(&self) -> Vec<String> {
        self.provider.effort_levels()
    }
    fn bind_session_id(&self, id: &str) {
        self.provider.bind_session_id(id);
    }
    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        options: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        let allowed = ["read_file", "list_directory", "use_skill", "list_skills"];
        assert!(
            tools
                .iter()
                .all(|tool| allowed.contains(&tool.name.as_str())),
            "live eval must expose only the declared read-only tools"
        );
        self.calls.lock().unwrap().push(json!({
            "auto_body_in_context": messages.iter().any(|message| message.text.starts_with("Automatically loaded static skill:")),
            "tool_names": tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(),
        }));
        self.provider.chat_stream(messages, tools, options).await
    }
}

#[tokio::test]
#[ignore = "requires explicit live-model config and report path; consumes configured model tokens"]
async fn real_model_implicit_debugging_and_negative_controls() {
    let config_path =
        std::env::var_os("ATOMCODE_SKILL_EVAL_CONFIG").expect("set explicit config path");
    let selection =
        std::env::var("ATOMCODE_SKILL_EVAL_MODEL").expect("set explicit model selection");
    let skill_source =
        std::env::var_os("ATOMCODE_SKILL_EVAL_SOURCE").expect("set explicit SKILL.md source");
    let report = std::path::PathBuf::from(
        std::env::var_os("ATOMCODE_SKILL_EVAL_REPORT").expect("set new report path"),
    );
    assert!(
        !report.exists(),
        "never overwrite an earlier live evaluation"
    );
    let config = atomcode_config::config::Config::load(std::path::Path::new(&config_path))
        .unwrap_or_else(|_| panic!("configured model file could not be loaded; details withheld"));
    assert!(
        config.resolve_model(Some(&selection)).is_ok(),
        "unknown configured model selection"
    );
    let skill_bytes = std::fs::read(&skill_source).expect("read installed skill source");
    let factory = DefaultCodingProviderFactory::new("atomcode-skill-eval");
    let positives = [
        ("failed-test", "测试失败了，帮我查原因并修好"),
        ("retry-question", "第一次重试等了200ms，是不是算错了"),
    ];
    let mut jobs = Vec::new();
    for (group, prompt) in positives {
        for repetition in 1..=5 {
            jobs.push((group, repetition, prompt, true));
        }
    }
    for (group, prompt) in [
        (
            "rename",
            "只把 first_delay 改名为 initial_delay，先给出修改建议，不修改文件",
        ),
        ("format", "仅格式化 backoff.rs，先说明格式变化，不修改文件"),
        ("translate", "请把 'test failed' 翻译为中文，不操作文件"),
        ("opt-out", "不要使用技能。第一次重试等了200ms，是不是算错了"),
    ] {
        jobs.push((group, 1, prompt, false));
    }
    let mut results = Vec::new();
    for (group, repetition, prompt, expected) in jobs {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let skill = project.join("skills/systematic-debugging");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), &skill_bytes).unwrap();
        std::fs::write(project.join("backoff.rs"), "pub fn first_delay(base_ms: u64, attempt: u32) -> u64 { base_ms * (1u64 << attempt) }\n#[test] fn first_attempt_uses_base() { assert_eq!(first_delay(100, 1), 100); }\n").unwrap();
        std::fs::write(project.join("test_report.txt"), "Fixed failure transcript for this read-only fixture: first_attempt_uses_base failed; attempt=1, base_ms=100, actual=200, expected=100. No test commands are executed by this evaluation.\n").unwrap();
        let dirs = ProductDirs::new(temp.path().join("product-home"), "eval-project");
        let mut cfg = CodingRuntimeConfig::from_config(
            &config,
            &project,
            dirs,
            Some(&selection),
            None,
            false,
            false,
        )
        .agent_config();
        cfg.system_prompt = None;
        cfg.chat_options.max_tokens = Some(4096);
        cfg.chat_options.reasoning_effort = Some(ReasoningEffort::Low);
        cfg.request_timeout = Some(std::time::Duration::from_secs(75));
        cfg.retry_max_attempts = Some(1);
        cfg.max_rounds = 6;
        let real = factory
            .build(&cfg, None)
            .unwrap_or_else(|_| panic!("provider construction failed; details withheld"));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::new(Observed {
            provider: real,
            calls: calls.clone(),
        });
        let mut opts = support::quiet_options();
        opts.skill_dirs = Some(vec![project.join("skills")]);
        opts.prompt_overrides.append = Some("This is a read-only evaluation in an isolated fixture. Do not modify files or execute commands. Inspect backoff.rs and test_report.txt with the available read-only tools when analysis is needed, then give a concise diagnosis and proposed change in at most 150 Chinese characters. For simple translation/format/rename requests, stay within that request. Do not create a plan or delegate work.".into());
        opts.host_plugins = opts.host_plugins.with_layer(atomcode_coding::plexus::Layer::from_toml("[[patch]]\nid = \"tools\"\nconfig = { include = [\"read_file\", \"list_directory\", \"use_skill\", \"list_skills\"] }").unwrap());
        let mut mounted = support::mount(&cfg, opts, observed).await;
        let started = std::time::Instant::now();
        mounted
            .handle
            .commands
            .send(AgentCommand::SendMessage {
                text: prompt.into(),
                images: vec![],
            })
            .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(180), async {
            let mut answer_chars = 0;
            let mut had_error = false;
            loop {
                match mounted.handle.events.recv().await {
                    Some(AgentEvent::TextDelta(text)) => answer_chars += text.chars().count(),
                    Some(AgentEvent::Error { .. }) => had_error = true,
                    Some(AgentEvent::Request { id, .. }) => {
                        let _ = mounted.handle.commands.send(AgentCommand::Respond {
                            id,
                            value: Value::Null,
                        });
                    }
                    Some(AgentEvent::TurnComplete { reason, .. }) => {
                        break (format!("{reason:?}"), had_error, answer_chars)
                    }
                    None => break ("closed".into(), true, answer_chars),
                    _ => {}
                }
            }
        })
        .await;
        let log = mounted.context().only_session().unwrap().events();
        let injected: Vec<_> = log.iter().filter(|event| matches!(&event.event, SessionEvent::Injected { origin: InjectionOrigin::Reminder, text, .. } if text.starts_with("Automatically loaded static skill:"))).collect();
        let first_tool = log
            .iter()
            .find(|event| matches!(&event.event, SessionEvent::ToolStarted { .. }));
        let tool_names: Vec<_> = log
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::ToolStarted { call, .. } => Some(call.name.clone()),
                _ => None,
            })
            .collect();
        let requested: Vec<_> = log
            .iter()
            .flat_map(|event| match &event.event {
                SessionEvent::AssistantMessage { tool_calls, .. } => tool_calls
                    .iter()
                    .map(|call| call.name.clone())
                    .collect::<Vec<_>>(),
                _ => vec![],
            })
            .collect();
        let audit = calls.lock().unwrap().clone();
        let in_first_context = audit
            .first()
            .and_then(|call| call["auto_body_in_context"].as_bool())
            .unwrap_or(false);
        let ordering =
            first_tool.is_none_or(|tool| injected.first().is_some_and(|body| body.seq < tool.seq));
        let terminal = outcome
            .as_ref()
            .is_ok_and(|(stop, error, count)| stop == "Stopped" && !error && *count > 0);
        let pass = terminal
            && in_first_context == expected
            && (injected.len() == usize::from(expected))
            && (!expected || ordering)
            && (expected || !requested.iter().any(|name| name == "use_skill"));
        let item = json!({"group":group,"repetition":repetition,"expected_auto_load":expected,"passed":pass,"first_request_has_body":in_first_context,"injections":injected.len(),"injection_precedes_first_tool":ordering,"executed_tool_names":tool_names,"requested_tool_names":requested,"requests":audit.len(),"terminal":outcome.as_ref().ok().map(|(stop, error, count)| json!({"stop":stop,"error":error,"answer_chars":count})),"timeout":outcome.is_err(),"seconds":started.elapsed().as_secs_f64()});
        println!("live-skill-eval {group}/{repetition}: passed={pass}, context={in_first_context}, requests={}", audit.len());
        results.push(item);
        std::fs::write(&report, serde_json::to_vec_pretty(&json!({"selection":selection,"effort":"low","max_output_tokens_per_request":4096,"cases":results,"limits":"Production mount with a real configured provider; isolated read-only fixtures; actual installed skill body; no CLI binary, no file mutation, no repair correctness claim, no saved sessions, no raw messages/reasoning/auth in report."})).unwrap()).unwrap();
        mounted.shutdown().await;
    }
    assert!(
        results.iter().all(|case| case["passed"] == true),
        "one or more live cases failed; inspect the sanitized report"
    );
}
