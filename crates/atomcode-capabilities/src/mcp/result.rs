//! Convert untrusted MCP content into the existing neutral tool result.
//!
//! Screenshots keep their original bytes and dimensions: resizing them here
//! would change the coordinate space without informing the execution backend.
use std::io::Cursor;

use atomcode_kernel::{message::ImageContent, tool::ToolResult};
use base64::Engine;

use super::types::{CallToolResult, ContentBlock};

const MAX_IMAGE_BASE64_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESULT_BASE64_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGES: usize = 4;
const MAX_PIXELS: u64 = 16_000_000;
const MAX_EDGE: u32 = 8192;

/// Runs on a blocking worker when the result contains images. A validation
/// failure retains text (the operation already ran), but never attaches a
/// partial or misleading set of screenshots, or prints their payloads.
pub(super) fn into_tool_result(result: CallToolResult) -> ToolResult {
    let mut text = Vec::new();
    let mut images = Vec::new();
    let mut descriptions = Vec::new();
    let mut failure = None;
    let mut encoded_bytes = 0usize;
    let mut count = 0usize;
    for block in result.content {
        match block {
            ContentBlock::Text { text: body } => text.push(body),
            ContentBlock::Image { data, mime_type } if failure.is_none() => {
                count += 1;
                encoded_bytes = encoded_bytes.saturating_add(data.len());
                let checked = if count > MAX_IMAGES {
                    Err("too many images (maximum 4)")
                } else if encoded_bytes > MAX_RESULT_BASE64_BYTES {
                    Err("combined image data exceeds 8 MiB of base64")
                } else {
                    validate_image(&mime_type, &data)
                };
                match checked {
                    Ok((media_type, width, height)) => {
                        descriptions.push(format!(
                            "MCP image {count}: {width}x{height} pixels ({media_type}); original resolution preserved."
                        ));
                        images.push(ImageContent {
                            media_type: media_type.into(),
                            data,
                        });
                    }
                    Err(reason) => failure = Some(reason),
                }
            }
            // Resource URLs are not fetched. This adapter supports inline
            // image blocks; other content retains its previous behavior.
            _ => {}
        }
    }
    let is_error = result.is_error || failure.is_some();
    if let Some(reason) = failure {
        images.clear();
        text.push(format!(
            "MCP tool executed, but image output rejected: {reason}."
        ));
    } else {
        // Some desktop servers embed the same screenshot in JSON metadata and
        // an image block. Keep the validated attachment, not a second base64
        // copy that can consume the text budget and truncate fresh target IDs.
        for body in &mut text {
            remove_duplicate_image_urls(body, &images);
        }
        text.extend(descriptions);
    }
    let mut content = text.join("\n");
    if result.is_error {
        content = format!("MCP tool error: {content}");
    }
    ToolResult {
        call_id: String::new(),
        content,
        is_error,
        images,
    }
}

fn remove_duplicate_image_urls(text: &mut String, images: &[ImageContent]) {
    if images.is_empty() || !text.contains("data:") {
        return;
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let urls: Vec<_> = images
        .iter()
        .map(|image| format!("data:{};base64,{}", image.media_type, image.data))
        .collect();
    fn visit(value: &mut serde_json::Value, urls: &[String]) -> bool {
        match value {
            serde_json::Value::String(value) => {
                if let Some(index) = urls.iter().position(|url| url == value) {
                    *value = format!("[MCP inline image {}; attached separately]", index + 1);
                    true
                } else {
                    false
                }
            }
            serde_json::Value::Array(values) => values
                .iter_mut()
                .fold(false, |changed, value| visit(value, urls) || changed),
            serde_json::Value::Object(values) => values
                .values_mut()
                .fold(false, |changed, value| visit(value, urls) || changed),
            _ => false,
        }
    }
    if visit(&mut value, &urls) {
        *text = serde_json::to_string(&value).expect("JSON value serializes");
    }
}

fn validate_image(mime: &str, data: &str) -> Result<(&'static str, u32, u32), &'static str> {
    if data.len() > MAX_IMAGE_BASE64_BYTES {
        return Err("image exceeds 4 MiB of base64");
    }
    let (expected, media_type) = match mime.to_ascii_lowercase().as_str() {
        "image/png" => (image::ImageFormat::Png, "image/png"),
        "image/jpeg" => (image::ImageFormat::Jpeg, "image/jpeg"),
        "image/gif" => (image::ImageFormat::Gif, "image/gif"),
        "image/webp" => (image::ImageFormat::WebP, "image/webp"),
        _ => return Err("unsupported image MIME type (use PNG, JPEG, GIF or WebP)"),
    };
    let raw = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| "invalid base64 image data")?;
    let actual = image::guess_format(&raw).map_err(|_| "unrecognized image encoding")?;
    if actual != expected {
        return Err("image MIME type does not match its encoding");
    }
    // Header parsing can itself decompress metadata, so bound allocation on
    // this probe as well as on the full pixel decode below.
    let mut header = image::ImageReader::with_format(Cursor::new(&raw), actual);
    let mut header_limits = image::Limits::default();
    header_limits.max_alloc = Some(80 * 1024 * 1024);
    header.limits(header_limits);
    let (width, height) = header
        .into_dimensions()
        .map_err(|_| "invalid image header")?;
    if width == 0
        || height == 0
        || width > MAX_EDGE
        || height > MAX_EDGE
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err("image dimensions exceed 8192 pixels per edge or 16 million pixels");
    }
    // Dimensions alone do not establish that the pixel data is valid. Bound
    // decoder allocation too, and reject corrupt/truncated images before they
    // can poison every subsequent model request.
    let mut reader = image::ImageReader::with_format(Cursor::new(&raw), actual);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(80 * 1024 * 1024);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|_| "invalid or excessive image pixel data")?;
    Ok((media_type, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAIAAAB7QOjdAAAADUlEQVR4nGP4zwAE/wEHAAH/4iOeWQAAAABJRU5ErkJggg==";

    fn picture(mime: &str, data: &str) -> ContentBlock {
        ContentBlock::Image {
            mime_type: mime.into(),
            data: data.into(),
        }
    }
    fn output(content: Vec<ContentBlock>) -> ToolResult {
        into_tool_result(CallToolResult {
            content,
            is_error: false,
        })
    }
    #[test]
    fn text_results_keep_their_existing_contents() {
        let result = output(vec![
            ContentBlock::Text { text: "one".into() },
            ContentBlock::Text { text: "two".into() },
        ]);
        assert_eq!(result.content, "one\ntwo");
        assert!(!result.is_error);
        assert!(result.images.is_empty());
    }
    #[test]
    fn an_image_only_result_keeps_original_pixels_and_names_the_dimensions() {
        let result = output(vec![picture("image/png", PNG)]);
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.images.len(), 1);
        assert_eq!(result.images[0].data, PNG);
        assert_eq!(result.images[0].media_type, "image/png");
        assert!(result.content.contains("2x1"));
    }
    #[test]
    fn duplicate_json_screenshot_urls_keep_metadata_without_second_payload() {
        let body = serde_json::json!({
            "screenshots": [{"id":"fresh-shot", "url":format!("data:image/png;base64,{PNG}")}],
            "window":{"id":42},
            "other":"data:image/png;base64,unrelated",
            "tree":"2 text field\n3 button"
        })
        .to_string();
        let result = output(vec![
            ContentBlock::Text { text: body },
            picture("image/png", PNG),
        ]);
        assert!(!result.is_error);
        assert_eq!(result.images[0].data, PNG);
        assert!(!result.content.contains(PNG));
        let metadata: serde_json::Value = serde_json::Deserializer::from_str(&result.content)
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(metadata["screenshots"][0]["id"], "fresh-shot");
        assert_eq!(metadata["window"]["id"], 42);
        assert_eq!(metadata["tree"], "2 text field\n3 button");
        assert_eq!(metadata["other"], "data:image/png;base64,unrelated");
        assert_eq!(
            metadata["screenshots"][0]["url"],
            "[MCP inline image 1; attached separately]"
        );
        // No attachment means no substitution, even for a syntactically valid URL.
        let text = format!("data:image/png;base64,{PNG}");
        assert_eq!(
            output(vec![ContentBlock::Text { text: text.clone() }]).content,
            text
        );
    }

    #[test]
    fn errors_retain_their_text_and_valid_screenshot() {
        let result = into_tool_result(CallToolResult {
            content: vec![
                ContentBlock::Text {
                    text: "button was not found".into(),
                },
                picture("image/png", PNG),
            ],
            is_error: true,
        });
        assert!(result.is_error);
        assert!(result
            .content
            .starts_with("MCP tool error: button was not found"));
        assert_eq!(result.images.len(), 1);
    }
    #[test]
    fn invalid_images_fail_without_leaking_payloads_or_a_partial_screenshot_set() {
        for (mime, data) in [
            ("image/png", "not-base64!"),
            ("image/png", ""),
            ("image/jpeg", PNG),
            ("image/svg+xml", PNG),
            ("image/png", "aGVsbG8="),
        ] {
            let result = output(vec![
                ContentBlock::Text {
                    text: "the operation completed".into(),
                },
                picture("image/png", PNG),
                picture(mime, data),
            ]);
            assert!(result.is_error, "mime={mime}");
            assert!(result.images.is_empty());
            assert!(result.content.contains("the operation completed"));
            assert!(result.content.contains("image output rejected"));
            assert!(!result.content.contains(PNG));
            assert!(!result.content.contains("original resolution preserved"));
        }
    }
    #[test]
    fn too_many_images_and_oversized_base64_are_rejected() {
        let result = output(
            (0..=MAX_IMAGES)
                .map(|_| picture("image/png", PNG))
                .collect(),
        );
        assert!(result.is_error);
        assert!(result.images.is_empty());
        let result = output(vec![picture(
            "image/png",
            &"A".repeat(MAX_IMAGE_BASE64_BYTES + 1),
        )]);
        assert!(result.is_error);
        assert!(result.content.contains("exceeds 4 MiB"));
    }
    #[test]
    fn a_truncated_image_with_valid_dimensions_is_rejected() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes[..33]);
        let result = output(vec![picture("image/png", &data)]);
        assert!(result.is_error);
        assert!(result.images.is_empty());
    }
    #[test]
    fn supported_formats_are_validated_without_reencoding() {
        for (format, mime) in [
            (image::ImageFormat::Png, "image/png"),
            (image::ImageFormat::Jpeg, "image/jpeg"),
            (image::ImageFormat::Gif, "image/gif"),
            (image::ImageFormat::WebP, "image/webp"),
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 1)
                .write_to(&mut bytes, format)
                .unwrap();
            let data = base64::engine::general_purpose::STANDARD.encode(bytes.into_inner());
            let result = output(vec![picture(mime, &data)]);
            assert!(!result.is_error, "{mime}: {}", result.content);
            assert_eq!(result.images[0].data, data);
        }
    }

    #[test]
    fn combined_image_budget_is_enforced_before_another_decode() {
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(PNG)
            .unwrap();
        // Padding after IEND keeps a valid tiny PNG while exercising the wire
        // budget without allocating a large decoded bitmap.
        bytes.resize(2_200_000, 0);
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert!(data.len() < MAX_IMAGE_BASE64_BYTES);
        let result = output((0..3).map(|_| picture("image/png", &data)).collect());
        assert!(result.is_error);
        assert!(result.images.is_empty());
        assert!(
            result.content.contains("combined image data"),
            "{}",
            result.content
        );
    }

    #[test]
    fn excessive_dimensions_are_rejected_from_the_header() {
        for (width, height) in [(MAX_EDGE + 1, 1u32), (4097, 4097)] {
            let mut bytes = base64::engine::general_purpose::STANDARD
                .decode(PNG)
                .unwrap();
            bytes[16..20].copy_from_slice(&width.to_be_bytes());
            bytes[20..24].copy_from_slice(&height.to_be_bytes());
            // Recompute the IHDR CRC so the dimensions, rather than a broken
            // checksum, are what the validator rejects.
            let mut crc = !0u32;
            for byte in &bytes[12..29] {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1));
                }
            }
            bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
            let data = base64::engine::general_purpose::STANDARD.encode(bytes);
            let result = output(vec![picture("image/png", &data)]);
            assert!(result.is_error);
            assert!(result.images.is_empty());
            assert!(
                result.content.contains("image dimensions exceed"),
                "{}",
                result.content
            );
        }
    }
}
