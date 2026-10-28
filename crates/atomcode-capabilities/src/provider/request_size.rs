//! Bound the actual wire JSON for the gateway with a known byte limit.
use atomcode_kernel::stream::ProviderError;
use serde_json::Value;

// Use decimal MB conservatively: do not assume the gateway means MiB.
const ATOMGIT_MAX_BYTES: usize = 20_000_000;

pub(super) fn serialize(url: &str, body: &Value) -> Result<Vec<u8>, ProviderError> {
    let bytes = serde_json::to_vec(body).map_err(|error| ProviderError {
        message: format!("request body serialization failed: {error}"),
        ..Default::default()
    })?;
    let gateway = reqwest::Url::parse(url)
        .ok()
        .is_some_and(|url| url.host_str() == Some("llm-api.atomgit.com"));
    if gateway && bytes.len() > ATOMGIT_MAX_BYTES {
        return Err(ProviderError {
            message: format!(
                "请求体超过网关 20MB 限制：实际 {} 字节，上限 {} 字节，已在发送前拦截。请用 /compact 压缩历史、减少附件，或让工具分段读取输出；原样重试不会解决此问题。",
                bytes.len(), ATOMGIT_MAX_BYTES
            ),
            code: Some("request_body_too_large".into()),
            // Preserve the existing image-budget recovery seam, without ordinary retries.
            http_status: Some(413),
            retryable: false,
            ..Default::default()
        });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_bytes_include_unicode_and_json_escaping() {
        let body = json!({"text": "中文\n\"".repeat(3_000_000)});
        let error =
            serialize("https://llm-api.atomgit.com/v1/chat/completions", &body).unwrap_err();
        assert_eq!(error.http_status, Some(413));
        assert!(!error.retryable);
        assert!(!error.is_context_overflow());
        assert!(error
            .message
            .contains(&serde_json::to_vec(&body).unwrap().len().to_string()));
    }

    #[test]
    fn boundary_and_other_providers() {
        // JSON adds two quote bytes around this string.
        let body = Value::String("x".repeat(ATOMGIT_MAX_BYTES - 2));
        assert_eq!(
            serialize("https://llm-api.atomgit.com/v1/responses", &body)
                .unwrap()
                .len(),
            ATOMGIT_MAX_BYTES
        );
        let body = Value::String("x".repeat(ATOMGIT_MAX_BYTES));
        for path in ["chat/completions", "responses", "messages"] {
            assert!(serialize(&format!("https://llm-api.atomgit.com/v1/{path}"), &body).is_err());
        }
        for url in [
            "https://other.example/v1",
            "https://llm-api.atomgit.com.evil.example/v1",
        ] {
            assert!(serialize(url, &body).is_ok());
        }
    }
}
