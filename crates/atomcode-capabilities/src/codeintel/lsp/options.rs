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

fn file_path(
    path: &Path,
    policy: &LspSettings,
    root: &Path,
    file: &Path,
    redactions: &mut Vec<String>,
) -> Result<PathBuf, String> {
    let previous = redactions.len();
    let text = expand(&path.to_string_lossy(), root, file, redactions)?;
    let path = resolve_file_path(&text, policy)?;
    if redactions.len() > previous {
        // URI decoding and Windows separators can change an environment value's
        // spelling. Redact the resolved filename as well as the original value.
        redactions.push(path.to_string_lossy().into_owned());
    }
    Ok(path)
}

fn resolve_file_path(text: &str, policy: &LspSettings) -> Result<PathBuf, String> {
    if text.starts_with("file:") {
        let url = url::Url::parse(text).map_err(|_| "invalid LSP configuration file URI")?;
        if url.query().is_some() || url.fragment().is_some() {
            return Err("LSP configuration file URI must not contain a query or fragment".into());
        }
        return url
            .to_file_path()
            .map_err(|_| "LSP configuration URI is not a local file path".into());
    }
    if text.contains("://") {
        return Err("LSP configuration files require local paths or file URIs".into());
    }
    if let Some(path) = crate::pathutil::expand_tilde(text) {
        return Ok(path);
    }
    let path = PathBuf::from(text);
    if path.is_absolute() {
        return Ok(path);
    }
    policy
        .config_dir
        .as_ref()
        .map(|base| base.join(path))
        .ok_or_else(|| "relative LSP configuration path requires a host config directory".into())
}

fn redact_path_message(mut message: String, redactions: &[String]) -> String {
    // Sort here too: preparation may fail before its final redaction sorting.
    let mut secrets: Vec<_> = redactions
        .iter()
        .filter(|value| !value.is_empty())
        .collect();
    secrets.sort_unstable_by_key(|value| std::cmp::Reverse(value.len()));
    for secret in secrets {
        message = message.replace(secret, "[redacted]");
    }
    message
}

async fn read_option_json(path: &Path, redactions: &[String]) -> Result<Value, String> {
    read_json(path)
        .await
        .map_err(|message| redact_path_message(message, redactions))
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
            let path = file_path(path, policy, root, file, &mut prepared.redactions)?;
            match tokio::fs::metadata(&path).await {
                Err(e)
                    if e.kind() == std::io::ErrorKind::NotFound
                        && options.init_options_mode == LspMode::Auto =>
                {
                    prepared.source = "auto: file absent".into();
                    None
                }
                _ => {
                    prepared.source = redact_path_message(
                        format!("file: {}", path.display()),
                        &prepared.redactions,
                    );
                    Some(read_option_json(&path, &prepared.redactions).await?)
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
        let path = file_path(path, policy, root, file, &mut prepared.redactions)?;
        read_option_json(&path, &prepared.redactions).await?
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
    #[test]
    fn file_templates_preserve_relative_paths_and_decode_uri_escapes() {
        let root = std::env::temp_dir().join("LSP 项目 #1");
        let file = root.join("src/main.cj");
        let policy = LspSettings {
            config_dir: Some(root.join("user")),
            ..Default::default()
        };
        for (template, expected) in [
            ("init.json", root.join("user/init.json")),
            ("${workspaceFolder}/init.json", root.join("init.json")),
            ("${workspaceUri}/init.json", root.join("init.json")),
            ("${fileDir}/settings.json", root.join("src/settings.json")),
            (
                "${fileDirUri}/settings.json",
                root.join("src/settings.json"),
            ),
        ] {
            assert_eq!(
                file_path(Path::new(template), &policy, &root, &file, &mut vec![]).unwrap(),
                expected
            );
        }
        assert_eq!(
            file_path(
                &root.join("literal.json"),
                &policy,
                &root,
                &file,
                &mut vec![]
            )
            .unwrap(),
            root.join("literal.json")
        );
        let home = crate::pathutil::expand_tilde("~/").unwrap();
        assert_eq!(
            file_path(Path::new("~/init.json"), &policy, &root, &file, &mut vec![]).unwrap(),
            home.join("init.json")
        );
        assert_eq!(
            file_path(
                Path::new("${HOME}/init.json"),
                &policy,
                &root,
                &file,
                &mut vec![]
            )
            .unwrap(),
            home.join("init.json")
        );
        for bad in [
            "${unknown}/x",
            "${workspaceUri}/x?query=1",
            "${workspaceUri}/x#fragment",
            "https://example.com/init.json",
            "${unterminated",
        ] {
            assert!(
                file_path(Path::new(bad), &policy, &root, &file, &mut vec![]).is_err(),
                "{bad}"
            );
        }
        assert!(file_path(
            Path::new("init.json"),
            &Default::default(),
            &root,
            &file,
            &mut vec![]
        )
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_file_uri_paths_are_decoded_to_drive_paths() {
        let root = Path::new(r"C:\项目 空格");
        let file = root.join("main.cj");
        assert_eq!(
            file_path(
                Path::new("${workspaceUri}/init.json"),
                &Default::default(),
                root,
                &file,
                &mut vec![]
            )
            .unwrap(),
            root.join("init.json")
        );
    }

    #[tokio::test]
    async fn two_projects_read_their_own_options_and_reread_changed_files() {
        let base = tempfile::tempdir().unwrap();
        let mut config = LspServerConfig {
            command: "unused".into(),
            args: vec![],
            root_markers: vec![],
            options: Default::default(),
        };
        config.options.init_options_mode = LspMode::On;
        config.options.init_options_file = Some("${workspaceUri}/init.json".into());
        config.options.settings_file = Some("${fileDir}/settings.json".into());
        for name in ["项目 A", "项目 B"] {
            let root = base.path().join(name);
            tokio::fs::create_dir_all(root.join("src")).await.unwrap();
            tokio::fs::write(root.join("init.json"), serde_json::to_vec(&json!({"multiModuleOption":{"${workspaceUri}":{"name":name,"src_path":"${workspaceUri}/src"}}})).unwrap()).await.unwrap();
            tokio::fs::write(
                root.join("src/settings.json"),
                r#"{"enabled":true,"limit":3}"#,
            )
            .await
            .unwrap();
            let query_file = root.join("src/main.cj");
            let prepared = prepare(&config, &Default::default(), &root, &query_file)
                .await
                .unwrap();
            assert_eq!(
                prepared.initialization.as_ref().unwrap()["multiModuleOption"][uri(&root).unwrap()]
                    ["name"],
                name
            );
            assert_eq!(prepared.settings, json!({"enabled":true,"limit":3}));
            tokio::fs::write(root.join("init.json"), r#"{"updated":true}"#)
                .await
                .unwrap();
            let updated = prepare(&config, &Default::default(), &root, &query_file)
                .await
                .unwrap();
            assert_eq!(updated.initialization, Some(json!({"updated":true})));
        }
    }

    #[tokio::test]
    async fn templated_missing_files_keep_auto_on_and_settings_failure_semantics() {
        let root = tempfile::tempdir().unwrap();
        let mut config = LspServerConfig {
            command: "unused".into(),
            args: vec![],
            root_markers: vec![],
            options: Default::default(),
        };
        config.options.init_options_file = Some("${workspaceFolder}/missing.json".into());
        let file = root.path().join("main.cj");
        assert!(prepare(&config, &Default::default(), root.path(), &file)
            .await
            .unwrap()
            .initialization
            .is_none());
        config.options.init_options_mode = LspMode::On;
        assert!(prepare(&config, &Default::default(), root.path(), &file)
            .await
            .is_err());
        config.options.init_options_mode = LspMode::Off;
        config.options.init_options_file = Some("${invalid}".into());
        assert!(prepare(&config, &Default::default(), root.path(), &file)
            .await
            .is_ok());
        config.options.settings_file = Some("${workspaceUri}/missing.json".into());
        assert!(prepare(&config, &Default::default(), root.path(), &file)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn path_errors_redact_environment_values_before_preparation_succeeds() {
        // A dedicated test-process environment key avoids touching real credentials.
        const KEY: &str = "ATOMCODE_LSP_PATH_TEMPLATE_TEST_SECRET";
        const SECRET: &str = "path-template-secret/segment-for-test";
        std::env::set_var(KEY, SECRET);
        let root = tempfile::tempdir().unwrap();
        let mut config = LspServerConfig {
            command: "unused".into(),
            args: vec![],
            root_markers: vec![],
            options: Default::default(),
        };
        config.options.init_options_mode = LspMode::On;
        config.options.init_options_file =
            Some(format!("${{workspaceUri}}/${{env:{KEY}}}/missing.json").into());
        let result = prepare(
            &config,
            &Default::default(),
            root.path(),
            &root.path().join("a.cj"),
        )
        .await;
        let message = result.err().unwrap();
        assert!(message.contains("[redacted]"));
        assert!(!message.contains(SECRET));
        tokio::fs::create_dir_all(root.path().join(SECRET))
            .await
            .unwrap();
        tokio::fs::write(root.path().join(SECRET).join("missing.json"), "{}")
            .await
            .unwrap();
        let prepared = prepare(
            &config,
            &Default::default(),
            root.path(),
            &root.path().join("a.cj"),
        )
        .await
        .unwrap();
        std::env::remove_var(KEY);
        assert!(prepared.source.contains("[redacted]"));
        assert!(!prepared.source.contains(SECRET));
    }

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
