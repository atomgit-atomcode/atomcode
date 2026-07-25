// crates/atomcode-cli/src/vision.rs
//
// Bridges the coding runtime's `ImagePreprocessor` seam to the kernel-native
// `atomcode_coding::vision::run_vl_caption`. The CLI owns building the VL
// provider (via `derive_tier_config` + the coding provider factory) from the
// configured `vision_preprocessor_provider`; the parity-critical streaming /
// outcome lives in coding. No `atomcode-core` dependency.
//
// Restores the TUI's VL image recognition that was dropped when the legacy
// `atomcode-bridge` (which did this in its turn handler) was retired.

use atomcode_coding::vision::{run_vl_caption, should_skip, vl_model_display, PreprocessOutcome};
use atomcode_coding::{
    derive_tier_config, CodingAgentConfig, CodingProviderFactory, ImageContent, ImagePreprocessor,
    UserInput, VisionNotice,
};
use atomcode_config::config::Config;
use std::sync::Arc;

/// VL preprocessing for the local TUI runtime. Carries the provider factory and
/// a base agent config (the runtime's main config) so it can build a one-off VL
/// provider from `config.vision_preprocessor_provider`. When the active (main)
/// model can't accept images, converts them to a text description and clears the
/// images; a vision-capable model passes through unchanged. Any failure to load
/// config / resolve / build degrades to sending the original `(text, images)`.
pub struct VlImagePreprocessor {
    factory: Arc<dyn CodingProviderFactory>,
    base: CodingAgentConfig,
}

impl VlImagePreprocessor {
    pub fn new(factory: Arc<dyn CodingProviderFactory>, base: CodingAgentConfig) -> Self {
        Self { factory, base }
    }
}

#[async_trait::async_trait]
impl ImagePreprocessor for VlImagePreprocessor {
    async fn preprocess(
        &self,
        text: String,
        images: Vec<ImageContent>,
        active_model: String,
        session_id: Option<String>,
    ) -> (UserInput, Option<VisionNotice>) {
        // Short-circuit: no images, or the main model already accepts images.
        if should_skip(&active_model, !images.is_empty()) {
            return (UserInput { text, images }, None);
        }
        let config = match Config::load(&Config::default_path()) {
            Ok(c) => c,
            Err(_) => return (UserInput { text, images }, None),
        };
        // Nothing configured (None or empty) ⇒ pass through unchanged (Skipped).
        let Some(vl_name) = config
            .vision_preprocessor_provider
            .clone()
            .filter(|s| !s.is_empty())
        else {
            return (UserInput { text, images }, None);
        };
        // Configured but absent from `config.providers` ⇒ Failed (mirror the
        // retired core `maybe_preprocess`): fold the failure marker + clear the
        // images so raw bytes never reach a text-only model.
        let Some(vl_pc) = config.providers.get(&vl_name).cloned() else {
            return apply_outcome(
                text,
                images,
                PreprocessOutcome::Failed {
                    reason: format!("VL provider '{vl_name}' not found in config"),
                },
            );
        };
        let vl_model = vl_model_display(&vl_pc.model).to_string();
        // Assemble the VL agent config from the base + the VL provider entry
        // (the same primitive subagent tier-routing uses), then build the
        // provider. `build` may block on auth I/O (token read + refresh over the
        // network); run it off the async owner task so a slow auth host can't
        // stall the runtime loop. `session_id` is bound at build so the one-off
        // VL call rides the same upstream account/replica as the main turn.
        let vl_cfg = derive_tier_config(&self.base, &vl_pc);
        let factory = self.factory.clone();
        let sid = session_id.filter(|s| !s.is_empty());
        let built =
            tokio::task::spawn_blocking(move || factory.build(&vl_cfg, sid.as_deref())).await;
        let provider = match built {
            Ok(Ok(p)) => p,
            _ => {
                return apply_outcome(
                    text,
                    images,
                    PreprocessOutcome::Failed {
                        reason: format!("VL provider '{vl_name}' build failed"),
                    },
                );
            }
        };
        let outcome = run_vl_caption(provider, vl_model, &text, &images).await;
        apply_outcome(text, images, outcome)
    }
}

/// Map a `run_vl_caption` outcome to `(UserInput, notice)`. Pure (no I/O) so
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
