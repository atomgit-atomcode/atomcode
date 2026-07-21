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

use atomcode_coding::{ImageContent, ImagePreprocessor, UserInput};
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
    ) -> UserInput {
        if images.is_empty() {
            return UserInput { text, images };
        }
        let config = match Config::load(&Config::default_path()) {
            Ok(c) => c,
            Err(_) => return UserInput { text, images },
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
            None => return UserInput { text, images },
        };
        // `create_provider` can do blocking auth I/O (token read + refresh over
        // the network); run it off the async owner task so a slow auth host
        // can't stall the runtime loop — same treatment `maybe_preprocess`
        // gives the VL provider build.
        let active = match tokio::task::spawn_blocking(move || create_provider(&pc)).await {
            Ok(Ok(p)) => p,
            _ => return UserInput { text, images },
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
        match maybe_preprocess(&config, active.as_ref(), &text, &parts).await {
            // Vision-capable model — keep the images, send them natively.
            PreprocessOutcome::Skipped => UserInput { text, images },
            PreprocessOutcome::Replaced { text: vl, vl_key } => {
                let merged = if text.trim().is_empty() {
                    format!("[图片内容（由 {vl_key} 识别）]\n{vl}")
                } else {
                    format!("{text}\n\n[图片内容（由 {vl_key} 识别）]\n{vl}")
                };
                UserInput {
                    text: merged,
                    images: Vec::new(),
                }
            }
            PreprocessOutcome::Failed { .. } => {
                let merged = if text.trim().is_empty() {
                    "[图片识别失败]".to_string()
                } else {
                    format!("{text}\n\n[图片识别失败]")
                };
                UserInput {
                    text: merged,
                    images: Vec::new(),
                }
            }
        }
    }
}
