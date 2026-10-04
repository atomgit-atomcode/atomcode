//! `image-budget` — how many image bytes one model request may carry.
//!
//! An image a tool returned (`read_file` on a `.png`) stays in the session log,
//! and the log is what every request is projected from: so every later request
//! sends every earlier picture again. A model checking rendered frames one by
//! one reads eight of them, and the ninth request is over the gateway's body
//! limit — `HTTP 413 ATOMCODE_BODY_TOO_LARGE` — and so is every request after
//! it, because nothing ever leaves the history. The turn stops and the session
//! cannot go on.
//!
//! This row keeps a request under a byte budget by leaving the **oldest**
//! pictures out of it, newest kept, each one left out replaced where it was by a
//! line saying so (the read that produced it still names the file, so the model
//! can read it again). Only the request changes: the log keeps every image, a
//! resume or a later model with more room sees them all. That is how oh-my-pi
//! bounds a request's images (`clampProviderContextImages`); its budget is a
//! count per provider, this one is bytes, because the limit that bites here is
//! a gateway's request body.
//!
//! And when a request comes back too large anyway — the budget was set above
//! what the gateway takes, or the text alone is large — it is sent once more
//! with half the budget before the failure is let through.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{AgentRequest, ModelRequest, ModelResponse, RequestError};

/// What stands where a picture was left out.
pub const IMAGE_LEFT_OUT: &str = "[an earlier image was left out of this request to keep it \
under the size limit — read the file again if you need to see it]";

/// Leave the oldest images out of `messages` until what is left fits in
/// `budget` bytes (of the encoded data, which is what goes on the wire). The
/// newest are kept. Returns how many were left out.
pub fn keep_within(messages: &mut [Message], budget: usize) -> usize {
    let mut kept = 0usize;
    let mut left_out = 0usize;
    for message in messages.iter_mut().rev() {
        if message.images.is_empty() {
            continue;
        }
        let before = message.images.len();
        // Newest first within a message too: the last picture in it is the one
        // the model was looking at.
        let mut keep = Vec::with_capacity(before);
        for image in message.images.drain(..).rev() {
            let size = image.data.len();
            if kept + size <= budget {
                kept += size;
                keep.push(image);
            }
        }
        keep.reverse();
        let dropped = before - keep.len();
        message.images = keep;
        if dropped > 0 {
            left_out += dropped;
            let note = if dropped == 1 {
                IMAGE_LEFT_OUT.to_string()
            } else {
                format!("{IMAGE_LEFT_OUT} (×{dropped})")
            };
            if message.text.is_empty() {
                message.text = note;
            } else {
                message.text.push_str("\n\n");
                message.text.push_str(&note);
            }
        }
    }
    left_out
}

/// A request the provider refused for its size — the body, not the context.
fn too_large(error: &RequestError) -> bool {
    if error.http_status == Some(413) {
        return true;
    }
    let text = error.message.to_ascii_lowercase();
    text.contains("body_too_large")
        || text.contains("payload too large")
        || text.contains("request entity too large")
        || error.message.contains("请求体超过")
}

struct ImageBudget {
    max_bytes: usize,
}

#[async_trait]
impl Waterfall<AgentRequest> for ImageBudget {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        if !req.messages.iter().any(|m| !m.images.is_empty()) {
            return next.run(req).await;
        }
        // Kept whole, so a second try is cut from what the log said rather than
        // from the first cut — one note per message, not one per attempt.
        let whole = req.messages.clone();
        keep_within(&mut req.messages, self.max_bytes);
        let result = next.run(req).await;
        match result {
            Err(error)
                if too_large(&error) && req.messages.iter().any(|m| !m.images.is_empty()) =>
            {
                req.messages = whole;
                keep_within(&mut req.messages, self.max_bytes / 2);
                next.run(req).await
            }
            other => other,
        }
    }
}

pub struct ImageBudgetPlugin;

#[async_trait]
impl Plugin for ImageBudgetPlugin {
    fn name(&self) -> &'static str {
        "image-budget"
    }
    fn description(&self) -> &'static str {
        "how many image bytes one model request carries; the oldest are left out first"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default = "default_max_bytes")]
            max_bytes: usize,
        }
        fn default_max_bytes() -> usize {
            8 * 1024 * 1024
        }
        let max_bytes = if config.is_null() {
            default_max_bytes()
        } else {
            serde_json::from_value::<Row>(config.clone())
                .map_err(|e| format!("bad config: {e}"))?
                .max_bytes
        };
        let _ = ctx.on_waterfall::<AgentRequest>(Arc::new(ImageBudget { max_bytes }), false);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::ImageContent;

    fn picture(bytes: usize) -> ImageContent {
        ImageContent {
            media_type: "image/png".into(),
            data: "x".repeat(bytes),
        }
    }

    fn carrying(text: &str, images: Vec<ImageContent>) -> Message {
        Message::user_with_images(text, images)
    }

    #[test]
    fn the_oldest_pictures_are_left_out_first_and_said_so() {
        let mut messages = vec![
            carrying("", vec![picture(4)]),
            Message::assistant("looked", vec![]),
            carrying("", vec![picture(4)]),
            carrying("frame", vec![picture(4)]),
        ];
        assert_eq!(keep_within(&mut messages, 9), 1);
        assert!(messages[0].images.is_empty(), "the oldest went");
        assert_eq!(messages[0].text, IMAGE_LEFT_OUT);
        assert_eq!(messages[2].images.len(), 1);
        assert_eq!(messages[3].images.len(), 1, "the newest stays");
        assert_eq!(messages[3].text, "frame", "a kept one is untouched");
    }

    #[test]
    fn within_one_message_the_last_picture_is_kept() {
        let mut messages = vec![carrying("two", vec![picture(5), picture(5)])];
        assert_eq!(keep_within(&mut messages, 6), 1);
        assert_eq!(messages[0].images.len(), 1);
        assert!(
            messages[0].text.starts_with("two\n\n"),
            "{}",
            messages[0].text
        );
    }

    #[test]
    fn under_budget_nothing_changes() {
        let mut messages = vec![
            carrying("a", vec![picture(3)]),
            carrying("b", vec![picture(3)]),
        ];
        let before = messages.clone();
        assert_eq!(keep_within(&mut messages, 100), 0);
        assert_eq!(messages, before);
    }

    #[test]
    fn a_body_too_large_is_recognised_by_status_or_by_what_it_says() {
        let mut by_status = RequestError::message("whatever");
        by_status.http_status = Some(413);
        assert!(too_large(&by_status));
        assert!(too_large(&RequestError::message(
            "HTTP 413: [ATOMCODE_BODY_TOO_LARGE] 请求体超过 20MB 限制"
        )));
        assert!(too_large(&RequestError::message("Payload Too Large")));
        assert!(!too_large(&RequestError::message(
            "context_length_exceeded"
        )));
    }
}
