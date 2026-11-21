//! Transient UI attachment correlation. Receipts claim exactly one source fact;
//! cancellation/generation drop releases bytes without changing model input.
use atomcode_kernel::message::ImageContent;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

#[derive(Default)]
pub(crate) struct DisplayInputs(Mutex<State>);
#[derive(Default)]
struct State {
    pending: HashMap<String, Input>,
    claimed: HashMap<u64, VecDeque<Input>>,
}
struct Input {
    receipt: String,
    text: String,
    images: Vec<ImageContent>,
}
impl atomcode_plexus::ServiceKey for DisplayInputs {
    const NAME: &'static str = "display-input-images";
    const MODE: atomcode_plexus::SeamMode = atomcode_plexus::SeamMode::Core;
    const TITLE: &'static str = "Original attachments waiting for a committed user fact";
    type Face = Self;
}
impl DisplayInputs {
    pub(crate) fn stage(&self, receipt: String, text: String, images: Vec<ImageContent>) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .insert(
                receipt.clone(),
                Input {
                    receipt,
                    text,
                    images,
                },
            );
    }
    pub(crate) fn claim(&self, receipt: &str, turn: u64) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(input) = state.pending.remove(receipt) {
            state.claimed.entry(turn).or_default().push_back(input);
        }
    }
    pub(crate) fn withdraw(&self, receipt: &str) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.remove(receipt);
        for queue in state.claimed.values_mut() {
            queue.retain(|input| input.receipt != receipt);
        }
    }
    pub(crate) fn take(&self, turn: u64, text: &str) -> Vec<ImageContent> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(queue) = state.claimed.get_mut(&turn) else {
            return Vec::new();
        };
        let Some(input) = queue.pop_front() else {
            return Vec::new();
        };
        if input.text == text {
            input.images
        } else {
            queue.clear();
            Vec::new()
        }
    }
    pub(crate) fn finish(&self, turn: u64) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .claimed
            .remove(&turn);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn img(data: &str) -> Vec<ImageContent> {
        vec![ImageContent {
            media_type: "image/png".into(),
            data: data.into(),
        }]
    }
    #[test]
    fn receipts_prevent_repeated_prompts_or_cancelled_inputs_from_stealing_images() {
        let inputs = DisplayInputs::default();
        inputs.stage("cancel".into(), "same".into(), img("old"));
        inputs.withdraw("cancel");
        inputs.claim("cancel", 1);
        assert!(inputs.take(1, "same").is_empty());
        inputs.stage("one".into(), "same".into(), img("first"));
        inputs.stage("two".into(), "same".into(), img("second"));
        inputs.claim("one", 2);
        inputs.claim("two", 2);
        assert_eq!(inputs.take(2, "same")[0].data, "first");
        assert_eq!(inputs.take(2, "same")[0].data, "second");
        inputs.stage("rewrite".into(), "before".into(), img("unsafe"));
        inputs.claim("rewrite", 3);
        assert!(inputs.take(3, "changed by hook").is_empty());
        assert!(inputs.take(3, "before").is_empty());
    }
}
