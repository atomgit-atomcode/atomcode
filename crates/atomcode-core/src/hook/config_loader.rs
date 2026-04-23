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
