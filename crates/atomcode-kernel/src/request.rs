use crate::event::{AgentEvent, RequestId};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

/// Kernel-internal round-trip broker. Lets a middleware emit events and perform
/// an id-correlated request to the driver. The `oneshot` that resolves a request
/// lives ONLY here — never inside AgentEvent — which is exactly what keeps events
/// serializable / wire-compatible.
#[derive(Clone)]
pub struct RequestCtx {
    pub(crate) events: UnboundedSender<AgentEvent>,
    pub(crate) pending: Arc<Mutex<HashMap<RequestId, oneshot::Sender<Value>>>>,
    pub(crate) next_id: Arc<AtomicU64>,
}

impl RequestCtx {
    pub fn new(events: UnboundedSender<AgentEvent>) -> Self {
        Self {
            events,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Emit an event to the driver.
    pub fn emit(&self, ev: AgentEvent) {
        let _ = self.events.send(ev);
    }

    /// Emit a Request and await the driver's Respond{id,value} (by id).
    pub async fn request(&self, kind: &str, payload: Value) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let _ = self.events.send(AgentEvent::Request { id, kind: kind.to_string(), payload });
        rx.await.unwrap_or(Value::Null)
    }

    /// Route a driver response to the waiting requester.
    pub(crate) fn resolve(&self, id: RequestId, value: Value) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(value);
        }
    }
}
