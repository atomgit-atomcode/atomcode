//! Explicit protocol tracing, bounded and redacted. Never writes to server stdio.
use super::super::lsp_policy::LspTrace;
use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct Trace(Option<Arc<Mutex<Sink>>>);
struct Sink {
    file: std::fs::File,
    mode: LspTrace,
    secrets: Vec<String>,
}
impl Trace {
    pub fn open(path: Option<&Path>, mode: LspTrace, secrets: Vec<String>) -> Result<Self, String> {
        if mode == LspTrace::Off {
            return Ok(Self::default());
        }
        let path = path.ok_or("LSP trace requires a host-provided log path")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("LSP trace directory: {e}"))?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(path)
            .map_err(|e| format!("LSP trace log: {e}"))?;
        Ok(Self(Some(Arc::new(Mutex::new(Sink {
            file,
            mode,
            secrets,
        })))))
    }
    pub fn record(&self, direction: &str, value: &Value) {
        let Some(sink) = &self.0 else {
            return;
        };
        let mut sink = sink.lock().unwrap();
        // Other clients and runs may append through a different handle. Consult
        // the real file length rather than a per-client byte counter.
        let Ok(metadata) = sink.file.metadata() else {
            return;
        };
        let bytes = metadata.len();
        if bytes >= 8 * 1024 * 1024 {
            return;
        }
        let mut value = value.clone();
        if sink.mode == LspTrace::Messages
            && value.get("method").and_then(Value::as_str) != Some("initialize")
        {
            if let Some(object) = value.as_object_mut() {
                object.remove("params");
                // Keep server capabilities but omit source-bearing query responses.
                if object
                    .get("result")
                    .is_some_and(|v| v.get("capabilities").is_none())
                {
                    object.insert(
                        "result".into(),
                        Value::String("[omitted: messages trace]".into()),
                    );
                }
            }
        }
        redact(&mut value, &sink.secrets);
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        if let Ok(line) = serde_json::to_vec(
            &serde_json::json!({"at_ms":at,"direction":direction,"message":value}),
        ) {
            if line.len() > 256 * 1024 || bytes + line.len() as u64 + 1 > 8 * 1024 * 1024 {
                return;
            }
            let _ = sink
                .file
                .write_all(&line)
                .and_then(|_| sink.file.write_all(b"\n"));
        }
    }
}
pub(super) fn credential_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "api_key",
        "api-key",
        "apikey",
        "authorization",
        "private_key",
        "privatekey",
        "credential",
    ]
    .iter()
    .any(|part| key.contains(part))
}

fn redact(value: &mut Value, secrets: &[String]) {
    match value {
        Value::Object(map) => {
            let original = std::mem::take(map);
            for (key, mut value) in original {
                if credential_key(&key) {
                    value = Value::String("[redacted]".into());
                } else {
                    redact(&mut value, secrets);
                }
                let mut key = key;
                for secret in secrets {
                    if !secret.is_empty() {
                        key = key.replace(secret, "[redacted]");
                    }
                }
                map.insert(key, value);
            }
        }
        Value::Array(array) => {
            for value in array {
                redact(value, secrets);
            }
        }
        Value::String(text) => {
            for secret in secrets {
                if !secret.is_empty() {
                    *text = text.replace(secret, "[redacted]");
                }
            }
        }
        _ => {}
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn log_limit_accounts_for_other_open_handles() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lsp.log");
        let first = Trace::open(Some(&path), LspTrace::Verbose, vec![]).unwrap();
        let second = Trace::open(Some(&path), LspTrace::Verbose, vec![]).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(8 * 1024 * 1024 - 1)
            .unwrap();
        first.record("send", &serde_json::json!({"method":"initialize"}));
        second.record("receive", &serde_json::json!({"result":"response"}));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 8 * 1024 * 1024 - 1);
    }

    #[test]
    fn credentials_and_expanded_environment_are_redacted() {
        let mut value = serde_json::json!({"apiKey":"private", "path":"prefix-sensitive-suffix", "nested":[{"token":"hidden"}]});
        redact(&mut value, &["sensitive".into()]);
        let text = value.to_string();
        assert!(!text.contains("private"));
        assert!(!text.contains("sensitive"));
        assert!(!text.contains("hidden"));
    }
}
