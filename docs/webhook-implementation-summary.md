# Webhook 支持实现总结

## 概述

已成功实现 Webhook 支持，允许通过 HTTP 远程调用 Hook，实现与外部系统的集成。

## 实现内容

### 1. 核心模块

**文件**: `crates/atomcode-core/src/hook/webhook.rs` (~748 行)

#### 主要结构

```rust
pub struct WebhookConfig {
    pub name: String,              // Webhook 名称
    pub trigger: String,           // 触发时机
    pub url: String,               // Webhook URL
    pub method: String,            // HTTP 方法（默认 POST）
    pub headers: HashMap<String, String>,  // 自定义 Header
    pub timeout_secs: u64,         // 超时时间（默认 10 秒）
    pub retries: u32,              // 重试次数（默认 2 次）
    pub enabled: bool,             // 是否启用
    pub description: String,       // 描述
}

pub struct WebhookHook {
    config: WebhookConfig,
    client: Client,                // reqwest HTTP 客户端
}
```

#### 功能特性

1. **HTTP 客户端**
   - 使用 `reqwest` 库（项目已有依赖）
   - 支持超时控制（默认 10 秒）
   - 自动重试（指数退避，默认 2 次）
   - 自定义 Header 支持（如认证 Token）

2. **请求格式**
   ```json
   {
     "hook_name": "webhook-name",
     "trigger": "post_tool",
     "event": "post_tool_execution",
     "context": { ... }
   }
   ```

3. **响应格式**
   ```json
   {
     "result": "ok|warning|deny|modify",
     "message": "Optional message",
     "modified_content": "Optional modified content"
   }
   ```

4. **支持所有 Hook 时机（12 个）**
   - ✅ OnTurnStart
   - ✅ OnToolCallStart
   - ✅ PreToolExecution
   - ✅ PostToolExecution
   - ✅ OnTurnComplete
   - ✅ PostTurn
   - ✅ OnSessionStart
   - ✅ OnSessionEnd
   - ✅ OnError
   - ✅ OnModelResponse
   - ✅ SystemPrompt
   - ✅ OnMessageReceived

### 2. 配置加载

**文件**: `crates/atomcode-core/src/hook/config_loader.rs` (更新)

#### hooks.toml 格式

```toml
[[webhooks]]
name = "slack-notify"
description = "发送通知到 Slack"
trigger = "post_tool"
url = "https://hooks.slack.com/services/XXX"
enabled = true
timeout_secs = 10
retries = 2

# 自定义 Header（可选）
[webhooks.headers]
Authorization = "Bearer YOUR_TOKEN"
```

#### 加载逻辑

- 从 `hooks.toml` 的 `[[webhooks]]` 数组加载
- 根据 trigger 字段匹配注册到对应 HookEngine 槽位
- 根据 `trigger` 字段过滤实际触发的事件

### 3. 文件变更

| 文件 | 变更 | 行数 |
|------|------|------|
| `src/hook/webhook.rs` | 新增 | ~748 行 |
| `src/hook/config_loader.rs` | 更新 | +40 行 |
| `src/hook/mod.rs` | 更新 | +1 行 |
| `tests/webhook_test.rs` | 新增 | 78 行 |
| `docs/webhook-guide.md` | 新增 | 450 行 |

### 4. 测试验证

```
running 3 tests
✓ test_webhook_config_defaults     - 配置默认值测试
✓ test_webhook_disabled            - 禁用状态测试
✓ test_webhook_name_and_description - 名称和描述测试

test result: ok. 3 passed; 0 failed
```

### 5. 使用示例

#### 示例 1：Slack 通知

```toml
[[webhooks]]
name = "slack-audit"
description = "记录所有工具调用到 Slack"
trigger = "tool_call_start"
url = "https://hooks.slack.com/services/YOUR/SLACK/WEBHOOK"
enabled = true
timeout_secs = 10
```

#### 示例 2：钉钉通知

```toml
[[webhooks]]
name = "dingtalk"
description = "发送通知到钉钉"
trigger = "turn_complete"
url = "https://oapi.dingtalk.com/robot/send?access_token=XXX"
enabled = true
```

#### 示例 3：企业微信

```toml
[[webhooks]]
name = "wechat"
description = "发送通知到企业微信"
trigger = "error"
url = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=XXX"
enabled = true
```

#### 示例 4：云端审计日志

```toml
[[webhooks]]
name = "audit-log"
description = "发送所有工具调用审计日志"
trigger = "tool_call_start"
url = "https://log-service.example.com/atomcode/audit"
enabled = true
timeout_secs = 5
retries = 3

[webhooks.headers]
Authorization = "Bearer AUDIT_TOKEN"
```

### 6. 特性对比

| 特性 | 脚本 Hook | Webhook Hook |
|------|----------|-------------|
| 执行方式 | 本地进程 | HTTP 请求 |
| 延迟 | 低（~10ms） | 中（10-200ms） |
| 依赖 | 本地环境 | 网络连接 |
| 适用场景 | 本地脚本、快速原型 | 远程服务、云端集成 |
| 超时控制 | ✅ | ✅ |
| 重试机制 | ❌ | ✅ |
| 自定义 Header | ❌ | ✅ |
| 认证支持 | 文件系统权限 | HTTP Header |

### 7. 安全机制

1. **HTTPS 优先** - 建议使用 HTTPS 加密传输
2. **认证 Header** - 支持自定义 Header 传递认证信息
3. **超时保护** - 默认 10 秒，避免长时间阻塞
4. **重试限制** - 默认 2 次，指数退避策略
5. **错误处理** - Webhook 失败不中断主流程（Warning 级别）

### 8. 性能影响

| 场景 | 延迟 | 说明 |
|------|------|------|
| 本地网络 | 1-5ms | 几乎无影响 |
| 同区域云服务 | 10-50ms | 可接受 |
| 跨区域 | 50-200ms | 建议异步处理 |
| 超时 | 10-30s | 会阻塞流程 |

**建议**：Webhook 服务端应该快速响应（< 1 秒），避免阻塞 AtomCode 流程。

### 9. 调试方法

#### 使用 webhook.site 测试

1. 访问 https://webhook.site
2. 复制生成的唯一 URL
3. 配置：
   ```toml
   [[webhooks]]
   name = "debug"
   trigger = "tool_call_start"
   url = "https://webhook.site/your-unique-id"
   enabled = true
   ```
4. 运行 AtomCode，查看 webhook.site 收到的请求

#### 查看日志

```bash
atomcode -p "test" 2>&1 | grep -i webhook
```

输出示例：
```
[Webhook] Registered: slack-notify -> https://hooks.slack.com/services/XXX
[Hook Warning] slack-notify: HTTP 500 at attempt 1: Internal Server Error
```

### 10. 完整配置示例

```toml
# ~/.atomcode/hooks/hooks.toml

# 脚本 Hook
[[hooks]]
name = "local-audit"
trigger = "post_tool"
script = "audit.sh"
script_type = "shell"
enabled = true

# Webhook Hook - Slack
[[webhooks]]
name = "slack-notify"
description = "发送工具调用通知到 Slack"
trigger = "tool_call_start"
url = "https://hooks.slack.com/services/XXX"
enabled = true
timeout_secs = 5

# Webhook Hook - 钉钉
[[webhooks]]
name = "dingtalk"
description = "Turn 完成通知"
trigger = "turn_complete"
url = "https://oapi.dingtalk.com/robot/send?access_token=XXX"
enabled = true

# Webhook Hook - 云端审计
[[webhooks]]
name = "audit-log"
description = "发送所有工具调用到云端"
trigger = "tool_call_start"
url = "https://log-service.example.com/audit"
enabled = true
retries = 3

[webhooks.headers]
Authorization = "Bearer AUDIT_TOKEN"
```

## 相关文档

- [Webhook 使用指南](./webhook-guide.md) - 完整的使用文档
- [Hook 系统总览](./hooks.md) - Hook 系统概述
- [完整 Hook 时机列表](./hook-timing-complete.md) - 所有 Hook 时机
- [CLI 使用指南](./hook-cli-guide.md) - CLI 命令使用

## 总结

### 完成的工作

1. ✅ **实现 Webhook Hook 核心模块** - ~748 行 Rust 代码
2. ✅ **支持所有 12 个 Hook 时机** - 完整的 HTTP 远程调用
3. ✅ **实现超时和重试机制** - 指数退避策略
4. ✅ **支持自定义 Header** - 认证和元数据传递
5. ✅ **集成配置加载系统** - hooks.toml 配置驱动
6. ✅ **编写完整文档** - 使用指南 + 示例
7. ✅ **编写测试** - 3 个单元测试全部通过

### Webhook 系统现在提供

- **HTTP 远程调用** - 与外部系统集成
- **超时控制** - 默认 10 秒
- **自动重试** - 指数退避，默认 2 次
- **自定义 Header** - 支持认证和元数据
- **所有 Hook 时机** - 12 个扩展点
- **错误处理** - 失败不中断主流程

### 下一步建议

1. **异步 Webhook** - 使用后台任务避免阻塞
2. **批量发送** - 聚合多个事件为一次 HTTP 请求
3. **Webhook 签名** - 验证请求来源
4. **Webhook 市场** - 预配置的 Webhook 模板
