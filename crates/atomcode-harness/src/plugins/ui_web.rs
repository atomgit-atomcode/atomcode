//! An HTTP front end: a page, a live event stream, and one endpoint to send.
//!
//! It exists to prove the `ui` seam is transport-shaped, not terminal-shaped.
//! Nothing below it changes: the same agent registry, the same session log, the
//! same event bus a terminal renders from — served over SSE instead of printed.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot};

use crate::agent::Agent;
use crate::events::{AgentCreated, AgentInfo, SessionEventCommitted};
use crate::seams::{
    AgentLoopSvc, AgentsSvc, ControlSvc, SessionSvc, UiSvc, UserInterface, UserQuestions,
    UserQuestionsSvc,
};
use crate::session::Committed;

#[derive(Debug, Deserialize)]
struct WebRow {
    #[serde(default = "default_addr")]
    addr: String,
}

impl Default for WebRow {
    fn default() -> Self {
        Self {
            addr: default_addr(),
        }
    }
}

fn default_addr() -> String {
    "127.0.0.1:7878".into()
}

/// Shared with every request handler.
#[derive(Clone)]
struct Web {
    ctx: Context,
    agent: Arc<Agent>,
    /// Committed session events, fanned out to connected browsers.
    events: broadcast::Sender<String>,
    questions: Arc<WebQuestions>,
}

/// Asking, through the browser that is already connected.
///
/// A front end with a person in front of it should ask rather than refuse, and
/// the web front end has one — it just had no way to reach them. Questions go
/// out on the same SSE stream the transcript uses; answers come back on a POST.
struct WebQuestions {
    events: broadcast::Sender<String>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Option<String>>>>,
    next_id: AtomicU64,
    timeout: std::time::Duration,
}

impl WebQuestions {
    fn new(events: broadcast::Sender<String>) -> Self {
        Self {
            events,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            // A tab that was closed must not hold a turn open forever. Timing
            // out answers `None`, which every caller treats as a refusal.
            timeout: std::time::Duration::from_secs(300),
        }
    }

    /// Deliver an answer to whoever is waiting for it.
    fn answer(&self, id: u64, answer: Option<String>) -> bool {
        let waiting = self.pending.lock().expect("questions poisoned").remove(&id);
        match waiting {
            Some(tx) => tx.send(answer).is_ok(),
            None => false,
        }
    }
}

#[async_trait]
impl UserQuestions for WebQuestions {
    fn describe(&self) -> String {
        "the connected browser".into()
    }

    async fn ask(&self, question: &str, options: &[String]) -> Option<String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("questions poisoned")
            .insert(id, tx);

        let sent = self.events.send(
            json!({
                "type": "question",
                "id": id,
                "question": question,
                "options": options,
            })
            .to_string(),
        );
        // Nobody is connected: there is no one to ask, so this is a refusal
        // rather than a wait. Clean up the slot we just took.
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

struct WebUi {
    addr: String,
    /// Built at apply time, not at run time: a service the tree depends on has
    /// to exist while the tree is mounting. Providing it from `run` made
    /// `approval-interactive` wait for something that only appeared after the
    /// wait had already failed.
    events: broadcast::Sender<String>,
    questions: Arc<WebQuestions>,
}

#[async_trait]
impl UserInterface for WebUi {
    fn describe(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn run(&self, ctx: &Context, initial: Option<String>) -> Result<(), String> {
        let agents = ctx.require::<AgentsSvc>().map_err(|e| e.to_string())?;
        let agent = agents.create(ctx);
        ctx.emit::<AgentCreated>(&AgentInfo { id: agent.id() });

        let events = self.events.clone();
        let fanout = events.clone();
        let _stream = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            let payload = json!({
                "type": "session",
                // Named, not filtered: a browser watching a delegating agent
                // may well want the child's stream — it just must be able to
                // tell which conversation each fact belongs to.
                "session": committed.session,
                "seq": committed.seq,
                "event": committed.event,
            })
            .to_string();
            // An error means nobody is listening, which is normal.
            let _ = fanout.send(payload);
        });

        if let Some(text) = initial {
            agent.send(text);
        }

        let state = Web {
            ctx: ctx.clone(),
            agent: agent.clone(),
            events,
            questions: self.questions.clone(),
        };
        let app = Router::new()
            .route("/", get(page))
            .route("/api/send", post(send))
            .route("/api/answer", post(answer))
            .route("/api/state", get(state_of))
            .route("/api/rows", get(rows))
            .route("/api/patch", post(patch))
            .route("/api/events", get(replay))
            .route("/api/stream", get(stream))
            .with_state(state);

        // A busy port is the most common way this front end fails to start, and
        // the OS message alone leaves the reader guessing. Name the way out.
        let listener = tokio::net::TcpListener::bind(&self.addr)
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    format!(
                        "{} is already in use.\n  \
                         - another harness may still be running: lsof -nP -iTCP:{} -sTCP:LISTEN\n  \
                         - or pick another port: --port 7879\n  \
                         - or set it in the row: [[patch]] id = \"ui\" / config = {{ addr = \"127.0.0.1:7879\" }}",
                        self.addr,
                        self.addr.rsplit(':').next().unwrap_or("7878")
                    )
                } else {
                    format!("cannot bind {}: {e}", self.addr)
                }
            })?;
        eprintln!("\x1b[2mharness listening on http://{}\x1b[0m", self.addr);
        axum::serve(listener, app)
            .await
            .map_err(|e| format!("server failed: {e}"))
    }
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

#[derive(Deserialize)]
struct SendBody {
    text: String,
}

/// Queue the message and drive the turn.
///
/// The inbox is what makes this safe to call while a turn is running: a second
/// request folds its message into the turn in flight rather than starting a
/// competing one.
async fn send(State(web): State<Web>, Json(body): Json<SendBody>) -> Json<Value> {
    web.agent.send(body.text);
    let Some(driver) = web.ctx.service::<AgentLoopSvc>() else {
        return Json(json!({ "error": "no agent-loop mounted" }));
    };
    let outcome = driver.drive(&web.agent).await;
    Json(json!({
        "turn": outcome.turn,
        "steps": outcome.steps,
        "tool_calls": outcome.tool_calls,
        "stop": format!("{:?}", outcome.stop),
        "text": outcome.text,
        "error": outcome.error,
    }))
}

#[derive(Deserialize)]
struct AnswerBody {
    id: u64,
    /// `None` means the person dismissed it, which is a refusal.
    #[serde(default)]
    answer: Option<String>,
}

async fn answer(State(web): State<Web>, Json(body): Json<AnswerBody>) -> Json<Value> {
    let delivered = web.questions.answer(body.id, body.answer);
    Json(json!({ "delivered": delivered }))
}

#[derive(Deserialize)]
struct PatchBody {
    /// A patch layer, as TOML.
    patch: String,
}

/// Reconfigure the running tree from the browser.
async fn patch(State(web): State<Web>, Json(body): Json<PatchBody>) -> Json<Value> {
    let Some(control) = web.ctx.service::<ControlSvc>() else {
        return Json(json!({ "error": "no control service" }));
    };
    match control.patch(&body.patch).await {
        Ok(summary) => Json(json!({ "result": summary })),
        Err(e) => Json(json!({ "error": e })),
    }
}

async fn rows(State(web): State<Web>) -> Json<Value> {
    let Some(control) = web.ctx.service::<ControlSvc>() else {
        return Json(json!({ "rows": [] }));
    };
    Json(json!({
        "rows": control
            .rows()
            .await
            .into_iter()
            .map(|(id, plugin, enabled)| json!({ "id": id, "plugin": plugin, "enabled": enabled }))
            .collect::<Vec<_>>(),
    }))
}

async fn state_of(State(web): State<Web>) -> Json<Value> {
    let messages = web
        .ctx
        .service::<SessionSvc>()
        .map(|log| {
            log.derive_messages()
                .iter()
                .map(|m| json!({ "role": format!("{:?}", m.role), "text": m.text }))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Json(json!({
        "agent": web.agent.id(),
        "status": format!("{:?}", web.agent.status()),
        "services": web.ctx.service_names(),
        "messages": messages,
    }))
}

#[derive(Deserialize)]
struct After {
    #[serde(default)]
    after: u64,
}

async fn replay(
    State(web): State<Web>,
    axum::extract::Query(q): axum::extract::Query<After>,
) -> Json<Value> {
    let events = web
        .ctx
        .service::<SessionSvc>()
        .map(|log| {
            log.since(q.after)
                .iter()
                .map(|e| json!({ "seq": e.seq, "event": e.event }))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Json(json!({ "events": events }))
}

async fn stream(
    State(web): State<Web>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let mut rx = web.events.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(payload) => yield Ok(Event::default().data(payload)),
                // Lagged: the browser resyncs through /api/events.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8"><title>atomcode harness</title>
<style>
 :root { color-scheme: light dark; }
 body { font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; margin: 0; padding: 1.5rem; max-width: 60rem; }
 #log { white-space: pre-wrap; }
 .user { color: #2563eb; } .tool { opacity: .65; } .err { color: #dc2626; }
 form { display: flex; gap: .5rem; margin-top: 1rem; position: sticky; bottom: 0; padding: .75rem 0; }
 input { flex: 1; font: inherit; padding: .5rem; }
</style>
<h1 style="font-size:1rem;opacity:.6">atomcode harness</h1>
<div id="log"></div>
<form onsubmit="send(event)"><input id="msg" autofocus placeholder="Ask something…"><button>send</button></form>
<script>
const log = document.getElementById('log');
function line(text, cls) {
  const el = document.createElement('div');
  if (cls) el.className = cls;
  el.textContent = text;
  log.appendChild(el);
  window.scrollTo(0, document.body.scrollHeight);
}
new EventSource('/api/stream').onmessage = (e) => {
  const { event } = JSON.parse(e.data);
  const kind = event.kind;
  if (kind === 'user_message') line('› ' + event.text, 'user');
  else if (kind === 'assistant_message' && event.text) line(event.text);
  else if (kind === 'assistant_message') for (const c of event.tool_calls || []) line('⚒ ' + c.name + ' ' + c.arguments, 'tool');
  else if (kind === 'tool_result_logged') line('  ' + (event.is_error ? '✗ ' : '✓ ') + event.content.split('\n')[0], event.is_error ? 'err' : 'tool');
  else if (kind === 'turn_end') line('— ' + event.stop, 'tool');
};
async function send(e) {
  e.preventDefault();
  const input = document.getElementById('msg');
  const text = input.value.trim();
  if (!text) return;
  input.value = '';
  await fetch('/api/send', { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ text }) });
}
</script>
"#;

pub struct WebUiPlugin;

#[async_trait]
impl Plugin for WebUiPlugin {
    fn name(&self) -> &'static str {
        "ui-web"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["agents", "agent-loop"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["ui", "sessions", "control"]
    }
    fn provides(&self) -> &'static [&'static str] {
        // The browser is a person to ask, so this row fills both — and both at
        // apply time, so anything that injects `user-questions` can mount.
        &["ui", "user-questions"]
    }
    fn description(&self) -> &'static str {
        "an HTTP server: a page, a live event stream, one send endpoint"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: WebRow = if config.is_null() {
            WebRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        // Capacity bounds a slow browser's backlog rather than the server's
        // memory; a client that falls behind misses events and resyncs through
        // `/api/events?after=`, which is why that endpoint exists.
        let (events, _) = broadcast::channel::<String>(1024);
        let questions = Arc::new(WebQuestions::new(events.clone()));
        let _ = ctx
            .provide::<UserQuestionsSvc>(questions.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<UiSvc>(Arc::new(WebUi {
                addr: row.addr,
                events,
                questions,
            }))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
