use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{timeout, Duration};

use crate::hook::{
    Hook, HookCtx, HookResult, PreToolExecutionHook, PostToolExecutionHook,
    PostTurnHook, SystemPromptHook, ToolResultContext,
};

/// 脚本 Hook 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptHookConfig {
    /// Hook 名称
    pub name: String,
    /// 触发时机: "pre_tool", "post_tool", "post_turn", "system_prompt"
    pub trigger: String,
    /// 脚本路径
    pub script: PathBuf,
    /// 脚本类型: "shell" 或 "python"
    #[serde(default = "default_script_type")]
    pub script_type: String,
    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 超时时间（秒）
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Hook 描述
    #[serde(default)]
    pub description: String,
}

fn default_script_type() -> String {
    "shell".to_string()
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    2
}

/// 脚本 Hook 实现
pub struct ScriptHook {
    config: ScriptHookConfig,
}

impl ScriptHook {
    pub fn new(config: ScriptHookConfig) -> Self {
        Self { config }
    }

    /// 执行脚本并获取结果
    async fn run_script(&self, input_json: &str) -> Result<String, String> {
        let script_path = &self.config.script;
        
        // 检查脚本是否存在
        if !script_path.exists() {
            return Err(format!("Script not found: {}", script_path.display()));
        }

        // 构建命令
        let (cmd, args) = match self.config.script_type.as_str() {
            "python" => ("python", vec![script_path.to_string_lossy().to_string()]),
            "shell" | "bash" => {
                if cfg!(windows) {
                    ("cmd", vec!["/C".to_string(), script_path.to_string_lossy().to_string()])
                } else {
                    ("sh", vec![script_path.to_string_lossy().to_string()])
                }
            }
            _ => return Err(format!("Unsupported script type: {}", self.config.script_type)),
        };

        // 启动子进程
        let mut cmd_builder = tokio::process::Command::new(cmd);
        cmd_builder
            .args(&args)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::process_utils::suppress_console_window(&mut cmd_builder);
        let mut child = cmd_builder
            .spawn()
            .map_err(|e| format!("Failed to spawn script: {}", e))?;

        // 写入输入
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(input_json.as_bytes())
                .await
                .map_err(|e| format!("Failed to write to script: {}", e))?;
        }

        // 等待输出（带超时）
        let result = timeout(
            Duration::from_secs(self.config.timeout_secs),
            Self::wait_for_output(&mut child),
        )
        .await
        .map_err(|_| "Script execution timed out".to_string())?;

        result
    }

    async fn wait_for_output(child: &mut tokio::process::Child) -> Result<String, String> {
        // Read raw bytes (not read_to_string, which hard-errors on non-UTF-8):
        // a Chinese-Windows script's stderr arrives as CP936/GBK, so decode
        // via the OEM-aware helper instead of assuming UTF-8.
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        if let Some(ref mut out) = child.stdout {
            out.read_to_end(&mut stdout_buf)
                .await
                .map_err(|e| format!("Failed to read stdout: {}", e))?;
        }

        if let Some(ref mut err) = child.stderr {
            err.read_to_end(&mut stderr_buf)
                .await
                .map_err(|e| format!("Failed to read stderr: {}", e))?;
        }

        let stdout = crate::process_utils::decode_subprocess_output(&stdout_buf);
        let stderr = crate::process_utils::decode_subprocess_output(&stderr_buf);

        let status = child
            .wait()
            .await
            .map_err(|e| format!("Script failed: {}", e))?;

        if !status.success() {
            return Err(format!("Script exited with status {}: {}", status, stderr));
        }

        Ok(stdout.trim().to_string())
    }

    /// 解析脚本输出为 HookResult
    fn parse_output(&self, output: &str) -> HookResult {
        // 脚本输出格式：JSON 或简单的字符串
        // 支持格式：
        // - "ok" 或空字符串 -> HookResult::Ok
        // - "warning: <msg>" -> HookResult::Warning
        // - "deny: <reason>" -> HookResult::Denied
        // - "modify: <new_args>" -> HookResult::Modified
        // - 任意 JSON 对象 { "result": "ok|warning|deny|modify", "message": "..." }

        let output = output.trim();
        if output.is_empty() {
            return HookResult::Ok;
        }

        // 尝试解析 JSON
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(output) {
            if let Some(result) = json.get("result").and_then(|v| v.as_str()) {
                let message = json
                    .get("message")
                    .or_else(|| json.get("reason"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                return match result {
                    "ok" => HookResult::Ok,
                    "warning" => HookResult::Warning(message),
                    "deny" => HookResult::Denied(message),
                    "modify" => HookResult::Modified(message),
                    _ => HookResult::Warning(format!("Unknown result: {}", result)),
                };
            }
        }

        // 解析简单字符串格式
        if output.starts_with("warning:") || output.starts_with("WARN:") {
            return HookResult::Warning(output.splitn(2, ':').nth(1).unwrap_or("").trim().to_string());
        }
        if output.starts_with("deny:") || output.starts_with("DENY:") {
            return HookResult::Denied(output.splitn(2, ':').nth(1).unwrap_or("").trim().to_string());
        }
        if output.starts_with("modify:") || output.starts_with("MODIFY:") {
            return HookResult::Modified(output.splitn(2, ':').nth(1).unwrap_or("").trim().to_string());
        }

        // 默认：视为成功
        HookResult::Ok
    }
}

impl Hook for ScriptHook {
    fn name(&self) -> &str {
        &self.config.name
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    fn is_enabled(&self) -> bool {
        self.config.enabled
    }
}

#[async_trait]
impl PreToolExecutionHook for ScriptHook {
    async fn on_pre_execute(&self, ctx: &HookCtx) -> HookResult {
        let input = serde_json::to_string(ctx).unwrap_or_default();
        match self.run_script(&input).await {
            Ok(output) => self.parse_output(&output),
            Err(e) => HookResult::Warning(format!("Script error: {}", e)),
        }
    }
}

#[async_trait]
impl PostToolExecutionHook for ScriptHook {
    async fn on_post_execute(&self, ctx: &HookCtx, result_ctx: &ToolResultContext) -> HookResult {
        let mut combined = serde_json::Map::new();
        combined.insert("hook_context".to_string(), serde_json::to_value(ctx).unwrap_or_default());
        combined.insert("result_context".to_string(), serde_json::to_value(result_ctx).unwrap_or_default());
        
        let input = serde_json::to_string(&combined).unwrap_or_default();
        match self.run_script(&input).await {
            Ok(output) => self.parse_output(&output),
            Err(e) => HookResult::Warning(format!("Script error: {}", e)),
        }
    }
}

#[async_trait]
impl PostTurnHook for ScriptHook {
    async fn on_post_turn(&self, ctx: &HookCtx, turn_result: &str) -> HookResult {
        let mut combined = serde_json::Map::new();
        combined.insert("hook_context".to_string(), serde_json::to_value(ctx).unwrap_or_default());
        combined.insert("turn_result".to_string(), serde_json::Value::String(turn_result.to_string()));
        
        let input = serde_json::to_string(&combined).unwrap_or_default();
        match self.run_script(&input).await {
            Ok(output) => self.parse_output(&output),
            Err(e) => HookResult::Warning(format!("Script error: {}", e)),
        }
    }
}

#[async_trait]
impl SystemPromptHook for ScriptHook {
    async fn extend_system_prompt(&self) -> Option<String> {
        let empty_ctx = HookCtx::new("".to_string(), "".to_string(), "".to_string());
        let input = serde_json::to_string(&empty_ctx).unwrap_or_default();
        
        match self.run_script(&input).await {
            Ok(output) if !output.is_empty() => Some(output),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::Hook;

    // ── ScriptHookConfig defaults ──

    #[test]
    fn config_default_script_type() {
        // Test serde defaults by deserializing from minimal JSON
        let json = r#"{"name":"test","trigger":"pre_tool","script":"test.sh"}"#;
        let config: ScriptHookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.script_type, "shell");
    }

    #[test]
    fn config_default_enabled() {
        let json = r#"{"name":"test","trigger":"pre_tool","script":"test.sh"}"#;
        let config: ScriptHookConfig = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
    }

    #[test]
    fn config_default_timeout() {
        let json = r#"{"name":"test","trigger":"pre_tool","script":"test.sh"}"#;
        let config: ScriptHookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.timeout_secs, 2);
    }

    // ── ScriptHook construction and trait methods ──

    #[test]
    fn script_hook_new_and_trait_methods() {
        let config = ScriptHookConfig {
            name: "my-hook".into(),
            trigger: "post_tool".into(),
            script: PathBuf::from("/tmp/dummy.sh"),
            script_type: "shell".into(),
            enabled: true,
            timeout_secs: 5,
            description: "My test hook".into(),
        };
        let hook = ScriptHook::new(config);

        assert_eq!(hook.name(), "my-hook");
        assert_eq!(hook.description(), "My test hook");
        assert!(hook.is_enabled());
    }

    #[test]
    fn script_hook_disabled() {
        let config = ScriptHookConfig {
            name: "disabled-hook".into(),
            trigger: "pre_tool".into(),
            script: PathBuf::from("ignored.sh"),
            script_type: "shell".into(),
            enabled: false,
            timeout_secs: 2,
            description: String::new(),
        };
        let hook = ScriptHook::new(config);
        assert!(!hook.is_enabled());
    }

    // ── parse_output ──

    #[test]
    fn parse_output_empty() {
        let config = ScriptHookConfig {
            name: "t".into(),
            trigger: "pre_tool".into(),
            script: PathBuf::from("t.sh"),
            script_type: "shell".into(),
            enabled: true,
            timeout_secs: 2,
            description: String::new(),
        };
        let hook = ScriptHook::new(config);
        assert!(matches!(hook.parse_output(""), HookResult::Ok));
    }

    #[test]
    fn parse_output_ok() {
        let hook = ScriptHook::new(ScriptHookConfig {
            name: "t".into(),
            trigger: "pre_tool".into(),
            script: PathBuf::from("t.sh"),
            script_type: "shell".into(),
            enabled: true,
            timeout_secs: 2,
            description: String::new(),
        });
        assert!(matches!(hook.parse_output("ok"), HookResult::Ok));
    }

    #[test]
    fn parse_output_warning() {
        let hook = ScriptHook::new(ScriptHookConfig {
            name: "t".into(),
            trigger: "pre_tool".into(),
            script: PathBuf::from("t.sh"),
            script_type: "shell".into(),
            enabled: true,
            timeout_secs: 2,
            description: String::new(),
        });
        let result = hook.parse_output("warning: something");
        assert!(matches!(result, HookResult::Warning(msg) if msg == "something"));
    }

    #[test]
    fn parse_output_deny() {
        let hook = ScriptHook::new(ScriptHookConfig {
            name: "t".into(),
            trigger: "pre_tool".into(),
            script: PathBuf::from("t.sh"),
            script_type: "shell".into(),
            enabled: true,
            timeout_secs: 2,
            description: String::new(),
        });
        let result = hook.parse_output("deny: access denied");
        assert!(matches!(result, HookResult::Denied(msg) if msg == "access denied"));
    }

    #[test]
    fn parse_output_modify() {
        let hook = ScriptHook::new(ScriptHookConfig {
            name: "t".into(),
            trigger: "pre_tool".into(),
            script: PathBuf::from("t.sh"),
            script_type: "shell".into(),
            enabled: true,
            timeout_secs: 2,
            description: String::new(),
        });
        let result = hook.parse_output("modify: new_args");
        assert!(matches!(result, HookResult::Modified(msg) if msg == "new_args"));
    }

    #[test]
    fn parse_output_json_ok() {
        let hook = ScriptHook::new(ScriptHookConfig {
            name: "t".into(),
            trigger: "pre_tool".into(),
            script: PathBuf::from("t.sh"),
            script_type: "shell".into(),
            enabled: true,
            timeout_secs: 2,
            description: String::new(),
        });
        let result = hook.parse_output(r#"{"result":"ok"}"#);
        assert!(matches!(result, HookResult::Ok));
    }

    #[test]
    fn parse_output_json_warning() {
        let hook = ScriptHook::new(ScriptHookConfig {
            name: "t".into(),
            trigger: "pre_tool".into(),
            script: PathBuf::from("t.sh"),
            script_type: "shell".into(),
            enabled: true,
            timeout_secs: 2,
            description: String::new(),
        });
        let result = hook.parse_output(r#"{"result":"warning","message":"be careful"}"#);
        assert!(matches!(result, HookResult::Warning(msg) if msg == "be careful"));
    }

    #[test]
    fn parse_output_unrecognized_fallback_to_ok() {
        let hook = ScriptHook::new(ScriptHookConfig {
            name: "t".into(),
            trigger: "pre_tool".into(),
            script: PathBuf::from("t.sh"),
            script_type: "shell".into(),
            enabled: true,
            timeout_secs: 2,
            description: String::new(),
        });
        // Unrecognized text should fall back to Ok
        let result = hook.parse_output("some random output");
        assert!(matches!(result, HookResult::Ok));
    }

    // ── ScriptHook implements Hook trait ──

    #[test]
    fn script_hook_impl_hook_trait() {
        fn require_hook<T: Hook>() {}
        require_hook::<ScriptHook>();
    }
}
