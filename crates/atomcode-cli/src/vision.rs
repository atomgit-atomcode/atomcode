// crates/atomcode-cli/src/vision.rs
//
// Bridges the coding runtime's `ImagePreprocessor` seam (a `core`-free trait)
// to `atomcode_core::vision_preprocessor::maybe_preprocess`.
//
// Why this exists: `atomcode-coding` deliberately has no `atomcode-core`
// dependency, so it can't call `maybe_preprocess` itself. It instead exposes
// the `ImagePreprocessor` hook (like `provider_factory`), and the CLI — which
// DOES have `core` — injects this implementation. Restores the TUI's VL
// image recognition that was dropped when the legacy `atomcode-bridge` (which
// did this in its turn handler) was retired.

use atomcode_coding::{ImageContent, ImagePreprocessor, UserInput, VisionNotice};
use atomcode_config::config::Config;
use atomcode_core::conversation::message::ImagePart;
use atomcode_core::provider::create_provider;
use atomcode_core::vision_preprocessor::{maybe_preprocess, PreprocessOutcome};

/// VL preprocessing for the local TUI runtime. When the active (main) model
/// can't accept images, converts them to a text description via the configured
/// VL provider and clears the images; a vision-capable model passes through
/// unchanged. Any failure to load config / build the provider degrades to
/// sending the original `(text, images)` rather than blocking the turn.
pub struct VlImagePreprocessor;

#[async_trait::async_trait]
impl ImagePreprocessor for VlImagePreprocessor {
    async fn preprocess(
        &self,
        text: String,
        images: Vec<ImageContent>,
        active_model: String,
        session_id: Option<String>,
    ) -> (UserInput, Option<VisionNotice>) {
        if images.is_empty() {
            return (UserInput { text, images }, None);
        }
        let config = match Config::load(&Config::default_path()) {
            Ok(c) => c,
            Err(_) => return (UserInput { text, images }, None),
        };
        // Build the ACTIVE provider — the one whose model matches the runtime's
        // resolved turn model (`active_model`), so a `--provider` / `/model`
        // selection is honoured and the vision-capability check inside
        // `maybe_preprocess` is made against the real main model rather than a
        // stale `default_provider`. Fall back to the default if no config entry
        // carries that model (e.g. an ephemeral provider).
        let pc = match config
            .providers
            .values()
            .find(|pc| pc.model == active_model)
            .or_else(|| config.providers.get(&config.default_provider))
            .cloned()
        {
            Some(pc) => pc,
            None => return (UserInput { text, images }, None),
        };
        // `create_provider` can do blocking auth I/O (token read + refresh over
        // the network); run it off the async owner task so a slow auth host
        // can't stall the runtime loop — same treatment `maybe_preprocess`
        // gives the VL provider build.
        let active = match tokio::task::spawn_blocking(move || create_provider(&pc)).await {
            Ok(Ok(p)) => p,
            _ => return (UserInput { text, images }, None),
        };
        // Forward the conversation's session id onto the one-off VL call so a
        // gateway pins it to the same upstream account/replica as the main turn.
        if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
            active.set_session_id(&sid);
        }
        let parts: Vec<ImagePart> = images
            .iter()
            .map(|i| ImagePart {
                media_type: i.media_type.clone(),
                data: i.data.clone(),
            })
            .collect();
        let outcome = maybe_preprocess(&config, active.as_ref(), &text, &parts).await;
        apply_outcome(text, images, outcome)
    }
}

/// Map a `maybe_preprocess` outcome to `(UserInput, notice)`. Pure (no I/O) so
/// the wrapping/`char_count` logic is unit-testable without a live VL provider.
///
/// `Skipped` (vision model) passes through with images kept; `Replaced`/`Failed`
/// clear the images from the model request (a non-vision model can't take them)
/// and fold a caption / failure marker into the text. On failure the TUI
/// re-attaches the images it remembers from submit, so the notice carries none.
fn apply_outcome(
    text: String,
    images: Vec<ImageContent>,
    outcome: PreprocessOutcome,
) -> (UserInput, Option<VisionNotice>) {
    match outcome {
        PreprocessOutcome::Skipped => (UserInput { text, images }, None),
        PreprocessOutcome::Replaced { text: vl, vl_model } => {
            // char_count is the VL description length — computed BEFORE merging
            // with the caption, so the toast reports the recognised content size.
            let char_count = vl.chars().count();
            let merged = if text.trim().is_empty() {
                format!("[图片内容（由 {vl_model} 识别）]\n{vl}")
            } else {
                format!("{text}\n\n[图片内容（由 {vl_model} 识别）]\n{vl}")
            };
            (
                UserInput {
                    text: merged,
                    images: Vec::new(),
                },
                Some(VisionNotice::Recognised {
                    vl_model,
                    char_count,
                }),
            )
        }
        PreprocessOutcome::Failed { reason } => {
            let merged = if text.trim().is_empty() {
                "[图片识别失败]".to_string()
            } else {
                format!("{text}\n\n[图片识别失败]")
            };
            (
                UserInput {
                    text: merged,
                    images: Vec::new(),
                },
                Some(VisionNotice::Failed { reason }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img() -> ImageContent {
        ImageContent {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }
    }

    #[test]
    fn replaced_reports_vl_char_count_not_merged_length() {
        // Multi-byte VL text: char_count must count CHARACTERS of the VL output,
        // NOT bytes and NOT the merged (caption + wrapper + VL) length.
        let (input, notice) = apply_outcome(
            "看这张图".to_string(),
            vec![img()],
            PreprocessOutcome::Replaced {
                text: "你好世界".to_string(), // 4 chars, 12 bytes
                vl_model: "qwen-vl".to_string(),
            },
        );
        assert!(
            input.images.is_empty(),
            "Replaced clears images for the model"
        );
        assert!(
            input.text.contains("你好世界"),
            "VL text folded into caption"
        );
        match notice {
            Some(VisionNotice::Recognised {
                vl_model,
                char_count,
            }) => {
                assert_eq!(char_count, 4, "must be VL char count, not bytes/merged len");
                assert_eq!(vl_model, "qwen-vl");
            }
            other => panic!("expected Recognised, got {other:?}"),
        }
    }

    #[test]
    fn failed_clears_images_and_carries_no_payload() {
        let (input, notice) = apply_outcome(
            "hi".to_string(),
            vec![img()],
            PreprocessOutcome::Failed {
                reason: "boom".into(),
            },
        );
        assert!(input.images.is_empty());
        assert!(input.text.contains("[图片识别失败]"));
        assert!(matches!(notice, Some(VisionNotice::Failed { reason }) if reason == "boom"));
    }

    #[test]
    fn skipped_keeps_images_and_emits_no_notice() {
        let (input, notice) =
            apply_outcome("hi".to_string(), vec![img()], PreprocessOutcome::Skipped);
        assert_eq!(input.images.len(), 1, "vision model keeps images");
        assert_eq!(input.text, "hi");
        assert!(notice.is_none());
    }
}
