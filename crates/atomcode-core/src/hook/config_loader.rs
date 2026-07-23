use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use super::async_batcher::{AsyncWebhookConfig, AsyncWebhookRegistry};
use super::script_runner::{ScriptHook, ScriptHookConfig};
use super::webhook::{WebhookConfig, WebhookHook};
use crate::hook::HookEngine;

/// Hooks 配置结构
#[derive(Debug, Deserialize)]
pub struct HooksConfig {
    /// 启用的脚本 hooks 列表
    #[serde(default)]
    pub hooks: Vec<ScriptHookConfig>,
    /// 启用的 webhook hooks 列表
    #[serde(default)]
    pub webhooks: Vec<WebhookConfig>,
    /// 启用的异步 webhook hooks 列表
    #[serde(default)]
    pub async_webhooks: Vec<AsyncWebhookConfig>,
}

impl HooksConfig {
    /// 从 TOML 文件加载配置
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read hooks config: {}", path.display()))?;

        let config: HooksConfig = toml::from_str(&content)
            .with_context(|| format!("Failed to parse hooks config: {}", path.display()))?;

        Ok(config)
    }

    /// 从目录自动加载脚本 hooks（hooks.toml 在该目录下）
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let config_path = dir.join("hooks.toml");
        if config_path.exists() {
            Self::from_file(&config_path)
        } else {
            Ok(Self {
                hooks: Vec::new(),
                webhooks: Vec::new(),
                async_webhooks: Vec::new(),
            })
        }
    }

    /// 注册所有脚本 hooks 到 HookEngine（统一引擎）
    /// 注册所有脚本 hooks 到 HookEngine
    pub fn register_hooks_to_engine(&self, engine: &mut HookEngine, base_dir: &Path) {
        for config in &self.hooks {
            if !config.enabled {
                continue;
            }

            // 解析脚本路径（相对或绝对）
            let script_path = if config.script.is_absolute() {
                config.script.clone()
            } else {
                base_dir.join(&config.script)
            };

            if !script_path.exists() {
                tracing::warn!(
                    "[Hook] Warning: Script not found: {}",
                    script_path.display()
                );
                continue;
            }

            let config_with_path = ScriptHookConfig {
                name: config.name.clone(),
                trigger: config.trigger.clone(),
                script: script_path,
                script_type: config.script_type.clone(),
                enabled: config.enabled,
                timeout_secs: config.timeout_secs,
                description: config.description.clone(),
            };

            let hook = Arc::new(ScriptHook::new(config_with_path));

            match config.trigger.as_str() {
                "pre_tool" | "pre_tool_execution" => {
                    engine.register_pre_tool_hook(hook);
                }
                "post_tool" | "post_tool_execution" => {
                    engine.register_post_tool_hook(hook);
                }
                "post_turn" => {
                    engine.register_post_turn_hook(hook);
                }
                "system_prompt" => {
                    engine.register_system_prompt_hook(hook);
                }
                _ => {
                    tracing::warn!("[Hook] Warning: Unknown trigger type: {}", config.trigger);
                }
            }
        }
    }

    /// 注册所有 webhooks 到 HookEngine（按 trigger 过滤，避免冗余注册）
    /// TODO(#914): async_webhook batchers 当前仅创建但未 flush。
    /// 需在 HookEngine 上增加 flush 调度器，定期调用 AsyncWebhookRegistry::flush_batch()。
    pub fn register_webhooks_to_engine(&self, engine: &mut HookEngine) {
        let mut async_registry = AsyncWebhookRegistry::new();

        for config in &self.async_webhooks {
            if !config.enabled {
                continue;
            }
            async_registry.register(config.clone());
        }

        for config in &self.webhooks {
            if !config.enabled {
                continue;
            }

            let webhook = if let Some(batcher) = async_registry.get(&config.name) {
                Arc::new(WebhookHook::new_with_async(config.clone(), batcher.clone()))
            } else {
                Arc::new(WebhookHook::new(config.clone()))
            };

            // 按 trigger 只注册到匹配的 slot（避免 9 个不必要的 Arc clone）
            Self::register_webhook_by_trigger(engine, &webhook, &config.trigger);

            tracing::info!("[Webhook] Registered: {} -> {}", config.name, config.url);
        }

        if !async_registry.batchers.is_empty() {
            tracing::info!(
                "[AsyncWebhook] Registered {} async batchers",
                async_registry.batchers.len()
            );
        }
    }

    /// 按 webhook 的 trigger 字符串注册到对应的 HookEngine slot。
    /// 支持逗号分隔的多个 trigger（如 "pre_tool,post_tool,error"）。
    fn register_webhook_by_trigger(
        engine: &mut HookEngine,
        webhook: &Arc<WebhookHook>,
        trigger: &str,
    ) {
        let t = trigger.to_lowercase();

        // 注意：WebhookHook 的 trait 实现内部还有自己的 trigger check，
        // 这里提前过滤以节省 Arc clone + Vec push 开销。
        if t.contains("pre_tool") || t.contains("before_tool") {
            engine.register_pre_tool_hook(webhook.clone());
        }
        if t.contains("post_tool") || t.contains("after_tool") {
            engine.register_post_tool_hook(webhook.clone());
        }
        if t.contains("post_turn") {
            engine.register_post_turn_hook(webhook.clone());
        }
        if t.contains("system_prompt") {
            engine.register_system_prompt_hook(webhook.clone());
        }
        if t.contains("session_start") {
            engine.register_on_session_start_hook(webhook.clone());
        }
        if t.contains("session_end") {
            engine.register_on_session_end_hook(webhook.clone());
        }
        if t.contains("error") {
            engine.register_on_error_hook(webhook.clone());
        }
        if t.contains("turn_start") {
            engine.register_on_turn_start_hook(webhook.clone());
        }
        if t.contains("turn_complete") || t.contains("after_turn") {
            engine.register_on_turn_complete_hook(webhook.clone());
        }
        if t.contains("tool_call_start") {
            engine.register_on_tool_call_start_hook(webhook.clone());
        }
        if t.contains("model_response") {
            engine.register_on_model_response_hook(webhook.clone());
        }
        // Note: "message" / "user_prompt_submit" webhook support requires an
        // OnMessageReceivedHook slot in HookEngine (TODO: follow-up PR).
    }
}

/// Load all enabled script hooks from TOML configs (global + project),
/// preserving their names. Project hooks override global hooks by name.
///
/// This is the TOML counterpart of [`json_config::load_hooks_config_with_names`],
/// used by `hook test <name>` to support testing TOML-configured hooks.
pub fn load_script_hooks_with_names(project_dir: &Path) -> Vec<(String, ScriptHookConfig)> {
    let mut merged: BTreeMap<String, ScriptHookConfig> = BTreeMap::new();

    // Global hooks: ~/.atomcode/hooks/hooks.toml
    if let Some(home) = dirs::home_dir() {
        let global_dir = home.join(".atomcode").join("hooks");
        match HooksConfig::from_dir(&global_dir) {
            Ok(config) => {
                for mut hook in config.hooks {
                    if hook.enabled {
                        if !hook.script.is_absolute() {
                            hook.script = global_dir.join(&hook.script);
                        }
                        merged.insert(hook.name.clone(), hook);
                    }
                }
            }
            Err(e) => {
                if global_dir.exists() {
                    tracing::warn!(
                        "[Hook] Failed to load global TOML hook config from {}: {}",
                        global_dir.display(),
                        e
                    );
                }
            }
        }
    }

    // Project hooks: <cwd>/.atomcode/hooks/hooks.toml — override global by name
    let project_hooks_dir = project_dir.join(".atomcode").join("hooks");
    match HooksConfig::from_dir(&project_hooks_dir) {
        Ok(config) => {
            for mut hook in config.hooks {
                if hook.enabled {
                    if !hook.script.is_absolute() {
                        hook.script = project_hooks_dir.join(&hook.script);
                    }
                    merged.insert(hook.name.clone(), hook);
                }
            }
        }
        Err(e) => {
            if project_hooks_dir.exists() {
                tracing::warn!(
                    "[Hook] Failed to load project TOML hook config from {}: {}",
                    project_hooks_dir.display(),
                    e
                );
            }
        }
    }

    merged.into_iter().collect()
}

// ────────────────────────────────────────────────────────────────────────────
// 旧 API (`load_hooks` / `load_hooks_from_dir` + `HookRegistry`) 已移除。
// 所有 hook 加载现在统一通过 `HookEngine::load_all()` 走 `register_hooks_to_engine`。
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;

    // ── HooksConfig deserialization ──────────────────────────────────

    #[test]
    fn test_hooks_config_deserialize_toml() {
        let toml_str = r#"
[[hooks]]
name = "pre-check"
trigger = "pre_tool"
script = "check.sh"
enabled = true
timeout_secs = 5

[[hooks]]
name = "post-check"
trigger = "post_tool"
script = "report.sh"
script_type = "python"
enabled = true

[[webhooks]]
name = "notify"
trigger = "post_turn"
url = "https://example.com/hook"

[[async_webhooks]]
name = "batch-logger"
trigger = "pre_tool"
url = "https://example.com/batch"
batch_size = 20
"#;

        let config: HooksConfig = toml::from_str(toml_str).expect("Should parse TOML");
        assert_eq!(config.hooks.len(), 2);
        assert_eq!(config.webhooks.len(), 1);
        assert_eq!(config.async_webhooks.len(), 1);

        // Check first hook
        assert_eq!(config.hooks[0].name, "pre-check");
        assert_eq!(config.hooks[0].trigger, "pre_tool");
        assert_eq!(config.hooks[0].script.to_string_lossy(), "check.sh");
        assert!(config.hooks[0].enabled);
        assert_eq!(config.hooks[0].timeout_secs, 5);
        assert_eq!(config.hooks[0].script_type, "shell");

        // Check second hook
        assert_eq!(config.hooks[1].name, "post-check");
        assert_eq!(config.hooks[1].trigger, "post_tool");
        assert_eq!(config.hooks[1].script_type, "python");

        // Check webhook
        assert_eq!(config.webhooks[0].name, "notify");
        assert_eq!(config.webhooks[0].url, "https://example.com/hook");

        // Check async webhook
        assert_eq!(config.async_webhooks[0].name, "batch-logger");
        assert_eq!(config.async_webhooks[0].batch_size, 20);
    }

    #[test]
    fn test_hooks_config_empty() {
        let config: HooksConfig = toml::from_str("").expect("Should parse empty TOML");
        assert!(config.hooks.is_empty());
        assert!(config.webhooks.is_empty());
        assert!(config.async_webhooks.is_empty());
    }

    // ── HooksConfig::from_file ───────────────────────────────────────

    #[test]
    fn test_from_file_valid_toml() {
        let dir = std::env::temp_dir().join(format!("hook_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let config_path = dir.join("hooks.toml");
        fs::write(
            &config_path,
            r#"
[[hooks]]
name = "test-hook"
trigger = "pre_tool"
script = "test.sh"
enabled = true
timeout_secs = 3
"#,
        )
        .expect("Should write test file");

        let config = HooksConfig::from_file(&config_path).expect("Should load from file");
        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks[0].name, "test-hook");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_from_file_nonexistent() {
        let result = HooksConfig::from_file(Path::new("/tmp/nonexistent_hooks_file_12345.toml"));
        assert!(result.is_err());
    }

    // ── HooksConfig::from_dir ───────────────────────────────────────

    #[test]
    fn test_from_dir_with_existing_config() {
        let dir = std::env::temp_dir().join(format!("hook_test_dir_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        fs::write(
            dir.join("hooks.toml"),
            r#"
[[hooks]]
name = "dir-hook"
trigger = "post_turn"
script = "report.sh"
"#,
        )
        .expect("Should write test file");

        let config = HooksConfig::from_dir(&dir).expect("Should load from dir");
        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks[0].name, "dir-hook");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_from_dir_without_config() {
        let dir = std::env::temp_dir().join(format!("hook_test_empty_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);

        let config = HooksConfig::from_dir(&dir).expect("Should return empty config");
        assert!(config.hooks.is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    // ── register_hooks_to_engine ──────────────────────────────────────

    fn make_script_config(name: &str, trigger: &str, script: &str) -> ScriptHookConfig {
        ScriptHookConfig {
            name: name.to_string(),
            trigger: trigger.to_string(),
            script: PathBuf::from(script),
            script_type: "shell".to_string(),
            enabled: true,
            timeout_secs: 5,
            description: String::new(),
        }
    }

    #[allow(dead_code)]
    fn assert_engine_stats(engine: &HookEngine, expected_has: bool) {
        // HookEngine doesn't expose per-slot counts, but we can check has_any()
        assert_eq!(engine.has_any(), expected_has, "has_any mismatch");
    }

    #[test]
    fn test_register_hooks_to_engine_pre_tool() {
        let config = HooksConfig {
            hooks: vec![make_script_config("pre", "pre_tool", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut engine = HookEngine::new();
        config.register_hooks_to_engine(&mut engine, Path::new("/tmp"));
        assert!(engine.has_any());
    }

    #[test]
    fn test_register_hooks_to_engine_post_tool() {
        let config = HooksConfig {
            hooks: vec![make_script_config("post", "post_tool", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut engine = HookEngine::new();
        config.register_hooks_to_engine(&mut engine, Path::new("/tmp"));
        assert!(engine.has_any());
    }

    #[test]
    fn test_register_hooks_to_engine_post_turn() {
        let config = HooksConfig {
            hooks: vec![make_script_config("turn", "post_turn", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut engine = HookEngine::new();
        config.register_hooks_to_engine(&mut engine, Path::new("/tmp"));
        assert!(engine.has_any());
    }

    #[test]
    fn test_register_hooks_to_engine_system_prompt() {
        let config = HooksConfig {
            hooks: vec![make_script_config("sys", "system_prompt", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut engine = HookEngine::new();
        config.register_hooks_to_engine(&mut engine, Path::new("/tmp"));
        assert!(engine.has_any());
    }

    #[test]
    fn test_register_hooks_to_engine_unknown_trigger() {
        let config = HooksConfig {
            hooks: vec![make_script_config("bad", "unknown_trigger", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut engine = HookEngine::new();
        config.register_hooks_to_engine(&mut engine, Path::new("/tmp"));
        assert!(!engine.has_any(), "unknown trigger should register nothing");
    }

    #[test]
    fn test_register_hooks_to_engine_disabled_skipped() {
        let mut script_cfg = make_script_config("disabled", "pre_tool", "/bin/echo");
        script_cfg.enabled = false;
        let config = HooksConfig {
            hooks: vec![script_cfg],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut engine = HookEngine::new();
        config.register_hooks_to_engine(&mut engine, Path::new("/tmp"));
        assert!(!engine.has_any(), "disabled hook should not register");
    }

    #[test]
    fn test_register_hooks_to_engine_nonexistent_script() {
        let config = HooksConfig {
            hooks: vec![make_script_config(
                "missing",
                "pre_tool",
                "/tmp/nonexistent_script_12345.sh",
            )],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut engine = HookEngine::new();
        config.register_hooks_to_engine(&mut engine, Path::new("/tmp"));
        assert!(!engine.has_any(), "nonexistent script should not register");
    }

    // ── register_webhooks_to_engine ─────────────────────────────────

    #[test]
    fn test_register_webhooks_to_engine_with_webhook() {
        let config = HooksConfig {
            hooks: vec![],
            webhooks: vec![WebhookConfig {
                name: "test-webhook".to_string(),
                trigger: "pre_tool".to_string(),
                url: "http://localhost:9999/hook".to_string(),
                enabled: true,
                timeout_secs: 5,
                method: "POST".to_string(),
                headers: std::collections::HashMap::new(),
                description: String::new(),
                retries: 0,
            }],
            async_webhooks: vec![],
        };
        let mut engine = HookEngine::new();
        config.register_webhooks_to_engine(&mut engine);
        // Webhook 会被注册到多个 slot
        assert!(engine.has_any());
    }

    #[test]
    fn test_register_webhooks_to_engine_empty() {
        let config = HooksConfig {
            hooks: vec![],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut engine = HookEngine::new();
        config.register_webhooks_to_engine(&mut engine);
        assert!(!engine.has_any());
    }

    // ── load_all integration (via HookEngine) ─────────────────────────

    #[test]
    fn test_hook_engine_load_all_from_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hooks_dir = dir.path().join(".atomcode").join("hooks");
        fs::create_dir_all(&hooks_dir).expect("create project hooks dir");
        fs::write(hooks_dir.join("project-hook.sh"), "").expect("create hook script");
        fs::write(
            hooks_dir.join("hooks.toml"),
            r#"
[[hooks]]
name = "test-hook"
trigger = "pre_tool"
script = "project-hook.sh"
enabled = true
"#,
        )
        .expect("Should write test file");

        let mut engine = HookEngine::new();
        engine.load_all(dir.path());
        assert!(engine.has_any(), "load_all should register hooks from dir");
    }

    #[test]
    #[serial_test::serial]
    fn test_hook_engine_load_all_empty_dir() {
        let _home = crate::plugin::test_support::isolated_home();
        let dir = tempfile::tempdir().expect("tempdir");

        let mut engine = HookEngine::new();
        engine.load_all(dir.path());
        assert!(!engine.has_any(), "empty dir must register no hooks");
    }

    #[test]
    #[serial_test::serial]
    fn test_hook_engine_load_all_uses_atomcode_home_for_global_toml() {
        let home = crate::plugin::test_support::isolated_home();
        let hooks_dir = home.path().join("hooks");
        fs::create_dir_all(&hooks_dir).expect("create global hooks dir");
        fs::write(hooks_dir.join("global-hook.sh"), "").expect("create hook script");
        fs::write(
            hooks_dir.join("hooks.toml"),
            r#"
[[hooks]]
name = "global-test-hook"
trigger = "pre_tool"
script = "global-hook.sh"
enabled = true
"#,
        )
        .expect("write global hooks config");

        let project = tempfile::tempdir().expect("tempdir");
        let mut engine = HookEngine::new();
        engine.load_all(project.path());

        assert!(
            engine.has_any(),
            "global TOML hooks must resolve from ATOMCODE_HOME"
        );
    }
}
