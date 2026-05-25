use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

use crate::hook::HookRegistry;
use super::script_runner::{ScriptHook, ScriptHookConfig};
use super::webhook::{WebhookHook, WebhookConfig};
use super::async_batcher::{AsyncWebhookRegistry, AsyncWebhookConfig};

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

    /// 注册所有脚本 hooks 到 HookRegistry
    pub fn register_hooks(&self, registry: &mut HookRegistry, base_dir: &Path) {
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
                eprintln!("[Hook] Warning: Script not found: {}", script_path.display());
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

            let hook = Arc::new(ScriptHook::new(config_with_path.clone()));

            // 根据 trigger 类型注册到不同的位置
            match config.trigger.as_str() {
                "pre_tool" | "pre_tool_execution" => {
                    registry.register_pre_tool_hook(hook);
                }
                "post_tool" | "post_tool_execution" => {
                    registry.register_post_tool_hook(hook);
                }
                "post_turn" => {
                    registry.register_post_turn_hook(hook);
                }
                "system_prompt" => {
                    registry.register_system_prompt_hook(hook);
                }
                _ => {
                    eprintln!("[Hook] Warning: Unknown trigger type: {}", config.trigger);
                }
            }
        }

        // 注册 webhooks
        self.register_webhooks(registry);
    }

    /// 注册所有 webhooks 到 HookRegistry
    pub fn register_webhooks(&self, registry: &mut HookRegistry) {
        // 创建异步批处理器注册表
        let mut async_registry = AsyncWebhookRegistry::new();

        // 先注册异步 webhooks
        for config in &self.async_webhooks {
            if !config.enabled {
                continue;
            }

            async_registry.register(config.clone());
        }

        // 注册同步 webhooks
        for config in &self.webhooks {
            if !config.enabled {
                continue;
            }

            // 检查是否有对应的异步批处理器
            let webhook = if let Some(batcher) = async_registry.get(&config.name) {
                Arc::new(WebhookHook::new_with_async(config.clone(), batcher.clone()))
            } else {
                Arc::new(WebhookHook::new(config.clone()))
            };

            // Webhook 实现所有 Hook trait，根据 trigger 注册到所有对应位置
            registry.register_on_message_received_hook(webhook.clone());
            registry.register_on_turn_start_hook(webhook.clone());
            registry.register_on_tool_call_start_hook(webhook.clone());
            registry.register_pre_tool_hook(webhook.clone());
            registry.register_post_tool_hook(webhook.clone());
            registry.register_on_turn_complete_hook(webhook.clone());
            registry.register_post_turn_hook(webhook.clone());
            registry.register_on_session_start_hook(webhook.clone());
            registry.register_on_session_end_hook(webhook.clone());
            registry.register_on_error_hook(webhook.clone());
            registry.register_on_model_response_hook(webhook.clone());
            registry.register_system_prompt_hook(webhook.clone());

            eprintln!("[Webhook] Registered: {} -> {}", config.name, config.url);
        }

        // 存储异步注册表以便后续关闭
        if !async_registry.batchers.is_empty() {
            eprintln!("[AsyncWebhook] Registered {} async batchers", async_registry.batchers.len());
        }
    }
}

/// 从默认位置加载 hooks
/// 优先级：全局 hooks > 项目级 hooks
pub fn load_hooks(registry: &mut HookRegistry) {
    // 1. 全局 hooks: ~/.atomcode/hooks/
    if let Some(home) = dirs::home_dir() {
        let global_hooks_dir = home.join(".atomcode").join("hooks");
        if global_hooks_dir.exists() {
            if let Ok(config) = HooksConfig::from_dir(&global_hooks_dir) {
                config.register_hooks(registry, &global_hooks_dir);
                eprintln!("[Hook] Loaded hooks from {}", global_hooks_dir.display());
            }
        }
    }

    // 2. 项目级 hooks: <cwd>/.atomcode/hooks/
    if let Ok(cwd) = std::env::current_dir() {
        let project_hooks_dir = cwd.join(".atomcode").join("hooks");
        if project_hooks_dir.exists() {
            if let Ok(config) = HooksConfig::from_dir(&project_hooks_dir) {
                config.register_hooks(registry, &project_hooks_dir);
                eprintln!("[Hook] Loaded project hooks from {}", project_hooks_dir.display());
            }
        }
    }
}

/// 从指定目录加载 hooks（用于 CLI --hooks-dir 参数）
pub fn load_hooks_from_dir(registry: &mut HookRegistry, dir: &Path) {
    if dir.exists() {
        if let Ok(config) = HooksConfig::from_dir(dir) {
            config.register_hooks(registry, dir);
            eprintln!("[Hook] Loaded hooks from {}", dir.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::path::PathBuf;
    use crate::hook::HookRegistry;

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
        fs::write(&config_path, r#"
[[hooks]]
name = "test-hook"
trigger = "pre_tool"
script = "test.sh"
enabled = true
timeout_secs = 3
"#).expect("Should write test file");

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
        fs::write(dir.join("hooks.toml"), r#"
[[hooks]]
name = "dir-hook"
trigger = "post_turn"
script = "report.sh"
"#).expect("Should write test file");

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

    // ── register_hooks ─────────────────────────────────────────────

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

    #[test]
    fn test_register_hooks_pre_tool() {
        let config = HooksConfig {
            hooks: vec![make_script_config("pre", "pre_tool", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut registry = HookRegistry::new();
        config.register_hooks(&mut registry, Path::new("/tmp"));

        let stats = registry.stats();
        assert_eq!(stats.pre_tool_hooks, 1);
    }

    #[test]
    fn test_register_hooks_post_tool() {
        let config = HooksConfig {
            hooks: vec![make_script_config("post", "post_tool", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut registry = HookRegistry::new();
        config.register_hooks(&mut registry, Path::new("/tmp"));

        let stats = registry.stats();
        assert_eq!(stats.post_tool_hooks, 1);
    }

    #[test]
    fn test_register_hooks_post_turn() {
        let config = HooksConfig {
            hooks: vec![make_script_config("turn", "post_turn", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut registry = HookRegistry::new();
        config.register_hooks(&mut registry, Path::new("/tmp"));

        let stats = registry.stats();
        assert_eq!(stats.post_turn_hooks, 1);
    }

    #[test]
    fn test_register_hooks_system_prompt() {
        let config = HooksConfig {
            hooks: vec![make_script_config("sys", "system_prompt", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut registry = HookRegistry::new();
        config.register_hooks(&mut registry, Path::new("/tmp"));

        let stats = registry.stats();
        assert_eq!(stats.system_prompt_hooks, 1);
    }

    #[test]
    fn test_register_hooks_unknown_trigger() {
        let config = HooksConfig {
            hooks: vec![make_script_config("bad", "unknown_trigger", "/bin/echo")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut registry = HookRegistry::new();
        config.register_hooks(&mut registry, Path::new("/tmp"));

        let stats = registry.stats();
        // Should not be registered under any known trigger
        assert_eq!(stats.pre_tool_hooks, 0);
        assert_eq!(stats.post_tool_hooks, 0);
        assert_eq!(stats.post_turn_hooks, 0);
        assert_eq!(stats.system_prompt_hooks, 0);
    }

    #[test]
    fn test_register_hooks_disabled_skipped() {
        let mut config = make_script_config("disabled", "pre_tool", "/bin/echo");
        config.enabled = false;
        let hooks_config = HooksConfig {
            hooks: vec![config],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut registry = HookRegistry::new();
        hooks_config.register_hooks(&mut registry, Path::new("/tmp"));

        let stats = registry.stats();
        assert_eq!(stats.pre_tool_hooks, 0);
    }

    #[test]
    fn test_register_hooks_nonexistent_script() {
        let config = HooksConfig {
            hooks: vec![make_script_config("missing", "pre_tool", "/tmp/nonexistent_script_12345.sh")],
            webhooks: vec![],
            async_webhooks: vec![],
        };
        let mut registry = HookRegistry::new();
        config.register_hooks(&mut registry, Path::new("/tmp"));

        let stats = registry.stats();
        assert_eq!(stats.pre_tool_hooks, 0);
    }

    // ── load_hooks_from_dir ─────────────────────────────────────────

    #[test]
    fn test_load_hooks_from_dir_valid() {
        let dir = std::env::temp_dir().join(format!("hook_load_test_{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        fs::write(dir.join("hooks.toml"), r#"
[[hooks]]
name = "loaded-hook"
trigger = "pre_tool"
script = "/bin/echo"
"#).expect("Should write test file");

        let mut registry = HookRegistry::new();
        load_hooks_from_dir(&mut registry, &dir);

        let stats = registry.stats();
        assert_eq!(stats.pre_tool_hooks, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_hooks_from_dir_nonexistent() {
        let mut registry = HookRegistry::new();
        load_hooks_from_dir(&mut registry, Path::new("/tmp/nonexistent_hooks_dir_12345"));

        let stats = registry.stats();
        assert_eq!(stats.pre_tool_hooks, 0);
    }
}
