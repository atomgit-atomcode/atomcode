//! A tail added to one request — a reminder, a nudge — and taken off again.
//!
//! A row may end the request with words for this round only: they ride past
//! the log check and are never logged. But the same request goes down the
//! chain again whenever a row nearer the front sends it again (`llm-retry`,
//! a rate limit waited out, a stream recovered, a picture refused), and a row
//! that only appends would append again: the resent request carried every
//! reminder twice. So the tail is added, the rest of the chain runs, and the
//! tail is taken off before the request goes back — the request leaves this
//! row as it came.

use atomcode_kernel::message::Message;
use atomcode_plexus::Next;

use crate::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};

/// Send `req` on with `tail` at its end, and take the tail off again after.
///
/// It is taken off only where it was put and only if it is still there: a row
/// nearer the provider that rebuilt the request has already dropped it.
pub async fn with_tail(
    req: &mut ModelRequest,
    tail: Vec<Message>,
    next: Next<'_, AgentRequest>,
) -> Result<ModelResponse, RequestError> {
    if tail.is_empty() {
        return next.run(req).await;
    }
    let at = req.messages.len();
    req.messages.extend(tail.iter().cloned());
    let result = next.run(req).await;
    take_off(&mut req.messages, at, &tail);
    result
}

/// Take `tail` off `messages` where it was put at `at`, if it is still there —
/// for a row that has more to do between adding a tail and sending.
pub fn take_off(messages: &mut Vec<Message>, at: usize, tail: &[Message]) {
    let end = at + tail.len();
    if !tail.is_empty() && messages.get(at..end) == Some(tail) {
        messages.drain(at..end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use atomcode_plexus::{App, ConfigTree, PluginRegistry, Waterfall};
    use std::sync::{Arc, Mutex};

    struct Remind;
    #[async_trait]
    impl Waterfall<AgentRequest> for Remind {
        async fn handle(
            &self,
            req: &mut ModelRequest,
            next: Next<'_, AgentRequest>,
        ) -> Result<ModelResponse, RequestError> {
            with_tail(req, vec![Message::user("reminder")], next).await
        }
    }

    /// Sends the request on twice, the way a retry does.
    struct SendTwice;
    #[async_trait]
    impl Waterfall<AgentRequest> for SendTwice {
        async fn handle(
            &self,
            req: &mut ModelRequest,
            next: Next<'_, AgentRequest>,
        ) -> Result<ModelResponse, RequestError> {
            let _ = next.run(req).await;
            next.run(req).await
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_resent_request_carries_the_tail_once() {
        let app = App::new(PluginRegistry::new(), ConfigTree::from_layers([]).unwrap());
        let _outer = app
            .context()
            .on_waterfall::<AgentRequest>(Arc::new(SendTwice), false);
        let _inner = app
            .context()
            .on_waterfall::<AgentRequest>(Arc::new(Remind), false);
        let mut req = ModelRequest {
            messages: vec![Message::user("question")],
            tools: vec![],
            options: Default::default(),
            turn: 1,
            round: 1,
            answering_a_nudge: false,
        };
        let seen = Mutex::new(Vec::new());
        let _ = app
            .context()
            .waterfall::<AgentRequest, _>(&mut req, |req| {
                seen.lock().unwrap().push(req.messages.len());
                Box::pin(async { Ok(ModelResponse::default()) })
            })
            .await;
        assert_eq!(*seen.lock().unwrap(), [2, 2]);
        assert_eq!(req.messages.len(), 1, "the request leaves as it came");
    }
}
