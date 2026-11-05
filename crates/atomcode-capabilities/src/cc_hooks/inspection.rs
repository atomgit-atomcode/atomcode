//! Host-local configuration inspection. Never installs or mutates live hooks.
use super::*;
use crate::ProductDirs;
use serde_json::json;
use sha2::{Digest, Sha256};

struct Candidate {
    value: Value,
    config: Option<HookConfig>,
}
fn row(
    cwd: &Path,
    source: &Path,
    scope: &str,
    name: &str,
    event: &str,
    matcher: &Option<String>,
    command: &str,
    timeout_ms: u64,
    disabled: bool,
    config: Option<HookConfig>,
) -> Candidate {
    let args = config
        .as_ref()
        .map(|config| config.args.as_slice())
        .unwrap_or(&[]);
    let mut value = json!({"name":name,"event":event,"matcher":matcher,"command":command,"args":args,"timeout_ms":timeout_ms,
        "source":source,"scope":scope,"disabled":disabled,"testable":config.is_some()});
    let fingerprint = format!("{}\n{}\n{:?}", cwd.display(), value, config);
    value["id"] = json!(format!("{:x}", Sha256::digest(fingerprint.as_bytes())));
    Candidate { value, config }
}
fn discover(cwd: &Path, dirs: &ProductDirs) -> (Value, Vec<Candidate>) {
    let mut files = Vec::new();
    let mut candidates = Vec::new();
    for (scope, path) in [
        ("global", global_hooks_path(dirs.user())),
        ("project", project_hooks_path(cwd)),
    ] {
        let status = hooks_file_status(&path);
        files.push(json!({"scope":scope,"path":path,"status":status}));
        if let Ok(raw) = std::fs::read_to_string(&path) {
            if let Ok(file) = serde_json::from_str::<HooksFile>(&crate::jsonc::strip_comments(&raw))
            {
                for (name, entry) in file.hooks {
                    let event = HookEvent::parse(&entry.event);
                    let config =
                        (!entry.disabled)
                            .then_some(event)
                            .flatten()
                            .map(|event| HookConfig {
                                event,
                                matcher: entry.matcher.clone(),
                                command: entry.command.clone(),
                                timeout_ms: entry.timeout_ms,
                                args: Vec::new(),
                                plugin_root: None,
                                plugin_content_hash: None,
                            });
                    candidates.push(row(
                        cwd,
                        &path,
                        scope,
                        &name,
                        &entry.event,
                        &entry.matcher,
                        &entry.command,
                        entry.timeout_ms,
                        entry.disabled,
                        config,
                    ));
                }
            }
        }
    }
    for (index, hook) in crate::plugin::loader::installed_plugin_cc_hooks(dirs)
        .into_iter()
        .enumerate()
    {
        let config = HookConfig::from_plugin_spec(
            &hook.event,
            hook.matcher.clone(),
            hook.command.clone(),
            hook.timeout_secs,
            hook.plugin_root.clone(),
        )
        .map(|mut config| {
            config.args = hook.args.clone();
            config
        });
        candidates.push(row(
            cwd,
            &hook.plugin_root,
            "plugin",
            &format!("plugin-{}", index + 1),
            &hook.event,
            &hook.matcher,
            &hook.command,
            config.as_ref().map_or(10000, |c| c.timeout_ms),
            false,
            config,
        ));
    }
    let plugins: Vec<_> = crate::plugin::installed_plugin_hook_trust_status(dirs).into_iter().map(|p|
        json!({"plugin":p.plugin_id,"trusted":p.trusted,"hooks":p.hook_count,"events":p.events,"invalid":p.hash.starts_with("invalid:")})).collect();
    (
        json!({"cwd":cwd,"files":files,"plugins":plugins,"items":candidates.iter().map(|r|r.value.clone()).collect::<Vec<_>>()}),
        candidates,
    )
}

/// Synthetic event payload shared with the CLI diagnostic command.
pub fn test_payload(event: HookEvent, cwd: &Path) -> Value {
    let mut value =
        json!({"session_id":"test-session-0000","hook_event_name":event.cc_name(),"cwd":cwd});
    match event {
        HookEvent::PreToolUse => {
            value["tool_name"] = json!("bash");
            value["tool_input"] = json!({"command":"echo hello"});
            value["call_id"] = json!("hook-test");
        }
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure => {
            value["tool_name"] = json!("bash");
            value["call_id"] = json!("hook-test");
            value["tool_response"] = json!(if event == HookEvent::PostToolUse {
                "hello\n"
            } else {
                "command failed\n"
            });
        }
        HookEvent::UserPromptSubmit => value["prompt"] = json!("test prompt"),
        HookEvent::Stop | HookEvent::StopFailure => {
            value["transcript_path"] = Value::Null;
            value["stop_hook_active"] = json!(false);
            value["stop_reason"] = json!(if event == HookEvent::Stop {
                "Stopped"
            } else {
                "ProviderError"
            });
        }
        _ => {}
    }
    value
}

/// User-only host command. Test selectors are exact content-bound IDs, never shell text.
pub async fn command(cwd: &Path, dirs: &ProductDirs, args: &str) -> Result<Value, String> {
    let operation = command_inner(cwd, dirs, args);
    if args.split_whitespace().next() == Some("test") {
        test_deadline(operation).await
    } else {
        operation.await
    }
}

async fn test_deadline(
    operation: impl std::future::Future<Output = Result<Value, String>>,
) -> Result<Value, String> {
    tokio::time::timeout(Duration::from_secs(30), operation)
        .await
        .map_err(|_| {
            "Hook test timed out after 30 seconds / 脚本测试超过 30 秒，已停止等待".to_string()
        })?
}

async fn command_inner(cwd: &Path, dirs: &ProductDirs, args: &str) -> Result<Value, String> {
    let project = cwd.to_path_buf();
    let directories = dirs.clone();
    let (catalog, rows) = tokio::task::spawn_blocking(move || discover(&project, &directories))
        .await
        .map_err(|error| format!("hooks inspection failed: {error}"))?;
    let args = args.trim();
    if matches!(args, "" | "list" | "paths") {
        return Ok(catalog);
    }
    let (action, id) = args
        .split_once(char::is_whitespace)
        .ok_or("usage: /hooks [list|paths|show <id>|test <id>]")?;
    let candidate = rows
        .into_iter()
        .find(|row| row.value["id"].as_str() == Some(id.trim()))
        .ok_or("hook configuration changed or selection not found; reopen /hooks")?;
    if action == "show" {
        return Ok(json!({"item":candidate.value}));
    }
    if action != "test" {
        return Err("unknown hooks action".into());
    }
    let mut config = candidate
        .config
        .ok_or("disabled or unknown-event hooks cannot be tested")?;
    config.timeout_ms = config.timeout_ms.min(30000);
    let payload = test_payload(config.event, cwd);
    let result = run_hook_for_test(&config, cwd, &payload).await;
    Ok(match result {
        Some(out) => {
            json!({"item":candidate.value,"result":{"completed":true,"exit_code":out.exit_code,
            "stdout":out.stdout.chars().take(16000).collect::<String>(),"stderr":out.stderr.chars().take(16000).collect::<String>()}})
        }
        None => {
            json!({"item":candidate.value,"result":{"completed":false,"exit_code":null,"stdout":"","stderr":"Hook timed out or failed to start / 测试超时或脚本无法启动"}})
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dirs(home: &Path) -> ProductDirs {
        ProductDirs::new(home, atomcode_config::distribution::PROJECT_DIR_NAME)
    }
    #[tokio::test]
    async fn inspection_preserves_disabled_unknown_and_parse_errors_without_execution() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(
            cwd.path().join(".hooks.json"),
            r#"{"hooks":{
            "audit":{"event":"PreToolUse","matcher":"bash","command":"must-not-run"},
            "disabled":{"event":"Stop","command":"must-not-run","disabled":true},
            "typo":{"event":"Typo","command":"must-not-run"}}}"#,
        )
        .unwrap();
        std::fs::write(home.path().join("hooks.json"), "{ malformed").unwrap();
        let view = command(cwd.path(), &dirs(home.path()), "list")
            .await
            .unwrap();
        assert_eq!(view["items"].as_array().unwrap().len(), 3);
        assert_eq!(view["files"][0]["status"]["status"], "malformed");
        assert_eq!(view["files"][1]["status"]["disabled"], 1);
        let disabled = view["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "disabled")
            .unwrap();
        assert!(command(
            cwd.path(),
            &dirs(home.path()),
            &format!("test {}", disabled["id"].as_str().unwrap())
        )
        .await
        .is_err());
        assert!(
            command(cwd.path(), &dirs(home.path()), "test echo injected")
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn old_selection_cannot_execute_edited_or_other_project_configuration() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let text = r#"{"hooks":{"audit":{"event":"PreToolUse","command":"must-not-run"}}}"#;
        std::fs::write(cwd.path().join(".hooks.json"), text).unwrap();
        std::fs::write(other.path().join(".hooks.json"), text).unwrap();
        let view = command(cwd.path(), &dirs(home.path()), "").await.unwrap();
        let id = view["items"][0]["id"].as_str().unwrap();
        assert!(
            command(other.path(), &dirs(home.path()), &format!("test {id}"))
                .await
                .is_err()
        );
        std::fs::write(
            cwd.path().join(".hooks.json"),
            text.replace("must-not-run", "changed-command"),
        )
        .unwrap();
        assert!(
            command(cwd.path(), &dirs(home.path()), &format!("test {id}"))
                .await
                .is_err()
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn tests_are_explicit_and_report_real_exit_and_output() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join(".hooks.json"),r#"{"hooks":{"audit":{"event":"Stop","command":"printf tested; printf diagnostic >&2; exit 2"}}}"#).unwrap();
        let view = command(cwd.path(), &dirs(home.path()), "list")
            .await
            .unwrap();
        let id = view["items"][0]["id"].as_str().unwrap();
        let shown = command(cwd.path(), &dirs(home.path()), &format!("show {id}"))
            .await
            .unwrap();
        assert!(shown.get("result").is_none());
        let result = command(cwd.path(), &dirs(home.path()), &format!("test {id}"))
            .await
            .unwrap();
        assert_eq!(result["result"]["exit_code"], 2);
        assert_eq!(result["result"]["stdout"], "tested");
        assert_eq!(result["result"]["stderr"], "diagnostic");
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_deadline_cancels_a_late_continuation() {
        let mut continued = false;
        let result = test_deadline(async {
            tokio::time::sleep(Duration::from_secs(31)).await;
            continued = true;
            Ok(Value::Null)
        })
        .await;
        assert!(result.unwrap_err().contains("30 seconds"));
        assert!(!continued);
    }

    #[test]
    fn explicit_arguments_are_visible_and_bound_to_the_selection() {
        let cwd = std::env::temp_dir();
        let make = |argument: &str| {
            let config = HookConfig {
                event: HookEvent::Stop,
                matcher: None,
                command: "python3".into(),
                args: vec![argument.into()],
                timeout_ms: 10000,
                plugin_root: None,
                plugin_content_hash: None,
            };
            row(
                &cwd,
                &cwd,
                "plugin",
                "audit",
                "Stop",
                &None,
                "python3",
                10000,
                false,
                Some(config),
            )
        };
        let first = make("original.py");
        let changed = make("changed.py");
        assert_eq!(first.value["args"], json!(["original.py"]));
        assert_ne!(first.value["id"], changed.value["id"]);
    }

    #[test]
    fn synthetic_payloads_keep_event_specific_fields() {
        let cwd = std::env::temp_dir();
        assert!(test_payload(HookEvent::Stop, &cwd)
            .get("tool_input")
            .is_none());
        assert_eq!(
            test_payload(HookEvent::UserPromptSubmit, &cwd)["prompt"],
            "test prompt"
        );
        assert!(test_payload(HookEvent::PostToolUseFailure, &cwd)
            .get("tool_response")
            .is_some());
    }
}
