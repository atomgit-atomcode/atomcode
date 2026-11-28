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
use axum::response::{IntoResponse, Json, Response};
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
    // Cheap and idempotent: the sweep is what keeps a recording that was never
    // transcribed from living forever.
    sweep_stale_recordings();
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
/// The path is nevertheless **constrained**: it has to resolve inside the
/// product tree's `voice/` directory. Without that, the route would read any
/// file the daemon can see and hand it to a model provider — and a `RIFF` header
/// is not a permission, it is four bytes anybody can write. The constraint is
/// the same shape as the workspace route's (`/workbench/preview`): canonicalize
/// both sides, then require containment.
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

    // The recording has served its purpose once it has been transcribed, so it
    // goes either way. Keeping raw microphone audio around after the transcript
    // exists is a privacy liability with no upside: the transcript is what the
    // person wanted, and the WAV is a second copy of their voice.
    let response = transcribe_audio(&path, &audio).await;
    let _ = std::fs::remove_file(&path);
    response
}

/// Read the audio once, then hand it to whichever engine is available.
async fn transcribe_audio(path: &std::path::Path, audio: &[u8]) -> Response {
    if let Some(package) = crate::packages::transcription_package() {
        return match run_plugin(&package, path).await {
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
    match call_model(&config, &source, audio).await {
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
/// Three things are checked here rather than in the app: where the file is, how
/// big it is, and whether it really is a WAV. The route is token-protected but
/// still reachable from the page, and forwarding an arbitrary local file to a
/// model provider is not something a typo — or a compromised page — should be
/// able to do.
///
/// The location check is the one that matters. A `RIFF`/`WAVE` header is four
/// bytes anybody can write, so a header check alone still lets any file on the
/// machine be uploaded. The path must resolve **inside the product tree's
/// `voice/` directory** — the same shape as the workspace route's containment
/// check: canonicalize both sides, then require the recording to be under the
/// root.
fn read_recording(path: &std::path::Path) -> Result<Vec<u8>, String> {
    read_recording_in(&recordings_dir(), path)
}

/// The same check against an explicit root, so it can be tested without a
/// product directory.
fn read_recording_in(root: &std::path::Path, path: &std::path::Path) -> Result<Vec<u8>, String> {
    // The root is the daemon's own tree; creating it is safe and keeps the
    // "no recordings yet" case from looking like a broken install.
    let _ = std::fs::create_dir_all(root);
    let root = std::fs::canonicalize(root).map_err(|error| {
        format!(
            "the recording directory {} is not available: {error}",
            root.display()
        )
    })?;
    let resolved = std::fs::canonicalize(path)
        .map_err(|error| format!("cannot read the recording {}: {error}", path.display()))?;
    if !resolved.starts_with(&root) {
        return Err(format!("the recording is outside {}", root.display()));
    }
    // Check and read **the same handle**. Deciding on a path and then opening that
    // path again leaves a window: whatever can write inside `voice/` can swap the
    // file — or its final component — for a symlink in between, and the read
    // follows it out of the root, into bytes that are then sent to the provider.
    let mut file = open_without_following(&resolved)
        .map_err(|error| format!("cannot read the recording {}: {error}", resolved.display()))?;
    // `O_NOFOLLOW` guards only the last component. A directory swapped on the way
    // is invisible to it, so on Linux ask the kernel what this handle really is
    // and re-check that — the answer comes from the handle, not from a second
    // path resolution.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some(opened) = opened_path(&file) {
        if !opened.starts_with(&root) {
            return Err(format!("the recording is outside {}", root.display()));
        }
    }
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot read the recording {}: {error}", resolved.display()))?;
    if !metadata.is_file() {
        return Err(format!("not a file: {}", resolved.display()));
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
    // Bound the read as well as the size check: the same inode can grow between
    // the two (another writer appending), and `read_to_end` would then run past
    // the limit it just passed.
    let mut audio = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(&mut file, MAX_RECORDING_BYTES + 1),
        &mut audio,
    )
    .map_err(|error| format!("cannot read the recording {}: {error}", resolved.display()))?;
    if audio.len() as u64 > MAX_RECORDING_BYTES {
        return Err(format!(
            "the recording grew past the {MAX_RECORDING_BYTES} byte limit while being read"
        ));
    }
    let is_wav = audio.len() >= 12 && &audio[0..4] == b"RIFF" && &audio[8..12] == b"WAVE";
    if !is_wav {
        return Err("the recording is not a WAV file".to_string());
    }
    Ok(audio)
}

/// Open a file without following a symlink in its final component.
///
/// The point is that the caller reads the handle it checked: following the link
/// at open time would let a swap between the check and the read decide what the
/// bytes are.
#[cfg(unix)]
fn open_without_following(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// Windows has no `O_NOFOLLOW`; the equivalent is opening the reparse point
/// instead of the target it points at.
#[cfg(windows)]
fn open_without_following(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_without_following(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new().read(true).open(path)
}

/// What the kernel actually opened, asked through the handle.
///
/// `None` where that question has no answer; the caller then keeps the weaker
/// guarantee (final component not a symlink) rather than failing the read.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn opened_path(file: &std::fs::File) -> Option<std::path::PathBuf> {
    use std::os::unix::io::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()
}

/// Where the app parks its recordings: `<product>/voice`.
///
/// The product tree is what the daemon already resolves (it is the config dir
/// on every platform), so the app and the daemon agree on it without a second
/// channel — the app writes under `filesDir/atomcode/voice`, and the daemon
/// reads under `ATOMCODE_HOME/voice`.
fn recordings_dir() -> PathBuf {
    Config::config_dir().join(RECORDINGS_DIR)
}

/// The directory the app records into, relative to the product tree.
const RECORDINGS_DIR: &str = "voice";

/// How long a recording may live before it is swept away.
///
/// The normal path deletes the take as soon as it is transcribed. This is the
/// backstop for the paths that do not get that far — the app was killed
/// mid-recording, or the page never sent it. Raw microphone audio should not
/// outlive its purpose by more than a day.
const RECORDING_MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// Delete recordings that are older than [`RECORDING_MAX_AGE_SECS`].
///
/// Best effort: a file that cannot be removed is left alone rather than turned
/// into an error, because a stale recording is a hygiene problem, not a failure
/// of the request that triggered the sweep.
fn sweep_stale_recordings() {
    sweep_stale_recordings_in(&recordings_dir());
}

/// The same sweep against an explicit directory, so it can be tested.
fn sweep_stale_recordings_in(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            // A file stamped in the future (clock moved) is not stale.
            continue;
        };
        if age.as_secs() > RECORDING_MAX_AGE_SECS {
            let _ = std::fs::remove_file(&path);
        }
    }
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
        let root = dir.path().join("voice");

        let good = root.join("good.wav");
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&64u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&[0u8; 64]);
        std::fs::create_dir_all(&root).expect("dir");
        std::fs::write(&good, &wav).expect("write");
        assert!(read_recording_in(&root, &good).is_ok());

        let text = root.join("notes.txt");
        std::fs::write(
            &text,
            "not audio at all, just some text longer than a header",
        )
        .expect("write");
        let refused = read_recording_in(&root, &text).expect_err("text must be refused");
        assert!(refused.contains("not a WAV"), "{refused}");

        let empty = root.join("empty.wav");
        std::fs::write(&empty, b"").expect("write");
        let refused = read_recording_in(&root, &empty).expect_err("an empty file must be refused");
        assert!(refused.contains("empty"), "{refused}");

        // Shorter than a WAV header but not empty: say which one it is, because
        // "empty" would send the caller looking for a take that never happened.
        let stub = root.join("stub.wav");
        std::fs::write(&stub, b"RIFF").expect("write");
        let refused = read_recording_in(&root, &stub).expect_err("a stub must be refused");
        assert!(refused.contains("too short"), "{refused}");
    }

    /// A file that merely *looks* like a WAV is still refused when it is not a
    /// recording.
    ///
    /// This is the hole the header check alone leaves open: `RIFF….WAVE` is four
    /// bytes anybody can write, so without the location check the route would
    /// read any file on the machine — the user's SSH key, a database dump — and
    /// upload it to a model provider. Both the file outside the directory and the
    /// traversal out of it have to be refused.
    #[test]
    fn a_recording_outside_the_voice_directory_is_refused() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("voice");
        std::fs::create_dir_all(&root).expect("dir");

        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&64u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&[0u8; 64]);

        // A perfectly valid WAV — just not in the recordings directory.
        let outside = dir.path().join("secrets.wav");
        std::fs::write(&outside, &wav).expect("write");
        let refused = read_recording_in(&root, &outside)
            .expect_err("a file outside the root must be refused");
        assert!(refused.contains("outside"), "{refused}");

        // And a traversal that points back into the directory by a longer path.
        let traversal = root.join("..").join("secrets.wav");
        let refused =
            read_recording_in(&root, &traversal).expect_err("a traversal must be refused");
        assert!(refused.contains("outside"), "{refused}");
    }

    /// The check and the read have to be about the same file.
    ///
    /// Containment is decided on a canonical path, but the read then has to open
    /// *that* path again — and in between, something able to write inside the
    /// recordings directory can put a symlink there. Refusing to follow the link
    /// at open time is what closes that window, so the open itself is the unit
    /// under test here.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_recording_is_not_followed_when_opened() {
        let dir = tempfile::tempdir().expect("temp dir");
        let real = dir.path().join("real.wav");
        std::fs::write(&real, b"RIFF").expect("write");
        let link = dir.path().join("link.wav");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");

        let error = open_without_following(&link).expect_err("opening must not follow the link");
        // Linux says ELOOP, other unices say something else; the outcome is the
        // contract, not the errno.
        assert!(!error.to_string().is_empty());

        // The real file still opens — the guard is about links, not files.
        let mut file = open_without_following(&real).expect("the real file opens");
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut body).expect("read");
        assert_eq!(body, b"RIFF");
    }

    /// A stale recording is swept away; a fresh one is left alone.
    ///
    /// The normal path deletes a take as soon as it is transcribed. This is the
    /// backstop for the ones that never got that far — the app was killed
    /// mid-recording — so raw microphone audio does not outlive its purpose.
    #[test]
    fn a_stale_recording_is_swept_and_a_fresh_one_is_kept() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("voice");
        std::fs::create_dir_all(&root).expect("dir");

        let old = root.join("old.wav");
        let fresh = root.join("fresh.wav");
        std::fs::write(&old, b"RIFF").expect("write");
        std::fs::write(&fresh, b"RIFF").expect("write");

        // Age the first one past the limit.
        let long_ago = std::time::SystemTime::now()
            - std::time::Duration::from_secs(RECORDING_MAX_AGE_SECS + 3600);
        set_file_age(&old, long_ago);

        sweep_stale_recordings_in(&root);
        assert!(!old.exists(), "the stale recording must be swept");
        assert!(fresh.exists(), "a fresh recording must be kept");
    }

    /// Backdate a file, so the age-based sweep can be exercised without waiting.
    fn set_file_age(path: &std::path::Path, when: std::time::SystemTime) {
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open");
        file.set_modified(when).expect("set mtime");
    }
}
