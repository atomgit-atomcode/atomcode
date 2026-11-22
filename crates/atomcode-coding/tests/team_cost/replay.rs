//! A reproducible cost experiment. The policy reacts to real tool results and
//! budget-closing requests; both arms use exactly the same policy and task.
use super::*;
use atomcode_capabilities::session::{
    events::storage_id, SessionManager, SessionMeta, StorageOwner,
};
use atomcode_harness::seams::{DelegatedLlmSvc, LlmSvc, LlmUtilitySvc};
use atomcode_kernel::message::{Message, Role};
use atomcode_kernel::provider::{ChatOptions, LlmProvider};
use atomcode_kernel::stream::{ProviderError, StreamEvent, TokenUsage};
use atomcode_kernel::tool::{ToolCall, ToolDef};
use futures::stream::BoxStream;
use serde_json::{json, Value};

struct Script {
    role: &'static str,
    lead: bool,
}

impl Script {
    fn response(&self, history: &[Message], tools: &[ToolDef]) -> StreamEvent {
        let call = |name: &str, args: Value| {
            StreamEvent::ToolCall(ToolCall {
                id: format!("call-{}", history.len()),
                name: name.into(),
                arguments: args.to_string(),
            })
        };
        let say = |text: &str| StreamEvent::TextDelta(text.into());
        if self.lead {
            let last = history
                .iter()
                .rev()
                .find(|m| m.role != Role::System)
                .unwrap();
            if last.role == Role::User {
                let verdict = match self.role {
                    "reviewer" => "verified: the required guard is disabled",
                    "implementer" => "verified: wrote guard enabled",
                    _ => "verified: found guard disabled",
                };
                if last.text.contains(verdict) {
                    return say("accepted verified result");
                }
                if last.text.contains("unfinished:") {
                    return call(
                        "team",
                        json!({"action":"tell","name":"member","text":"finish the outstanding check"}),
                    );
                }
                if !last.text.starts_with("[message from ") {
                    return call(
                        "team",
                        json!({"action":"delegate","name":"member","role":self.role,
                        "task":last.text,"scope":["result.txt"]}),
                    );
                }
            }
            return say("awaiting result");
        }
        let successful: Vec<_> = history
            .iter()
            .filter(|m| m.role == Role::Tool && !m.is_error)
            .collect();
        let next = if successful.is_empty() {
            Some(("read_file", json!({"file_path":"source.txt"})))
        } else if successful.len() == 1 && self.role == "implementer" {
            Some((
                "write_file",
                json!({"file_path":"result.txt","content":"guard enabled"}),
            ))
        } else if successful.len() == 1 && self.role == "reviewer" {
            Some(("read_file", json!({"file_path":"policy.txt"})))
        } else {
            None
        };
        match next {
            Some(_) if tools.is_empty() => say("unfinished: another tool action is required"),
            Some((name, args)) => call(name, args),
            None if self.role == "reviewer"
                && successful.iter().any(|m| m.text.contains("guard disabled"))
                && successful.iter().any(|m| m.text.contains("guard required")) =>
            {
                say("verified: the required guard is disabled")
            }
            None if self.role == "implementer" => say("verified: wrote guard enabled"),
            None if successful.iter().any(|m| m.text.contains("guard disabled")) => {
                say("verified: found guard disabled")
            }
            None => say("missing evidence"),
        }
    }
}

#[async_trait::async_trait]
impl LlmProvider for Script {
    fn model_name(&self) -> &str {
        "team-cost-replay"
    }
    async fn chat_stream(
        &self,
        history: &[Message],
        tools: &[ToolDef],
        _: &ChatOptions,
    ) -> Result<BoxStream<'static, StreamEvent>, ProviderError> {
        // The member reports after the lead has gone idle, in both arms.
        if !self.lead {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        Ok(Box::pin(futures::stream::iter([
            self.response(history, tools),
            StreamEvent::Usage(TokenUsage {
                prompt: 100,
                completion: 20,
                cached: 0,
            }),
            StreamEvent::Done { truncated: false },
        ])))
    }
}

struct Models(&'static str);
#[async_trait::async_trait]
impl atomcode_plexus::Plugin for Models {
    fn name(&self) -> &'static str {
        "team-cost-replay"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm", "llm-utility", "llm-delegated"]
    }
    fn description(&self) -> &'static str {
        "Offline team task cost experiment"
    }
    async fn apply(&self, ctx: &atomcode_plexus::Context, _: &Value) -> Result<(), String> {
        let member = Arc::new(Script {
            role: self.0,
            lead: false,
        });
        let _ = ctx
            .provide::<LlmSvc>(Arc::new(Script {
                role: self.0,
                lead: true,
            }))
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<LlmUtilitySvc>(member.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<DelegatedLlmSvc>(member)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
#[ignore = "paired experiment; runs the Python task-cost reader against six native logs"]
async fn paired_role_budgets() {
    for (role, task) in [
        ("explorer", "Locate the guard setting"),
        (
            "implementer",
            "Write the enabled guard setting to result.txt",
        ),
        ("reviewer", "Check the guard setting against policy.txt"),
    ] {
        for limit in [0, 1] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            std::fs::write(root.join("source.txt"), "guard disabled").unwrap();
            std::fs::write(root.join("policy.txt"), "guard required").unwrap();
            let mut layers = layers_with(
                root,
                "",
                "",
                &format!("role_request_budgets = {{ {role} = {limit} }}"),
            );
            layers.push(Layer::from_toml("[[patch]]\nid='llm'\nname='team-cost-replay'\n[[patch]]\nid='llm-utility'\ndisabled=true").unwrap());
            let mut registry = atomcode_coding::on_harness::catalog();
            registry.register(Arc::new(Models(role)));
            let mut app = App::new(registry, ConfigTree::from_layers(layers).unwrap());
            app.start().await.unwrap();
            let lead = create_agent(&app).await.unwrap();
            run_turn(&app, task).await.unwrap();
            let agents = app.context().require::<AgentsSvc>().unwrap();
            let member = agents
                .by_session(&format!("{}/member", lead.session_id()))
                .unwrap();
            for _ in 0..4 {
                until_idle(&member).await;
                if !lead.inbox().has_waking_input() {
                    break;
                }
                drive(&app, &lead).await.unwrap();
            }
            assert_eq!(
                last_said(&lead).as_deref(),
                Some("accepted verified result")
            );
            if role == "implementer" {
                assert_eq!(
                    std::fs::read_to_string(root.join("result.txt")).unwrap(),
                    "guard enabled"
                );
            }
            let store = SessionManager::with_root(root.join("sessions"));
            for agent in agents.list() {
                let log = agent.session();
                let id = storage_id(log.id());
                let lease = store.acquire_lease(&id).unwrap();
                let mut meta = SessionMeta::new(&id, root.display().to_string(), 0);
                meta.owner = StorageOwner::Native;
                meta.parent = log.header().parent.clone();
                store
                    .create_event_session(&lease, log.header(), &meta)
                    .unwrap();
                store.append_events(&lease, &log.events()).unwrap();
            }
            let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/team-task-cost.py");
            let out = std::process::Command::new("python3")
                .arg(script)
                .arg(store.events_path(lead.session_id()).unwrap())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let report: Value = serde_json::from_slice(&out.stdout).unwrap();
            let totals = &report["totals"];
            assert_eq!(report["by_task"].as_object().unwrap().len(), 1, "{report}");
            assert!(report["by_task"].get("unattributed").is_none());
            assert_eq!(totals["rounds_without_call_marker"], 0);
            assert_eq!(totals["rounds_without_reported_usage"], 0);
            assert_eq!(totals["delegations"], 1);
            assert_eq!(
                totals["followups"].as_u64().unwrap_or(0),
                u64::from(limit == 1 && role != "explorer")
            );
            assert_eq!(
                totals["input_tokens_reported"].as_u64(),
                totals["provider_calls_observed"].as_u64().map(|n| n * 100)
            );
            println!("role={role} limit={limit} verified=true {totals}");
        }
    }
}
