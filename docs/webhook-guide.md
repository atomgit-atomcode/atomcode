# Webhook Hook 使用指南

## 概述

Webhook Hook 允许你通过 HTTP 远程调用 Hook，实现：
- 远程审计和日志记录
- 与外部系统集成（如 Slack、钉钉、企业微信）
- 云端分析和监控
- 自定义服务端逻辑

## 配置示例

### 基本配置

在 `~/.atomcode/hooks/hooks.toml` 中添加：

```toml
[[webhooks]]
name = "slack-notify"
description = "发送工具调用通知到 Slack"
trigger = "post_tool"
url = "https://hooks.slack.com/services/YOUR/SLACK/WEBHOOK"
enabled = true
timeout_secs = 10
retries = 2
```

### 带认证的 Webhook

```toml
[[webhooks]]
name = "custom-api"
description = "发送到自定义 API"
trigger = "tool_call_start"
url = "https://api.example.com/atomcode/hooks"
method = "POST"
enabled = true
timeout_secs = 15
retries = 3

# 自定义 Header
[webhooks.headers]
Authorization = "Bearer YOUR_TOKEN"
X-Custom-Header = "value"
```

### 多个 Webhook

```toml
# Slack 通知
[[webhooks]]
name = "slack"
trigger = "post_tool"
url = "https://hooks.slack.com/services/XXX"
enabled = true

# 钉钉通知
[[webhooks]]
name = "dingtalk"
trigger = "turn_complete"
url = "https://oapi.dingtalk.com/robot/send?access_token=XXX"
enabled = true

# 企业微信
[[webhooks]]
name = "wechat"
trigger = "session_end"
url = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=XXX"
enabled = true
```

## 支持的触发时机

Webhook 支持除 OnUserPromptSubmit 外的所有 Hook 时机（共 12 种），trigger 字段可以包含以下值（逗号分隔多个）：

| trigger 值（规范） | 别名 | 触发时机 |
|-----------|------|---------|
| `turn_start` | — | Turn 开始前 |
| `tool_call_start` | — | 工具调用开始时 |
| `pre_tool` | `before_tool` | 工具执行前 |
| `post_tool` | `after_tool` | 工具执行后 |
| `turn_complete` | `after_turn` | Turn 完成后（详细统计） |
| `post_turn` | — | Turn 完成后（旧版兼容） |
| `session_start` | — | 会话启动时 |
| `session_end` | — | 会话结束时 |
| `error` | — | 错误发生时 |
| `model_response` | — | 模型响应完成后 |
| `system_prompt` | — | 系统 Prompt 构建时 |
| `message`² | `message_received` | 用户消息接收时 |

> ² `message`：WebhookHook 已实现对应 trait，但引擎尚未注册触发槽位，当前实际不可用。
>
> **匹配规则**：使用 **contains（子串包含）** 匹配，因此 `on_turn_start`、`turn_start_hook` 等变体也能工作。但推荐使用上表的规范值，避免歧义。例如 `trigger = "error"` 会匹配所有含 "error" 的 trigger 字符串，如果同时写了 `trigger = "pre_tool,error"`，则该 Webhook 也会在错误事件触发。
>
> **注意**：Webhook 会注册到所有触发时机，但只会在 `trigger` 字段匹配时实际发送 HTTP 请求。

## 请求格式

Webhook 发送的 JSON 请求格式：

```json
{
  "hook_name": "slack-notify",
  "trigger": "post_tool",
  "event": "post_tool_execution",
  "hook_context": {
    "tool_name": "edit_file",
    "tool_args": "{...}",
    "working_dir": "/path/to/project",
    "session_id": "session-123",
    "turn_number": 5
  },
  "result_context": {
    "tool_name": "edit_file",
    "tool_args": "{...}",
    "result": "File updated",
    "success": true,
    "duration_ms": 150
  }
}
```

不同事件的 payload 结构略有不同，但都包含：
- `hook_name` - Webhook 名称
- `trigger` - 配置的触发条件
- `event` - 实际事件类型
- `context` - 上下文数据

## 响应格式

Webhook 服务端应该返回 JSON 响应：

```json
{
  "result": "ok",
  "message": "Optional message",
  "modified_content": "Optional modified content"
}
```

### result 字段

| 值 | 行为 |
|---|------|
| `ok` | 继续执行 |
| `warning` | 记录警告，继续执行 |
| `deny` | 拒绝执行（仅 pre-tool 和 tool-call-start） |
| `modify` | 使用 `modified_content` 作为新参数 |

## 示例：Slack 集成

### 1. 创建 Slack Webhook

在 Slack 中创建 Incoming Webhook：
1. 访问 https://slack.com/apps
2. 搜索 "Incoming Webhooks"
3. 添加到你的工作区
4. 复制 Webhook URL

### 2. 配置 AtomCode

```toml
[[webhooks]]
name = "slack-audit"
description = "记录所有工具调用到 Slack"
trigger = "tool_call_start"
url = "https://hooks.slack.com/services/YOUR/SLACK/WEBHOOK"
enabled = true
timeout_secs = 10
```

### 3. 创建 Slack 适配器（可选）

如果你的 Slack Webhook 需要特定格式，可以创建一个中间服务：

```python
# webhook_adapter.py
from flask import Flask, request, jsonify
import requests

app = Flask(__name__)

SLACK_WEBHOOK = "https://hooks.slack.com/services/YOUR/SLACK/WEBHOOK"

@app.route('/atomcode/hooks', methods=['POST'])
def handle_hook():
    data = request.json
    
    # 格式化为 Slack 消息
    slack_payload = {
        "text": f"🔔 AtomCode Hook\n"
                f"Event: `{data['event']}`\n"
                f"Tool: `{data.get('hook_context', {}).get('tool_name', 'N/A')}`\n"
                f"Turn: `{data.get('hook_context', {}).get('turn_number', 'N/A')}`"
    }
    
    # 发送到 Slack
    requests.post(SLACK_WEBHOOK, json=slack_payload)
    
    # 返回 AtomCode 期望的响应
    return jsonify({"result": "ok"})

if __name__ == '__main__':
    app.run(port=5000)
```

配置：

```toml
[[webhooks]]
name = "slack-via-adapter"
trigger = "tool_call_start"
url = "http://localhost:5000/atomcode/hooks"
enabled = true
```

## 示例：钉钉集成

### 配置

```toml
[[webhooks]]
name = "dingtalk"
description = "发送通知到钉钉"
trigger = "turn_complete"
url = "https://oapi.dingtalk.com/robot/send?access_token=XXX"
enabled = true
```

### 钉钉适配器

```python
from flask import Flask, request, jsonify
import requests

app = Flask(__name__)

DINGTALK_WEBHOOK = "https://oapi.dingtalk.com/robot/send?access_token=XXX"

@app.route('/atomcode/hooks', methods=['POST'])
def handle_hook():
    data = request.json
    
    # 格式化为钉钉消息
    dingtalk_payload = {
        "msgtype": "markdown",
        "markdown": {
            "title": "AtomCode Hook",
            "text": f"### AtomCode Hook\n"
                    f"- 事件: `{data['event']}`\n"
                    f"- 工具: `{data.get('hook_context', {}).get('tool_name', 'N/A')}`\n"
                    f"- Turn: `{data.get('hook_context', {}).get('turn_number', 'N/A')}`"
        }
    }
    
    requests.post(DINGTALK_WEBHOOK, json=dingtalk_payload)
    return jsonify({"result": "ok"})

if __name__ == '__main__':
    app.run(port=5000)
```

## 示例：企业微信集成

### 配置

```toml
[[webhooks]]
name = "wechat"
description = "发送通知到企业微信"
trigger = "error"
url = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=XXX"
enabled = true
```

## 示例：云端审计日志

### 发送到日志服务

```toml
[[webhooks]]
name = "audit-log"
description = "发送所有工具调用审计日志"
trigger = "tool_call_start"
url = "https://log-service.example.com/atomcode/audit"
method = "POST"
enabled = true
timeout_secs = 5
retries = 3

[webhooks.headers]
Authorization = "Bearer AUDIT_TOKEN"
Content-Type = "application/json"
```

## 示例：模型响应验证

### 远程验证服务

```toml
[[webhooks]]
name = "response-validator"
description = "远程验证模型响应"
trigger = "model_response"
url = "https://validator.example.com/check"
enabled = true
timeout_secs = 10
```

服务端可以检查：
- 是否包含敏感信息（API Key、密码等）
- 代码是否符合编码规范
- 是否包含潜在的安全问题

## 超时和重试

### 超时控制

```toml
[[webhooks]]
name = "slow-service"
timeout_secs = 30  # 30 秒超时
enabled = true
```

**默认超时**：10 秒  
**建议超时**：5-15 秒（根据服务响应时间调整）

### 自动重试

```toml
[[webhooks]]
name = "unreliable-service"
retries = 5  # 最多重试 5 次
enabled = true
```

**默认重试**：2 次  
**重试策略**：指数退避（100ms, 200ms, 400ms, ...）

## 故障排查

### Webhook 未发送

1. **检查 enabled 字段**：
   ```toml
   enabled = true  # 必须是 true
   ```

2. **检查 URL 是否正确**：
   ```bash
   curl -X POST https://your-webhook-url \
        -H "Content-Type: application/json" \
        -d '{"test": true}'
   ```

3. **查看 stderr 输出**：
   ```bash
   atomcode -p "test" 2>&1 | grep -i webhook
   ```

### Webhook 返回错误

Webhook 错误会显示为警告，不会中断流程：

```
[Hook Warning] slack-notify: HTTP 500 at attempt 1: Internal Server Error
```

### 调试 Webhook

使用 [webhook.site](https://webhook.site) 测试：

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

## 安全注意事项

1. **使用 HTTPS** - 避免明文传输敏感数据
2. **添加认证 Header** - 防止未授权访问
3. **验证服务端** - 确保 Webhook URL 可信
4. **限制超时** - 避免长时间阻塞
5. **监控重试** - 频繁重试可能表示服务问题

## 性能影响

| 场景 | 延迟 | 说明 |
|------|------|------|
| 本地网络 | 1-5ms | 几乎无影响 |
| 同区域云服务 | 10-50ms | 可接受 |
| 跨区域 | 50-200ms | 建议异步处理 |
| 超时 | 10-30s | 会阻塞流程 |

**建议**：Webhook 服务端应该快速响应（< 1 秒），避免阻塞 AtomCode 流程。

## 完整示例

### hooks.toml

```toml
# Slack 工具调用通知
[[webhooks]]
name = "slack-tools"
description = "Slack 工具调用通知"
trigger = "post_tool"
url = "https://hooks.slack.com/services/XXX"
enabled = true
timeout_secs = 5

# 钉钉 Turn 完成通知
[[webhooks]]
name = "dingtalk-turns"
description = "钉钉 Turn 完成通知"
trigger = "turn_complete"
url = "https://oapi.dingtalk.com/robot/send?access_token=XXX"
enabled = true
timeout_secs = 5

# 企业微信会话结束通知
[[webhooks]]
name = "wechat-session"
description = "企业微信会话结束通知"
trigger = "session_end"
url = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=XXX"
enabled = true
timeout_secs = 5

# 云端审计日志
[[webhooks]]
name = "audit-log"
description = "云端审计日志"
trigger = "tool_call_start"
url = "https://log-service.example.com/audit"
enabled = true
timeout_secs = 5
retries = 3

[webhooks.headers]
Authorization = "Bearer AUDIT_TOKEN"
```

## 相关文档

- [Hook 系统总览](./hooks.md)
- [完整 Hook 时机列表](./hook-timing-complete.md)
- [CLI 使用指南](./hook-cli-guide.md)
