//! A line-delimited JSON-RPC front end on stdio.
//!
//! The point is not the protocol — it is that a front end with no terminal, no
//! human, and no rendering fills the same `ui` slot as the REPL and needs
//! nothing else to change. Everything it exposes it resolves from the tree:
//! agents from the registry, transcripts from the session log, live events from
//! the bus.
//!
//! One JSON object per line in, one per line out. stdout carries the protocol
//! and nothing else, which is why the `sdk` bundle silences the renderer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::agent::AgentId;
use crate::events::SessionEventCommitted;
use crate::seams::{
    AgentLoopSvc, AgentsSvc, ControlSvc, UiSvc, UserInterface, UserQuestions, UserQuestionsSvc,
};
use crate::session::Committed;

/// Asking, over the same socket.
///
/// A program driving an agent can approve things — it just needs to be asked in
/// a way it can answer. Without this the JSON-RPC front end had to fall back to
/// refusing every risky call, which makes it useless for exactly the automation
/// it exists to serve.
struct RpcQuestions {
    outgoing: mpsc::UnboundedSender<Value>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Option<String>>>>,
    next_id: AtomicU64,
    timeout: std::time::Duration,
}

impl RpcQuestions {
    fn new(outgoing: mpsc::UnboundedSender<Value>) -> Self {
        Self {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            timeout: std::time::Duration::from_secs(300),
        }
    }

    fn answer(&self, id: u64, answer: Option<String>) -> bool {
        let waiting = self.pending.lock().expect("questions poisoned").remove(&id);
        match waiting {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }
}

#[async_trait]
impl UserQuestions for RpcQuestions {
    fn describe(&self) -> String {
        "the connected JSON-RPC client".into()
    }

    async fn ask(&self, question: &crate::seams::Question) -> Option<String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("questions poisoned")
            .insert(id, tx);
        let sent = self.outgoing.send(json!({
            "jsonrpc": "2.0",
            "method": "user/question",
            "params": {
                "id": id,
                "question": question.prompt,
                "options": question.values(),
                // The structured half, for a client that draws its own prompt.
                // A client that ignores it still has the sentence above.
                "asker": question.asker,
                "about": question.about.as_ref().map(|a| json!({
                    "tool": a.tool,
                    "arguments": a.arguments,
                    "grant": a.grant,
                })),
            },
        }));
        // The socket is gone: nobody to ask, which is a refusal, not a wait.
        if sent.is_err() {
            self.pending.lock().expect("questions poisoned").remove(&id);
            return None;
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(answer)) => answer,
            _ => {
                self.pending.lock().expect("questions poisoned").remove(&id);
                None
            }
        }
    }
}

struct JsonRpc {
    /// Built at apply time so the slot is filled while the tree mounts, and
    /// held here so `run` writes to the same channel the asker does.
    outgoing: mpsc::UnboundedSender<Value>,
    incoming: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
    questions: Arc<RpcQuestions>,
}

/// A request the client sent.
struct Call {
    id: Value,
    method: String,
    params: Value,
}

fn parse_call(line: &str) -> Result<Call, String> {
    let value: Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .ok_or("missing `method`")?
        .to_string();
    Ok(Call {
        id: value.get("id").cloned().unwrap_or(Value::Null),
        method,
        params: value.get("params").cloned().unwrap_or(json!({})),
    })
}

#[async_trait]
impl UserInterface for JsonRpc {
    fn describe(&self) -> String {
        "line-delimited JSON-RPC on stdio".into()
    }

    async fn run(&self, ctx: &Context, initial: Option<String>) -> Result<(), String> {
        let driver = ctx.require::<AgentLoopSvc>().map_err(|e| e.to_string())?;
        let agents = ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;

        // Every committed session event is forwarded as a notification. The
        // client gets the same stream a UI renders from, because there is only
        // one stream.
        let out_tx = self.outgoing.clone();
        let mut out_rx = self
            .incoming
            .lock()
            .expect("jsonrpc receiver poisoned")
            .take()
            .ok_or("this front end can only be run once")?;
        let notify_tx = out_tx.clone();
        let stream = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            let _ = notify_tx.send(json!({
                "jsonrpc": "2.0",
                "method": "session/event",
                "params": {
                    "session": committed.session,
                    "seq": committed.seq,
                    "event": committed.event,
                },
            }));
        });

        let writer = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(message) = out_rx.recv().await {
                let mut line = message.to_string();
                line.push('\n');
                if stdout.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdout.flush().await;
            }
        });

        if let Some(text) = initial {
            let agent = agents
                .create(ctx, crate::agent::CreateAgent::root(ctx))
                .await?;
            agent.send(text);
            let outcome = driver.drive(&agent).await;
            let _ = out_tx.send(json!({
                "jsonrpc": "2.0",
                "method": "turn/complete",
                "params": outcome_json(&outcome, agent.id()),
            }));
        }

        let mut lines = BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let call = match parse_call(&line) {
                Ok(call) => call,
                Err(message) => {
                    let _ = out_tx.send(error_response(Value::Null, -32700, &message));
                    continue;
                }
            };
            match self.dispatch(ctx, &call).await {
                Ok(Some(result)) => {
                    let _ = out_tx.send(json!({
                        "jsonrpc": "2.0",
                        "id": call.id,
                        "result": result,
                    }));
                }
                // `shutdown` answers and then ends the session.
                Ok(None) => {
                    let _ = out_tx.send(json!({
                        "jsonrpc": "2.0",
                        "id": call.id,
                        "result": { "ok": true },
                    }));
                    break;
                }
                Err(message) => {
                    let _ = out_tx.send(error_response(call.id, -32000, &message));
                }
            }
        }
        // Order matters on the way out: the listener holds a clone of the
        // sender, so revoking it is what lets the channel close and the writer
        // finish. Dropping only the local handle would hang forever.
        stream.dispose();
        drop(out_tx);
        let _ = writer.await;
        Ok(())
    }
}

impl JsonRpc {
    /// `Ok(None)` means "answer, then stop".
    async fn dispatch(&self, ctx: &Context, call: &Call) -> Result<Option<Value>, String> {
        let agents = ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
        match call.method.as_str() {
            "agent/create" => {
                let agent = agents
                    .create(ctx, crate::agent::CreateAgent::root(ctx))
                    .await?;
                Ok(Some(json!({ "agent": agent.id() })))
            }
            "agent/list" => Ok(Some(json!({
                "agents": agents
                    .list()
                    .iter()
                    .map(|a| json!({ "id": a.id(), "status": format!("{:?}", a.status()) }))
                    .collect::<Vec<_>>(),
            }))),
            // Queue work and drive it. Synchronous by design: a client that
            // wants to interleave sends its next `agent/send` from another
            // connection, and the inbox folds it into the running turn.
            "agent/send" => {
                let agent = self.resolve(ctx, &call.params).await?;
                let text = call
                    .params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("missing `text`")?;
                agent.send(text);
                let driver = ctx.require::<AgentLoopSvc>().map_err(|e| e.to_string())?;
                let outcome = driver.drive(&agent).await;
                Ok(Some(outcome_json(&outcome, agent.id())))
            }
            "agent/inject" => {
                let agent = self.resolve(ctx, &call.params).await?;
                let text = call
                    .params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or("missing `text`")?;
                agent.inject(text, crate::session::InjectionOrigin::Reminder);
                Ok(Some(json!({ "queued": true })))
            }
            "agent/cancel" => {
                let agent = self.resolve(ctx, &call.params).await?;
                agent.cancel();
                Ok(Some(json!({ "cancelled": true })))
            }
            "session/transcript" => {
                let log = self.resolve(ctx, &call.params).await?.session();
                Ok(Some(json!({
                    "messages": log
                        .derive_messages()
                        .iter()
                        .map(|m| json!({ "role": format!("{:?}", m.role), "text": m.text }))
                        .collect::<Vec<_>>(),
                })))
            }
            "session/events" => {
                let log = self.resolve(ctx, &call.params).await?.session();
                let after = call
                    .params
                    .get("after")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                Ok(Some(json!({
                    "events": log
                        .since(after)
                        .iter()
                        .map(|e| json!({ "seq": e.seq, "event": e.event }))
                        .collect::<Vec<_>>(),
                })))
            }
            // What is mounted. A client that can see the tree can reason about
            // what this harness can do without being told out of band.
            "harness/seams" => Ok(Some(json!({ "services": ctx.service_names() }))),
            "harness/rows" => {
                let control = ctx.require::<ControlSvc>().map_err(|e| e.to_string())?;
                Ok(Some(json!({
                    "rows": control
                        .rows()
                        .await
                        .into_iter()
                        .map(|(id, plugin, enabled)| json!({ "id": id, "plugin": plugin, "enabled": enabled }))
                        .collect::<Vec<_>>(),
                })))
            }
            // Reconfigure the running tree. The client sends a patch layer as
            // TOML and gets back what moved; rows it did not touch keep running.
            "harness/patch" => {
                let control = ctx.require::<ControlSvc>().map_err(|e| e.to_string())?;
                let toml = call
                    .params
                    .get("patch")
                    .and_then(Value::as_str)
                    .ok_or("missing `patch` (a TOML layer)")?;
                Ok(Some(json!({ "result": control.patch(toml).await? })))
            }
            "harness/audit" => {
                let control = ctx.require::<ControlSvc>().map_err(|e| e.to_string())?;
                Ok(Some(json!({ "findings": control.audit().await })))
            }
            // The other half of `user/question`.
            "user/answer" => {
                let id = call
                    .params
                    .get("id")
                    .and_then(Value::as_u64)
                    .ok_or("missing `id`")?;
                let answer = call
                    .params
                    .get("answer")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                Ok(Some(
                    json!({ "delivered": self.questions.answer(id, answer) }),
                ))
            }
            "shutdown" => Ok(None),
            other => Err(format!("unknown method `{other}`")),
        }
    }

    async fn resolve(
        &self,
        ctx: &Context,
        params: &Value,
    ) -> Result<Arc<crate::agent::Agent>, String> {
        let agents = ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
        match params.get("agent").and_then(Value::as_u64) {
            Some(id) => agents
                .get(id as AgentId)
                .ok_or_else(|| format!("no agent #{id}")),
            // No id: address the only one, or create the first. Convenient for
            // a single-agent client without making multi-agent ambiguous.
            None => match agents.list().as_slice() {
                [only] => Ok(only.clone()),
                [] => {
                    agents
                        .create(ctx, crate::agent::CreateAgent::root(ctx))
                        .await
                }
                _ => Err("several agents exist; name one with `agent`".into()),
            },
        }
    }
}

fn outcome_json(outcome: &crate::seams::TurnOutcome, agent: AgentId) -> Value {
    json!({
        "agent": agent,
        "turn": outcome.turn,
        "steps": outcome.steps,
        "tool_calls": outcome.tool_calls,
        "stop": format!("{:?}", outcome.stop),
        "text": outcome.text,
        "error": outcome.error,
    })
}

fn error_response(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

pub struct JsonRpcUiPlugin;

#[async_trait]
impl Plugin for JsonRpcUiPlugin {
    fn name(&self) -> &'static str {
        "ui-jsonrpc"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["agents", "agent-loop"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["ui", "control", "session-defaults"]
    }
    fn provides(&self) -> &'static [&'static str] {
        // A program on the other end can answer, so this row fills both.
        &["ui", "user-questions"]
    }
    fn description(&self) -> &'static str {
        "line-delimited JSON-RPC on stdio, for a program on the other end"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
        let questions = Arc::new(RpcQuestions::new(out_tx.clone()));
        let _ = ctx
            .provide::<UserQuestionsSvc>(questions.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<UiSvc>(Arc::new(JsonRpc {
                outgoing: out_tx,
                incoming: Mutex::new(Some(out_rx)),
                questions,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
