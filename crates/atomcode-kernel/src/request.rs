use crate::event::{AgentEvent, RequestId};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
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
    /// LIVENESS: optional bound on how long `request` will await the driver's
    /// `Respond`. `None` (default) = unbounded (only a DROPPED sender unblocks);
    /// `Some(d)` = a crashed/silent driver that never answers within `d` degrades
    /// the round-trip to `Value::Null` so the awaiting middleware proceeds instead
    /// of parking forever. Injected by the builder — kernel mechanism, policy value.
    pub(crate) request_timeout: Option<Duration>,
}

impl RequestCtx {
    pub fn new(events: UnboundedSender<AgentEvent>, request_timeout: Option<Duration>) -> Self {
        Self {
            events,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            request_timeout,
        }
    }

    /// Emit an event to the driver.
    pub fn emit(&self, ev: AgentEvent) {
        let _ = self.events.send(ev);
    }

    /// Emit a Request and await the driver's Respond{id,value} (by id).
    ///
    /// LIVENESS: when a `request_timeout` is configured, the await is bounded — a
    /// crashed/silent driver that never sends `Respond` within the timeout degrades
    /// the round-trip to `Value::Null` (the SAME degraded value as a dropped
    /// sender), so an awaiting middleware proceeds (e.g. ApprovalMiddleware sees
    /// Null → deny/block) instead of parking forever. On timeout the pending entry
    /// is removed so the oneshot is not leaked in `pending` (a late `Respond` then
    /// no-ops in `resolve`). When `None` (default), behavior is unchanged: await
    /// indefinitely; only a DROPPED sender unblocks (→ Null).
    pub async fn request(&self, kind: &str, payload: Value) -> Value {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let _ = self.events.send(AgentEvent::Request { id, kind: kind.to_string(), payload });
        match self.request_timeout {
            Some(d) => match tokio::time::timeout(d, rx).await {
                // Driver answered in time (or the sender was dropped → Null).
                Ok(res) => res.unwrap_or(Value::Null),
                // Timed out: clean up the pending entry so the oneshot is not leaked
                // (a later Respond for this id then finds nothing and no-ops), and
                // degrade to Null exactly like a dropped sender.
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    Value::Null
                }
            },
            None => rx.await.unwrap_or(Value::Null),
        }
    }

    /// Route a driver response to the waiting requester.
    pub(crate) fn resolve(&self, id: RequestId, value: Value) {
        if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
            let _ = tx.send(value);
        }
    }

    /// Resolve EVERY pending request to `Value::Null` — the same degraded value as
    /// a dropped sender or a `request_timeout` expiry, so awaiting middlewares
    /// proceed fail-closed (e.g. an approval sees Null → deny). Called on
    /// `AgentCommand::Cancel`: a cancel must also unblock a turn parked inside a
    /// middleware round-trip (the turn token alone cannot — `request` awaits a
    /// oneshot, not the token). A late `Respond` for a flushed id no-ops.
    pub(crate) fn cancel_pending(&self) {
        for (_, tx) in self.pending.lock().unwrap().drain() {
            let _ = tx.send(Value::Null);
        }
    }
}
