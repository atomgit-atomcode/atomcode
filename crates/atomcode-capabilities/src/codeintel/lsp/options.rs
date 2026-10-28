//! Resolve declarations before spawning. Values never enter printable row configuration.
use super::super::{lsp_policy::LspMode, LspSettings};
use super::registry::LspServerConfig;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

const MAX_OPTIONS_BYTES: u64 = 1024 * 1024;

#[derive(Default)]
pub struct PreparedOptions {
    pub initialization: Option<Value>,
    pub settings: Value,
    pub source: String,
    pub redactions: Vec<String>,
}

pub async fn read_json(path: &Path) -> Result<Value, String> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("LSP configuration {}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_OPTIONS_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("LSP configuration read failed: {e}"))?;
    if bytes.len() as u64 > MAX_OPTIONS_BYTES {
        return Err("LSP configuration exceeds 1 MiB".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "LSP configuration {}: invalid JSON at line {}, column {}",
            path.display(),
            e.line(),
            e.column()
        )
    })
}

fn file_path(path: &Path, policy: &LspSettings) -> Result<PathBuf, String> {
    if let Some(path) = crate::pathutil::expand_tilde(&path.to_string_lossy()) {
        return Ok(path);
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    policy
        .config_dir
        .as_ref()
        .map(|base| base.join(path))
        .ok_or_else(|| "relative LSP configuration path requires a host config directory".into())
}

fn uri(path: &Path) -> Result<String, String> {
    url::Url::from_file_path(path)
        .map(|u| u.to_string())
        .map_err(|_| "LSP template requires an absolute directory".into())
}

fn expand(
    text: &str,
    root: &Path,
    file: &Path,
    redactions: &mut Vec<String>,
) -> Result<String, String> {
    let mut rest = text;
    let mut result = String::new();
    while let Some(start) = rest.find("${") {
        result.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        let end = rest.find('}').ok_or("unterminated LSP template variable")?;
        let name = &rest[..end];
        let value = match name {
            "workspaceFolder" => root.to_string_lossy().into_owned(),
            "workspaceUri" => uri(root)?,
            "fileDir" => file.parent().unwrap_or(root).to_string_lossy().into_owned(),
            "fileDirUri" => uri(file.parent().unwrap_or(root))?,
            "HOME" => {
                let home = crate::pathutil::expand_tilde("~/")
                    .ok_or("cannot resolve HOME for LSP template")?;
                home.to_string_lossy().into_owned()
            }
            name if name.starts_with("env:") => {
                let key = &name[4..];
                let value = std::env::var(key).map_err(|_| {
                    format!("LSP template environment variable {key} is not defined")
                })?;
                if !value.is_empty() {
                    redactions.push(value.clone());
                }
                value
            }
            _ => return Err(format!("unknown LSP template variable: {name}")),
        };
        result.push_str(&value);
        rest = &rest[end + 1..];
    }
    result.push_str(rest);
    Ok(result)
}
fn expand_value(
    value: Value,
    root: &Path,
    file: &Path,
    secrets: &mut Vec<String>,
) -> Result<Value, String> {
    Ok(match value {
        Value::String(text) => Value::String(expand(&text, root, file, secrets)?),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| expand_value(v, root, file, secrets))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(values) => {
            let mut expanded = serde_json::Map::new();
            for (key, value) in values {
                let key = expand(&key, root, file, secrets)?;
                if expanded.contains_key(&key) {
                    return Err("LSP template produced duplicate object keys".into());
                }
                expanded.insert(key, expand_value(value, root, file, secrets)?);
            }
            Value::Object(expanded)
        }
        other => other,
    })
}

fn collect_credentials(value: &Value, sensitive: bool, redactions: &mut Vec<String>) {
    match value {
        Value::String(text) if sensitive && !text.is_empty() => redactions.push(text.clone()),
        Value::Object(map) => {
            for (key, value) in map {
                collect_credentials(
                    value,
                    sensitive || super::trace::credential_key(key),
                    redactions,
                );
            }
        }
        Value::Array(array) => {
            for value in array {
                collect_credentials(value, sensitive, redactions);
            }
        }
        _ => {}
    }
}

pub async fn prepare(
    config: &LspServerConfig,
    policy: &LspSettings,
    root: &Path,
    file: &Path,
) -> Result<PreparedOptions, String> {
    let options = &config.options;
    if options.disabled {
        return Err("language server is disabled by configuration".into());
    }
    let mut prepared = PreparedOptions {
        source: "off".into(),
        settings: json!({}),
        ..Default::default()
    };
    if !policy.init_options_off && options.init_options_mode != LspMode::Off {
        if options.init_options.is_some() && options.init_options_file.is_some() {
            return Err("use init_options or init_options_file, not both".into());
        }
        prepared.initialization = if let Some(inline) = &options.init_options {
            prepared.source = "inline".into();
            Some(inline.clone())
        } else if let Some(path) = &options.init_options_file {
            let path = file_path(path, policy)?;
            match tokio::fs::metadata(&path).await {
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        && options.init_options_mode == LspMode::Auto =>
                {
                    prepared.source = "auto: file absent".into();
                    None
                }
                _ => {
                    prepared.source = format!("file: {}", path.display());
                    Some(read_json(&path).await?)
                }
            }
        } else {
            if options.init_options_mode == LspMode::On {
                return Err(
                    "init_options_mode=on requires init_options or init_options_file".into(),
                );
            }
            prepared.source = "auto: no initialization source configured".into();
            None
        };
        if let Some(value) = prepared.initialization.take() {
            prepared.initialization =
                Some(expand_value(value, root, file, &mut prepared.redactions)?);
        }
    }
    if options.settings.is_some() && options.settings_file.is_some() {
        return Err("use settings or settings_file, not both".into());
    }
    let settings = if let Some(path) = &options.settings_file {
        read_json(&file_path(path, policy)?).await?
    } else {
        options.settings.clone().unwrap_or_else(|| json!({}))
    };
    if !settings.is_object() {
        return Err("LSP settings must be a JSON object".into());
    }
    prepared.settings = expand_value(settings, root, file, &mut prepared.redactions)?;
    if let Some(value) = &prepared.initialization {
        collect_credentials(value, false, &mut prepared.redactions);
    }
    collect_credentials(&prepared.settings, false, &mut prepared.redactions);
    prepared
        .redactions
        .sort_unstable_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    prepared.redactions.dedup();
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn server_errors_cannot_echo_credentials_from_initialization_or_settings() {
        let root = tempfile::tempdir().unwrap();
        let config = LspServerConfig {
            command: "unused".into(),
            args: vec![],
            root_markers: vec![],
            options: super::super::super::lsp_policy::LspOptions {
                init_options: Some(json!({"apiKey":"inline-credential-for-log-test"})),
                settings: Some(json!({"auth":{"password":"settings-credential-for-log-test"}})),
                ..Default::default()
            },
        };
        let prepared = prepare(
            &config,
            &Default::default(),
            root.path(),
            &root.path().join("source.rs"),
        )
        .await
        .unwrap();
        let path = root.path().join("lsp.log");
        let trace = super::super::trace::Trace::open(
            Some(&path),
            super::super::super::lsp_policy::LspTrace::Verbose,
            prepared.redactions,
        )
        .unwrap();
        trace.record("receive", &json!({"error":{"message":"invalid apiKey inline-credential-for-log-test; password settings-credential-for-log-test"}}));
        let log = std::fs::read_to_string(path).unwrap();
        assert!(!log.contains("inline-credential-for-log-test"));
        assert!(!log.contains("settings-credential-for-log-test"));
        assert!(log.contains("[redacted]"));
    }

    #[test]
    fn templates_expand_keys_and_keep_types() {
        let root = std::env::temp_dir().join("lsp project 中文");
        let file = root.join("src/main.cj");
        let result = expand_value(
            json!({"${workspaceUri}": {"path":"${workspaceFolder}", "enabled":true,"n":3}}),
            &root,
            &file,
            &mut vec![],
        )
        .unwrap();
        let value = result.as_object().unwrap().values().next().unwrap();
        assert_eq!(value["enabled"], true);
        assert_eq!(value["n"], 3);
        assert!(result
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .contains("%20"));
        assert!(expand("${unknown}", &root, &file, &mut vec![]).is_err());
    }
    #[tokio::test]
    async fn off_skips_invalid_initialization_but_settings_are_independent() {
        let mut config = LspServerConfig {
            command: "unused".into(),
            args: vec![],
            root_markers: vec![],
            options: Default::default(),
        };
        config.options.init_options_mode = LspMode::Off;
        config.options.init_options_file = Some("missing.json".into());
        config.options.init_options = Some(json!({"bad":"${unknown}"}));
        config.options.settings = Some(json!({"python":{"analysis":{"mode":"basic"}}}));
        let root = std::env::temp_dir();
        let prepared = prepare(&config, &Default::default(), &root, &root.join("a.py"))
            .await
            .unwrap();
        assert!(prepared.initialization.is_none());
        assert_eq!(prepared.settings["python"]["analysis"]["mode"], "basic");
    }
    #[tokio::test]
    async fn missing_auto_file_is_skipped_but_on_and_invalid_json_fail() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = LspServerConfig {
            command: "unused".into(),
            args: vec![],
            root_markers: vec![],
            options: Default::default(),
        };
        config.options.init_options_file = Some(dir.path().join("init.json"));
        assert!(prepare(
            &config,
            &Default::default(),
            dir.path(),
            &dir.path().join("a.cj")
        )
        .await
        .unwrap()
        .initialization
        .is_none());
        config.options.init_options_mode = LspMode::On;
        assert!(prepare(
            &config,
            &Default::default(),
            dir.path(),
            &dir.path().join("a.cj")
        )
        .await
        .is_err());
        tokio::fs::write(dir.path().join("init.json"), "{broken")
            .await
            .unwrap();
        config.options.init_options_mode = LspMode::Auto;
        assert!(prepare(
            &config,
            &Default::default(),
            dir.path(),
            &dir.path().join("a.cj")
        )
        .await
        .is_err());
    }
}
