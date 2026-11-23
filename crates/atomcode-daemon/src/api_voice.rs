//! Speech input for a phone that has no recogniser.
//!
//! Android's own route is `ACTION_RECOGNIZE_SPEECH`: another app owns the
//! microphone and the agent receives only the transcript. That route needs a
//! recogniser to be installed, and a phone whose speech engine is private to its
//! keyboard has none — measured on PLT140 (OPLUS / Android 16):
//! `query-services -a android.speech.RecognitionService` answers "No services
//! found" while the installed Baidu IME dictates happily into any text field. The
//! mic button therefore said "this device has no speech recogniser" on a phone
//! where the person can plainly dictate.
//!
//! Two fallbacks exist, and [`capabilities`] is how the page learns which ones it
//! may offer:
//!
//! * **plugin** — an installed package that turns a WAV into text
//!   (`"transcribe": true` in its `package.json`). Runs through `pluginhost`, so
//!   the audio never leaves the phone.
//! * **model** — the configured provider's `/audio/transcriptions` endpoint.
//!
//! Neither replaces the system recogniser: that one needs no permission, no
//! package and no network round trip, so the page still tries it first.

use std::path::PathBuf;

use atomcode_config::config::Config;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use serde::Serialize;

use crate::json_error;

/// A provider that can transcribe, and the model to ask for.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ModelVoiceSource {
    provider: String,
    model: String,
}

/// GET /voice/capabilities
///
/// The host reports the system recogniser itself (it can `resolveActivity`); this
/// answers the two things only the daemon can see. Both `null` means voice input
/// has nothing to fall back to, and the page says so instead of offering a
/// button that fails.
pub(crate) async fn capabilities() -> impl IntoResponse {
    let config = crate::api_config::load_config().unwrap_or_default();
    Json(serde_json::json!({
        "success": true,
        "plugin": crate::packages::transcription_package(),
        "model": model_source(&config),
    }))
}

/// POST /voice/transcribe — a recording's path in, a transcript out.
///
/// The path rather than the bytes: the app records in its own process and the
/// daemon shares that filesystem, so a 2 MB upload over loopback would be copies
/// of a file both sides already have.
///
/// The engine is picked here rather than by the page, because only this side
/// knows what is installed and configured: a local package wins over a network
/// call, since it is free, private and cannot be rate-limited.
pub(crate) async fn transcribe(Json(request): Json<TranscribeRequest>) -> impl IntoResponse {
    let field = request.path.trim();
    if field.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "no recording path given").into_response();
    }
    let path = PathBuf::from(field);
    if !path.is_absolute() {
        return json_error(
            StatusCode::BAD_REQUEST,
            "the recording path must be absolute",
        )
        .into_response();
    }
    let audio = match read_recording(&path) {
        Ok(audio) => audio,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error).into_response(),
    };

    if let Some(package) = crate::packages::transcription_package() {
        return match run_plugin(&package, &path).await {
            Ok(text) => Json(serde_json::json!({
                "success": true,
                "text": text,
                "engine": "plugin",
                "package": package,
            }))
            .into_response(),
            Err(error) => json_error(
                StatusCode::BAD_GATEWAY,
                format!("the transcription package `{package}` failed: {error}"),
            )
            .into_response(),
        };
    }

    let config = crate::api_config::load_config().unwrap_or_default();
    let Some(source) = model_source(&config) else {
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no transcription engine: install a local transcription package from the plugin page, \
             or configure a model provider that offers /audio/transcriptions",
        )
        .into_response();
    };
    match call_model(&config, &source, &audio).await {
        Ok(text) => Json(serde_json::json!({
            "success": true,
            "text": text,
            "engine": "model",
            "provider": source.provider,
            "model": source.model,
        }))
        .into_response(),
        Err(error) => json_error(StatusCode::BAD_GATEWAY, error).into_response(),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct TranscribeRequest {
    /// Absolute path to a recording the app just made.
    path: String,
}

/// Read a recording, refusing anything that is not one.
///
/// Size and header are both checked here rather than in the app: the route is
/// token-protected but still reachable from the page, and forwarding an arbitrary
/// local file to a model provider is not something a typo should be able to do.
fn read_recording(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("cannot read the recording {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("not a file: {}", path.display()));
    }
    if metadata.len() < 44 {
        // A WAV cannot be shorter than its own header, but "empty" is the wrong
        // word for a 4-byte file: the caller gets the size so the two cases — a
        // take nobody spoke into, and a path that is not a recording — stay
        // distinguishable without trying again.
        return Err(if metadata.len() == 0 {
            "the recording is empty".to_string()
        } else {
            format!(
                "the recording is {} bytes, too short to be a WAV file",
                metadata.len()
            )
        });
    }
    if metadata.len() > MAX_RECORDING_BYTES {
        return Err(format!(
            "the recording is {} bytes; the limit is {MAX_RECORDING_BYTES}",
            metadata.len()
        ));
    }
    let audio = std::fs::read(path)
        .map_err(|error| format!("cannot read the recording {}: {error}", path.display()))?;
    let is_wav = audio.len() >= 12 && &audio[0..4] == b"RIFF" && &audio[8..12] == b"WAVE";
    if !is_wav {
        return Err("the recording is not a WAV file".to_string());
    }
    Ok(audio)
}

/// A few minutes of 16 kHz mono speech; beyond that the caller is confused.
const MAX_RECORDING_BYTES: u64 = 16 * 1024 * 1024;

/// Which provider would transcribe, if one is configured well enough to try.
///
/// `[voice] provider` wins, otherwise the active model's provider — a single
/// provider setup should not have to name itself twice. A provider without a base
/// URL or without a resolvable key is reported as absent rather than attempted: a
/// button that always fails is worse than one that explains.
fn model_source(config: &Config) -> Option<ModelVoiceSource> {
    let selection = config
        .voice
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| config.effective_model_selection())?;
    let provider = config.provider_config_for_selection(&selection)?;
    let base = provider.base_url.as_deref()?.trim();
    if base.is_empty() {
        return None;
    }
    provider.resolved_api_key()?;
    Some(ModelVoiceSource {
        provider: selection,
        model: config.voice.transcription_model().to_string(),
    })
}

/// Run the installed transcription package on `wav`.
///
/// The recording's own path, not a copy: the plugin runs in this app's process
/// tree and can read the same file. Staging into `std::env::temp_dir()` looked
/// tidier and would have failed on the device — on Android that is
/// `/data/local/tmp`, which the app cannot write.
async fn run_plugin(package: &str, wav: &std::path::Path) -> Result<String, String> {
    let package = package.to_string();
    let staged = wav.to_path_buf();
    tokio::task::spawn_blocking(move || crate::packages::transcribe_audio(&package, &staged))
        .await
        .map_err(|error| format!("the transcription task panicked: {error}"))?
}

/// POST the audio to the provider's transcription endpoint.
async fn call_model(
    config: &Config,
    source: &ModelVoiceSource,
    wav: &[u8],
) -> Result<String, String> {
    let provider = config
        .provider_config_for_selection(&source.provider)
        .ok_or_else(|| format!("provider `{}` is no longer configured", source.provider))?;
    let base = provider
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .ok_or_else(|| format!("provider `{}` has no base URL", source.provider))?;
    let key = provider
        .resolved_api_key()
        .ok_or_else(|| format!("provider `{}` has no usable credential", source.provider))?;

    // Built by hand rather than through reqwest's `multipart` feature: one audio
    // part and one model part do not justify a dependency change, and the Cargo
    // lock in this repository is watched closely.
    let boundary = "----atomcode-voice-boundary";
    let body = multipart(boundary, &source.model, wav);
    let url = format!("{}/audio/transcriptions", base.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|error| format!("cannot build the HTTP client: {error}"))?;
    let response = client
        .post(&url)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("authorization", format!("Bearer {key}"))
        .body(body)
        .send()
        .await
        .map_err(|error| format!("cannot reach {url}: {error}"))?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        // The provider's own words: "model not found" and "unsupported audio"
        // are different problems and only the endpoint knows which one it is.
        return Err(format!(
            "{url} answered {status}: {}",
            text.trim().chars().take(400).collect::<String>()
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| format!("the transcription endpoint returned unreadable JSON: {error}"))?;
    parsed
        .get("text")
        .and_then(|value| value.as_str())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "the transcription endpoint returned no text".to_string())
}

fn multipart(boundary: &str, model: &str, wav: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(wav.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
             filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n{model}\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config whose only provider is `openai`, with the given base URL and key.
    fn config_with(base_url: &str, api_key: &str) -> Config {
        serde_json::from_value(serde_json::json!({
            "default_model": "openai",
            "providers": {
                "openai": {
                    "type": "openai",
                    "model": "gpt-4o",
                    "base_url": base_url,
                    "api_key": api_key,
                }
            }
        }))
        .expect("a minimal config must deserialize")
    }

    /// A provider that names a base URL but no credential cannot transcribe, and
    /// reporting it as available would offer a button that always fails.
    #[test]
    fn a_provider_without_a_credential_is_not_offered() {
        let config = config_with("https://api.example/v1", "");
        assert_eq!(model_source(&config), None);
    }

    #[test]
    fn a_provider_with_a_credential_is_offered_with_the_default_model() {
        let config = config_with("https://api.example/v1", "sk-x");
        assert_eq!(
            model_source(&config),
            Some(ModelVoiceSource {
                provider: "openai".to_string(),
                model: "whisper-1".to_string(),
            })
        );
    }

    /// `[voice] model` is what the endpoint is asked for; the provider stays the
    /// active selection.
    #[test]
    fn the_configured_transcription_model_wins_over_the_default() {
        let mut config = config_with("https://api.example/v1", "sk-x");
        config.voice.model = Some("gpt-4o-transcribe".to_string());
        assert_eq!(
            model_source(&config).map(|source| source.model),
            Some("gpt-4o-transcribe".to_string())
        );
    }

    /// A provider with no base URL is not attempted: there is nowhere to send the
    /// audio, and guessing an endpoint is how a key ends up at the wrong host.
    #[test]
    fn a_provider_without_a_base_url_is_not_offered() {
        let config = config_with("", "sk-x");
        assert_eq!(model_source(&config), None);
    }

    /// The multipart body carries both fields the endpoint needs, and the audio
    /// verbatim.
    #[test]
    fn the_multipart_body_carries_the_model_and_the_audio() {
        let audio = b"RIFFxxxxWAVE";
        let body = multipart("BOUND", "whisper-1", audio);
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"file\"; filename=\"speech.wav\""));
        assert!(text.contains("Content-Type: audio/wav"));
        assert!(text.contains("name=\"model\""));
        assert!(text.contains("whisper-1"));
        assert!(text.contains("--BOUND--"));
        assert!(body.windows(audio.len()).any(|window| window == audio));
    }

    /// A WAV the app wrote is accepted; anything else is refused before it can be
    /// forwarded, because "read whatever path the page names and send it to a
    /// model provider" is not a thing a typo should be able to do.
    #[test]
    fn only_a_real_wav_recording_is_accepted() {
        let dir = tempfile::tempdir().expect("temp dir");

        let good = dir.path().join("good.wav");
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&64u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&[0u8; 64]);
        std::fs::write(&good, &wav).expect("write");
        assert!(read_recording(&good).is_ok());

        let text = dir.path().join("notes.txt");
        std::fs::write(
            &text,
            "not audio at all, just some text longer than a header",
        )
        .expect("write");
        let refused = read_recording(&text).expect_err("text must be refused");
        assert!(refused.contains("not a WAV"), "{refused}");

        let empty = dir.path().join("empty.wav");
        std::fs::write(&empty, b"").expect("write");
        let refused = read_recording(&empty).expect_err("an empty file must be refused");
        assert!(refused.contains("empty"), "{refused}");

        // Shorter than a WAV header but not empty: say which one it is, because
        // "empty" would send the caller looking for a take that never happened.
        let stub = dir.path().join("stub.wav");
        std::fs::write(&stub, b"RIFF").expect("write");
        let refused = read_recording(&stub).expect_err("a stub must be refused");
        assert!(refused.contains("too short"), "{refused}");

        let missing = dir.path().join("missing.wav");
        assert!(read_recording(&missing).is_err());
    }
}
