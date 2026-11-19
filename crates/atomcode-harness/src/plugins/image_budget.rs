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
//! with half the image bytes it actually carried before the failure is let through.
//!
//! A picture can also break a session by being one the provider will not take
//! at all — a file cut short, or a format it does not read: DeepSeek answers
//! `400 … unsupported image`, and since the picture stays in the log, so does
//! every request after it. So a picture that does not decode never leaves this
//! host, and when the provider refuses the pictures anyway the request is sent
//! once more without them. Nothing is remembered about a refusal: which picture
//! it was is not said, and another model may take it.
//!
//! All of it is judged where the request is sent ([`finalize`], called by the
//! agent loop on a copy of the final messages), not here: a row nearer the
//! provider may rebuild the request from the log — compaction does — and would
//! bring every left-out picture back. This row only sets the limits for each
//! attempt and reads back what was sent. Like the budget, only the request
//! changes — never the log.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use atomcode_kernel::message::{ImageContent, Message};
use atomcode_plexus::{Context, Next, Plugin, Waterfall};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{AgentRequest, ImageLimits, ModelRequest, ModelResponse, RequestError};

/// What stands where a picture was left out.
pub const IMAGE_LEFT_OUT: &str = "[an earlier image was left out of this request to keep it \
under the size limit — read the file again if you need to see it]";

/// What stands where a picture the provider cannot take was left out.
pub const IMAGE_REFUSED: &str = "[an image was left out of this request: it is damaged or in \
a format the model service does not accept — tell the user, and do not read it again]";

/// What stands where the pictures of a request the provider refused were.
pub const IMAGE_WITHHELD: &str = "[an image was left out of this request: the model service \
refused the images in it]";

/// How many images are left out at a time.
///
/// The cut moves in steps, not image by image: a request that drops one more
/// old picture every round changes its prefix every round, and a provider's
/// prompt cache — 97% on the session this was reported from — is matched by
/// prefix. Leaving out four at once holds the cut still for the next several
/// pictures, and the cut is still worked out from the log every time, with no
/// state to keep.
const STEP: usize = 4;

/// Leave the oldest images out of `messages` until what is left fits in
/// `budget` bytes (of the encoded data, which is what goes on the wire),
/// leaving them out [`STEP`] at a time. The newest are kept. Returns how many
/// were left out.
pub fn keep_within(messages: &mut Vec<Message>, budget: usize) -> usize {
    // Every image, oldest first, by size.
    let sizes: Vec<usize> = messages
        .iter()
        .flat_map(|m| m.images.iter().map(|image| image.data.len()))
        .collect();
    let total: usize = sizes.iter().sum();
    if total <= budget {
        return 0;
    }
    // The fewest oldest that have to go for the rest to fit…
    let mut over = total - budget;
    let mut fewest: usize = 0;
    for size in &sizes {
        if over == 0 {
            break;
        }
        over = over.saturating_sub(*size);
        fewest += 1;
    }
    // …rounded up to a whole step, so the cut stays put while pictures arrive.
    let left_out = fewest.div_ceil(STEP).saturating_mul(STEP).min(sizes.len());

    let mut to_drop = left_out;
    let mut notes = Vec::new();
    for (index, message) in messages.iter_mut().enumerate() {
        if to_drop == 0 {
            break;
        }
        if message.images.is_empty() {
            continue;
        }
        let dropped = to_drop.min(message.images.len());
        message.images.drain(..dropped);
        to_drop -= dropped;
        notes.push((index, counted(IMAGE_LEFT_OUT, dropped)));
    }
    say(messages, notes);
    left_out
}

/// `note`, with how many it stands for when more than one.
fn counted(note: &str, n: usize) -> String {
    if n == 1 {
        note.to_string()
    } else {
        format!("{note} (×{n})")
    }
}

/// Say each note where its pictures were: at the end of that message — or,
/// for a pinned task specification, in a message right after it, because one
/// must reach the provider word for word (`retention::validate_request`
/// refuses the request otherwise).
fn say(messages: &mut Vec<Message>, notes: Vec<(usize, String)>) {
    for (index, note) in notes.into_iter().rev() {
        let message = &mut messages[index];
        if message.internal_origin.as_deref()
            == Some(atomcode_kernel::session::retention::PINNED_ORIGIN)
        {
            let mut beside = Message::user(note);
            beside.synthetic = true;
            messages.insert(index + 1, beside);
        } else if message.text.is_empty() {
            message.text = note;
        } else {
            message.text.push_str("\n\n");
            message.text.push_str(&note);
        }
    }
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

/// Nothing came back before the failure — no text, no reasoning, no tool call
/// begun. Only then is sending the request again this row's to do: once a reply
/// began, a second answer would replace it, and the stream recovery keeps it.
fn nothing_said(error: &RequestError) -> bool {
    error.partial.is_none() && !error.unexecuted_tool_output
}

/// A request the provider refused for a picture in it — not for its size, and
/// before any reply or tool call began (a stream that failed part way is the
/// stream recovery's: sending again here would throw away what was already said).
fn refused_image(error: &RequestError) -> bool {
    if !nothing_said(error) || error.context_overflow || too_large(error) {
        return false;
    }
    if !matches!(error.http_status, None | Some(400) | Some(422)) {
        return false;
    }
    let text = error.message.to_ascii_lowercase();
    text.contains("image")
        && [
            "unsupported",
            "not supported",
            "invalid",
            "format",
            "decode",
            "corrupt",
            "could not process",
        ]
        .iter()
        .any(|word| text.contains(word))
}

/// Which picture this is, by its bytes.
fn fingerprint(image: &ImageContent) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    image.data.hash(&mut hasher);
    hasher.finish()
}

/// Take the pictures `keep` refuses out of `messages`, saying `note` where they
/// were. `keep` may also correct a picture it keeps.
fn leave_out(
    messages: &mut Vec<Message>,
    note: &str,
    mut keep: impl FnMut(&mut ImageContent) -> bool,
) {
    let mut notes = Vec::new();
    for (index, message) in messages.iter_mut().enumerate() {
        let before = message.images.len();
        message.images.retain_mut(&mut keep);
        let gone = before - message.images.len();
        if gone > 0 {
            notes.push((index, counted(note, gone)));
        }
    }
    say(messages, notes);
}

/// What a picture really is — its media type, or `None` when it does not
/// decode — by [`fingerprint`]. Pure data about bytes, so one table serves
/// every agent and model; each picture is decoded once.
fn decoded() -> &'static Mutex<HashMap<u64, Option<&'static str>>> {
    static DECODED: OnceLock<Mutex<HashMap<u64, Option<&'static str>>>> = OnceLock::new();
    DECODED.get_or_init(Default::default)
}

/// What each picture in `messages` really is, decoding the ones not seen
/// before outside the lock, so one request is not held up behind another.
fn media_types(messages: &[Message]) -> HashMap<u64, Option<&'static str>> {
    let prints: Vec<(u64, &ImageContent)> = messages
        .iter()
        .flat_map(|m| &m.images)
        .map(|image| (fingerprint(image), image))
        .collect();
    let mut verdicts = HashMap::new();
    {
        let known = decoded().lock().unwrap_or_else(|e| e.into_inner());
        for (print, _) in &prints {
            if let Some(verdict) = known.get(print) {
                verdicts.insert(*print, *verdict);
            }
        }
    }
    let mut fresh = Vec::new();
    for (print, image) in prints {
        if let std::collections::hash_map::Entry::Vacant(slot) = verdicts.entry(print) {
            let verdict =
                atomcode_capabilities::image_normalize::base64_image_media_type(&image.data);
            slot.insert(verdict);
            fresh.push((print, verdict));
        }
    }
    if !fresh.is_empty() {
        decoded()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend(fresh);
    }
    verdicts
}

/// Make `messages` — a copy of what is about to be sent — fit `limits`: no
/// picture that does not decode (each kept one named by its bytes), then the
/// budget, or no pictures at all when the provider refused them. Records in
/// `limits` what is left to send.
///
/// The broken ones go first, so they take no room a good one could have had.
pub fn finalize(messages: &mut Vec<Message>, limits: &mut ImageLimits) {
    if messages.iter().all(|m| m.images.is_empty()) {
        limits.sent = 0;
        limits.sent_bytes = 0;
        return;
    }
    if limits.none {
        leave_out(messages, IMAGE_WITHHELD, |_| false);
    } else {
        let verdicts = media_types(messages);
        let mut broken = 0;
        leave_out(messages, IMAGE_REFUSED, |image| {
            match verdicts.get(&fingerprint(image)).copied().flatten() {
                Some(media_type) => {
                    if image.media_type != media_type {
                        image.media_type = media_type.to_string();
                    }
                    true
                }
                None => {
                    broken += 1;
                    false
                }
            }
        });
        if broken > 0 {
            tracing::warn!(broken, "images that do not decode left out of the request");
        }
        if let Some(budget) = limits.max_bytes {
            keep_within(messages, budget);
        }
    }
    let sent = messages.iter().flat_map(|m| &m.images);
    limits.sent = sent.clone().count();
    limits.sent_bytes = sent.map(|image| image.data.len()).sum();
}

struct ImageBudget {
    max_bytes: usize,
}

impl ImageBudget {
    fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
}

#[async_trait]
impl Waterfall<AgentRequest> for ImageBudget {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        // Every request, pictures or not: a row nearer the provider may add
        // them. The limits apply to what is sent; the recoveries go by what
        // was. Each once: at most three sends. A retry goes out with the
        // request as the rows left it — compaction rewrites it once a turn —
        // and changes only what may be done with its pictures.
        let mut halved = false;
        let mut refused = false;
        req.images = ImageLimits {
            max_bytes: Some(self.max_bytes),
            ..ImageLimits::default()
        };
        loop {
            // What this attempt sends is what the loop records for it — or
            // nothing, if it never reaches the provider.
            req.images.sent = 0;
            req.images.sent_bytes = 0;
            let result = next.run(req).await;
            let carried = req.images.sent > 0;
            match result {
                // A host policy refused it before any provider saw it.
                Err(error) if error.policy_denied => return Err(error),
                Err(error) if carried && !halved && nothing_said(&error) && too_large(&error) => {
                    // Halve what was actually sent, not just the configured
                    // ceiling: a gateway may reject a request below it already.
                    halved = true;
                    req.images.max_bytes = Some(req.images.sent_bytes / 2);
                }
                Err(error) if carried && !refused && refused_image(&error) => {
                    // Which picture it was is not said in a form that maps
                    // back to a message, so the request goes again with none.
                    refused = true;
                    req.images.none = true;
                    tracing::warn!(
                        refused = req.images.sent,
                        "the provider refused this request's images; sending it again without them"
                    );
                }
                other => return other,
            }
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
        let _ = ctx.on_waterfall::<AgentRequest>(Arc::new(ImageBudget::new(max_bytes)), false);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(bytes: usize) -> ImageContent {
        ImageContent {
            media_type: "image/png".into(),
            data: "x".repeat(bytes),
        }
    }

    /// A picture of `bytes` encoded bytes that decodes: a PNG followed by zeros,
    /// which a decoder stops reading at `IEND`. `bytes` is 96 plus a multiple of 4.
    fn decodable(bytes: usize) -> ImageContent {
        const UNPADDED_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAIAAAB7QOjdAAAADUlEQVR4nGP4zwAE/wEHAAH/4iOeWQAAAABJRU5ErkJgggAA";
        assert!(bytes >= UNPADDED_PNG.len() && (bytes - UNPADDED_PNG.len()) % 4 == 0);
        ImageContent {
            media_type: "image/png".into(),
            data: format!("{UNPADDED_PNG}{}", "A".repeat(bytes - UNPADDED_PNG.len())),
        }
    }

    fn carrying(text: &str, images: Vec<ImageContent>) -> Message {
        Message::user_with_images(text, images)
    }

    #[test]
    fn the_oldest_pictures_are_left_out_first_and_said_so() {
        let mut messages: Vec<Message> = (0..6)
            .map(|n| carrying(&format!("frame {n}"), vec![picture(4)]))
            .collect();
        // 24 bytes over a 20-byte budget: one has to go, so a whole step goes.
        assert_eq!(keep_within(&mut messages, 20), 4);
        for gone in &messages[..4] {
            assert!(gone.images.is_empty());
            assert!(gone.text.ends_with(IMAGE_LEFT_OUT), "{}", gone.text);
        }
        assert_eq!(messages[4].images.len(), 1);
        assert_eq!(messages[5].images.len(), 1, "the newest stays");
        assert_eq!(messages[5].text, "frame 5", "a kept one is untouched");
    }

    /// The cut holds still while pictures keep arriving, so the request's
    /// prefix — what a prompt cache matches — does not change every round.
    #[test]
    fn the_cut_moves_a_step_at_a_time_not_every_round() {
        let cut = |n: usize| {
            let mut messages: Vec<Message> =
                (0..n).map(|_| carrying("", vec![picture(4)])).collect();
            keep_within(&mut messages, 20)
        };
        assert_eq!(cut(5), 0);
        assert_eq!([cut(6), cut(7), cut(8), cut(9)], [4, 4, 4, 4]);
        assert_eq!(cut(10), 8, "and then a whole step more");
    }

    #[test]
    fn within_one_message_the_oldest_go_first() {
        let mut messages = vec![carrying("two", vec![picture(5), picture(5)])];
        assert_eq!(keep_within(&mut messages, 6), 2, "a step takes both");
        assert!(messages[0].images.is_empty());
        assert!(
            messages[0].text.starts_with("two\n\n"),
            "{}",
            messages[0].text
        );
        assert!(messages[0].text.ends_with("(×2)"), "{}", messages[0].text);
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

    /// A 2×1 PNG, and the same file cut short: the header still parses, the
    /// pixels do not.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAIAAAB7QOjdAAAADUlEQVR4nGP4zwAE/wEHAAH/4iOeWQAAAABJRU5ErkJggg==";
    const CUT_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAIAAAB7QOjd";

    fn png(data: &str) -> ImageContent {
        ImageContent {
            media_type: "image/png".into(),
            data: data.into(),
        }
    }

    fn request(messages: Vec<Message>) -> ModelRequest {
        ModelRequest {
            messages,
            tools: vec![],
            options: Default::default(),
            turn: 1,
            round: 1,
            answering_a_nudge: false,
            images: Default::default(),
        }
    }

    fn images_in(messages: &[Message]) -> usize {
        messages.iter().map(|m| m.images.len()).sum()
    }

    /// DeepSeek's words for a picture it will not take, as the user saw them.
    const REFUSED: &str = "HTTP 400: [invalid_request_error/invalid_request_error] \
        .messages[10].content[0]: You have uploaded an unsupported image. Please make sure \
        your image is valid and in a supported image format.";

    fn refused() -> RequestError {
        let mut error = RequestError::message(REFUSED);
        error.http_status = Some(400);
        error
    }

    /// What one request went through: its result, how many pictures each
    /// attempt sent, what the last attempt sent, and the request afterwards.
    struct Sent {
        result: Result<ModelResponse, RequestError>,
        seen: Vec<usize>,
        last: Vec<Message>,
        req: ModelRequest,
    }

    /// Run one request through `row` — and through `inner`, rows that sit
    /// between it and the provider — to an end that does what the agent loop
    /// does with a request (`finalize` a copy, send that), the provider
    /// answering with `answer` given how many pictures it was sent.
    async fn send_through(
        row: Arc<ImageBudget>,
        inner: Vec<Arc<dyn Waterfall<AgentRequest>>>,
        messages: Vec<Message>,
        answer: fn(usize) -> Result<(), RequestError>,
    ) -> Sent {
        use atomcode_plexus::{App, ConfigTree, PluginRegistry};
        let app = App::new(PluginRegistry::new(), ConfigTree::from_layers([]).unwrap());
        let mut guards = vec![app.context().on_waterfall::<AgentRequest>(row, false)];
        for listener in inner {
            guards.push(app.context().on_waterfall::<AgentRequest>(listener, false));
        }
        let mut req = request(messages);
        let seen = Mutex::new(Vec::new());
        let last = Mutex::new(Vec::new());
        let result = app
            .context()
            .waterfall::<AgentRequest, _>(&mut req, |req| {
                let mut sent = req.messages.clone();
                finalize(&mut sent, &mut req.images);
                let carried = images_in(&sent);
                seen.lock().unwrap().push(carried);
                *last.lock().unwrap() = sent;
                Box::pin(async move { answer(carried).map(|()| ModelResponse::default()) })
            })
            .await;
        drop(guards);
        Sent {
            result,
            seen: seen.into_inner().unwrap(),
            last: last.into_inner().unwrap(),
            req,
        }
    }

    async fn send(
        row: Arc<ImageBudget>,
        messages: Vec<Message>,
        answer: fn(usize) -> Result<(), RequestError>,
    ) -> Sent {
        send_through(row, vec![], messages, answer).await
    }

    fn row() -> Arc<ImageBudget> {
        Arc::new(ImageBudget::new(8 * 1024 * 1024))
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_413_below_the_configured_budget_retries_with_fewer_images() {
        fn too_large(_: usize) -> Result<(), RequestError> {
            let mut error = RequestError::message("payload too large");
            error.http_status = Some(413);
            Err(error)
        }
        let frames = vec![carrying("frames", vec![decodable(100); 6])];
        let sent = send(row(), frames, too_large).await;
        assert!(sent.result.is_err(), "a second refusal must propagate");
        assert_eq!(sent.seen, [6, 2], "600 bytes, then half: two");
        assert_eq!(
            sent.req.messages[0].images.len(),
            6,
            "the request as projected is untouched"
        );
        assert_eq!(sent.last[0].text.matches(IMAGE_LEFT_OUT).count(), 1);
    }

    /// The reported session: a picture that does not decode was sent, refused,
    /// and — being in the history — sent and refused on every request after.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_picture_that_does_not_decode_is_never_sent() {
        let messages = vec![
            carrying("good", vec![png(PNG)]),
            carrying("", vec![png(CUT_PNG)]),
        ];
        let sent = send(row(), messages, |_| Ok(())).await;
        assert!(sent.result.is_ok());
        assert_eq!(sent.seen, [1], "only the good picture goes, in one request");
        assert!(
            sent.last[1].text.contains(IMAGE_REFUSED),
            "{}",
            sent.last[1].text
        );
        assert_eq!(sent.last[0].images.len(), 1);
    }

    /// A row between this one and the provider may rebuild the request from the
    /// log — compaction does — and so bring back every picture this row left
    /// out. What goes to the provider is checked where it is sent.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_rebuilt_request_cannot_bring_a_broken_picture_back() {
        struct Rebuild;
        #[async_trait]
        impl Waterfall<AgentRequest> for Rebuild {
            async fn handle(
                &self,
                req: &mut ModelRequest,
                next: Next<'_, AgentRequest>,
            ) -> Result<ModelResponse, RequestError> {
                req.messages = vec![carrying("summary", vec![png(CUT_PNG)])];
                next.run(req).await
            }
        }
        let messages = vec![carrying("look", vec![png(PNG)])];
        let sent = send_through(row(), vec![Arc::new(Rebuild)], messages, |_| Ok(())).await;
        assert_eq!(sent.seen, [0]);
        assert!(sent.last[0].text.contains(IMAGE_REFUSED));
    }

    /// A picture that does not decode takes no room in the budget: the good one
    /// it would have pushed out still goes.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_broken_picture_does_not_take_a_good_ones_place() {
        let messages = vec![
            carrying("older", vec![decodable(100)]),
            carrying("newer", vec![png(CUT_PNG)]),
        ];
        let sent = send(Arc::new(ImageBudget::new(100)), messages, |_| Ok(())).await;
        assert_eq!(sent.seen, [1]);
        assert!(
            !sent.last[0].text.contains(IMAGE_LEFT_OUT),
            "{}",
            sent.last[0].text
        );
    }

    /// A picture this host decodes but the provider will not take: the request
    /// goes again without pictures — every time, since nothing says which one
    /// it was or whether the next model would take it.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_picture_the_provider_refuses_does_not_end_the_session() {
        fn deepseek(carried: usize) -> Result<(), RequestError> {
            if carried == 0 {
                Ok(())
            } else {
                Err(refused())
            }
        }
        let row = row();
        let history = vec![carrying("look", vec![png(PNG)])];
        let sent = send(row.clone(), history.clone(), deepseek).await;
        assert!(sent.result.is_ok(), "{:?}", sent.result);
        assert_eq!(sent.seen, [1, 0]);
        assert!(sent.last[0].text.contains(IMAGE_WITHHELD));
        let sent = send(row, history, deepseek).await;
        assert!(sent.result.is_ok());
        assert_eq!(
            sent.seen,
            [1, 0],
            "and the next request recovers the same way"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_refusal_that_is_not_about_a_picture_is_let_through() {
        fn bad_request(_: usize) -> Result<(), RequestError> {
            let mut error = RequestError::message("HTTP 400: invalid tool_call_id");
            error.http_status = Some(400);
            Err(error)
        }
        let messages = vec![carrying("look", vec![png(PNG)])];
        let sent = send(row(), messages, bad_request).await;
        assert!(sent.result.is_err());
        assert_eq!(sent.seen, [1], "not retried");
    }

    fn pinned(text: &str, images: Vec<ImageContent>) -> Message {
        let mut message = carrying(text, images);
        message.internal_origin =
            Some(atomcode_kernel::session::retention::PINNED_ORIGIN.to_string());
        message
    }

    /// A pinned task specification must reach the provider word for word —
    /// `validate_request` refuses the request otherwise — so what is said about
    /// its picture goes beside it, not into it.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_pinned_message_keeps_its_words_when_its_picture_is_left_out() {
        let messages = vec![pinned("the spec", vec![png(CUT_PNG)])];
        let sent = send(row(), messages, |_| Ok(())).await;
        assert!(sent.result.is_ok());
        assert_eq!(sent.last[0].text, "the spec");
        assert!(sent.last[0].images.is_empty());
        assert!(
            sent.last.iter().any(|m| m.text.contains(IMAGE_REFUSED)),
            "said beside it"
        );
    }

    #[test]
    fn a_pinned_message_keeps_its_words_when_the_budget_cuts_its_pictures() {
        let mut messages = vec![pinned("the spec", vec![picture(4); 6])];
        assert_eq!(keep_within(&mut messages, 8), 4);
        assert_eq!(messages[0].text, "the spec");
        assert!(messages.iter().any(|m| m.text.contains(IMAGE_LEFT_OUT)));
    }

    /// Refused with two pictures, the provider said nothing about which: the
    /// one it did take must still go next time.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_refusal_of_a_batch_does_not_shut_out_its_good_pictures() {
        fn refuses_the_pair(carried: usize) -> Result<(), RequestError> {
            if carried < 2 {
                Ok(())
            } else {
                Err(refused())
            }
        }
        let row = row();
        let pair = vec![carrying("two", vec![png(PNG), decodable(100)])];
        let sent = send(row.clone(), pair, refuses_the_pair).await;
        assert!(sent.result.is_ok());
        assert_eq!(sent.seen, [2, 0]);
        let good = vec![carrying("one", vec![decodable(100)])];
        let sent = send(row, good, refuses_the_pair).await;
        assert_eq!(sent.seen, [1], "the good picture is not shut out");
    }

    /// Too large first, then a refused picture: both recoveries apply, in turn.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_refused_picture_after_a_413_is_still_recovered() {
        fn gateway(carried: usize) -> Result<(), RequestError> {
            match carried {
                0 => Ok(()),
                6 => {
                    let mut error = RequestError::message("payload too large");
                    error.http_status = Some(413);
                    Err(error)
                }
                _ => Err(refused()),
            }
        }
        let frames = vec![carrying("frames", vec![decodable(100); 6])];
        let sent = send(row(), frames, gateway).await;
        assert!(sent.result.is_ok(), "{:?}", sent.result);
        assert_eq!(sent.seen, [6, 2, 0]);
    }

    /// A refusal that arrives after the reply began is the stream recovery's to
    /// handle: sending again here would throw away what was already said.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_refusal_after_output_began_is_not_resent() {
        fn mid_stream(_: usize) -> Result<(), RequestError> {
            let mut error = refused();
            error.partial = Some(Box::new(ModelResponse {
                text: "half a reply".into(),
                ..Default::default()
            }));
            Err(error)
        }
        let messages = vec![carrying("look", vec![png(PNG)])];
        let sent = send(row(), messages, mid_stream).await;
        assert_eq!(sent.seen, [1]);
        assert_eq!(
            sent.result.unwrap_err().partial.unwrap().text,
            "half a reply"
        );
    }

    /// Nor after the model began writing a tool call, with nothing else yet.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_refusal_after_a_tool_call_began_is_not_resent() {
        fn mid_call(_: usize) -> Result<(), RequestError> {
            let mut error = refused();
            error.unexecuted_tool_output = true;
            Err(error)
        }
        let messages = vec![carrying("look", vec![png(PNG)])];
        let sent = send(row(), messages, mid_call).await;
        assert_eq!(sent.seen, [1]);
        assert!(sent.result.unwrap_err().unexecuted_tool_output);
    }

    /// Compaction rewrites the request once per turn, nearer the provider than
    /// this row: a retry must go out with what it made of the request, not with
    /// what this row was handed.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_retry_keeps_what_a_row_nearer_the_provider_made_of_the_request() {
        struct CompactOnce(std::sync::atomic::AtomicBool);
        #[async_trait]
        impl Waterfall<AgentRequest> for CompactOnce {
            async fn handle(
                &self,
                req: &mut ModelRequest,
                next: Next<'_, AgentRequest>,
            ) -> Result<ModelResponse, RequestError> {
                if !self.0.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    req.messages = vec![carrying("the summary", vec![png(PNG)])];
                }
                next.run(req).await
            }
        }
        fn deepseek(carried: usize) -> Result<(), RequestError> {
            if carried == 0 {
                Ok(())
            } else {
                Err(refused())
            }
        }
        let messages = vec![carrying("the long history", vec![png(PNG)])];
        let compact = Arc::new(CompactOnce(Default::default()));
        let sent = send_through(row(), vec![compact], messages, deepseek).await;
        assert!(sent.result.is_ok(), "{:?}", sent.result);
        assert_eq!(sent.seen, [1, 0]);
        assert!(
            sent.last[0].text.starts_with("the summary"),
            "the retry undid the compaction: {}",
            sent.last[0].text
        );
    }

    /// A row nearer the provider may add the pictures itself: the budget and
    /// the recoveries still hold for what is actually sent.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn pictures_added_nearer_the_provider_are_held_to_the_budget() {
        struct AddFrames;
        #[async_trait]
        impl Waterfall<AgentRequest> for AddFrames {
            async fn handle(
                &self,
                req: &mut ModelRequest,
                next: Next<'_, AgentRequest>,
            ) -> Result<ModelResponse, RequestError> {
                if req.messages.iter().all(|m| m.images.is_empty()) {
                    req.messages
                        .push(carrying("frames", vec![decodable(96); 6]));
                }
                next.run(req).await
            }
        }
        let text_only = vec![carrying("look", vec![])];
        let sent = send_through(
            Arc::new(ImageBudget::new(320)),
            vec![Arc::new(AddFrames)],
            text_only.clone(),
            |_| Ok(()),
        )
        .await;
        assert_eq!(sent.seen, [2], "576 bytes over 320: a step of four goes");

        fn deepseek(carried: usize) -> Result<(), RequestError> {
            if carried == 0 {
                Ok(())
            } else {
                Err(refused())
            }
        }
        let sent = send_through(row(), vec![Arc::new(AddFrames)], text_only, deepseek).await;
        assert!(sent.result.is_ok(), "{:?}", sent.result);
        assert_eq!(sent.seen, [6, 0]);
    }

    /// A 413 that arrives after the reply began is not sent again either: the
    /// second answer would replace what was already said.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_413_after_output_began_is_not_resent() {
        fn mid_stream(_: usize) -> Result<(), RequestError> {
            let mut error = RequestError::message("payload too large");
            error.http_status = Some(413);
            error.partial = Some(Box::new(ModelResponse {
                text: "half a reply".into(),
                ..Default::default()
            }));
            Err(error)
        }
        let frames = vec![carrying("frames", vec![decodable(100); 6])];
        let sent = send(row(), frames, mid_stream).await;
        assert_eq!(sent.seen, [6]);
        assert_eq!(
            sent.result.unwrap_err().partial.unwrap().text,
            "half a reply"
        );
    }

    /// An attempt that never reached the provider sent nothing: a host policy
    /// refusing it is not the provider refusing pictures, and is not resent.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_policy_refusal_before_the_provider_is_not_taken_for_its_answer() {
        /// Lets the first attempt through, refuses the second, and would let
        /// a third through.
        struct DenySecond(std::sync::atomic::AtomicUsize);
        #[async_trait]
        impl Waterfall<AgentRequest> for DenySecond {
            async fn handle(
                &self,
                req: &mut ModelRequest,
                next: Next<'_, AgentRequest>,
            ) -> Result<ModelResponse, RequestError> {
                if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 1 {
                    let mut denied =
                        RequestError::message("denied by host policy: unsupported image");
                    denied.policy_denied = true;
                    return Err(denied);
                }
                next.run(req).await
            }
        }
        fn too_large_once(carried: usize) -> Result<(), RequestError> {
            if carried < 6 {
                return Ok(());
            }
            let mut error = RequestError::message("payload too large");
            error.http_status = Some(413);
            Err(error)
        }
        let frames = vec![carrying("frames", vec![decodable(100); 6])];
        let deny = Arc::new(DenySecond(Default::default()));
        let sent = send_through(row(), vec![deny], frames, too_large_once).await;
        assert_eq!(sent.seen, [6], "the provider was asked once");
        assert!(sent.result.unwrap_err().policy_denied);
    }

    /// The label a picture arrived with is not trusted: it goes out named by
    /// its bytes.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_picture_goes_out_named_by_its_bytes() {
        let mislabeled = ImageContent {
            media_type: "image/jpeg".into(),
            data: PNG.into(),
        };
        let sent = send(row(), vec![carrying("look", vec![mislabeled])], |_| Ok(())).await;
        assert_eq!(sent.last[0].images[0].media_type, "image/png");
    }

    #[test]
    fn a_refused_picture_is_recognised_by_what_it_says() {
        let refused = |text: &str| {
            let mut error = RequestError::message(text);
            error.http_status = Some(400);
            refused_image(&error)
        };
        assert!(refused(REFUSED));
        assert!(refused("Invalid image data"));
        assert!(refused("Could not process image"));
        assert!(!refused("invalid tool_call_id"));
        assert!(!refused("context_length_exceeded"));
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
