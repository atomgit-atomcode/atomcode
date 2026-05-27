# 异步 Webhook 和批量发送指南

> ⚠️ **当前状态警告**：异步 Webhook 的定时 flush 机制尚未完全激活（参见 issue #914）。目前 batcher 已创建但不会自动定期发送。在定时 flush 完成之前，请使用同步 Webhook 替代异步模式。关注后续更新以获取可用通知。

## 概述

异步 Webhook 使用后台任务处理 Webhook 请求，避免阻塞 AtomCode 主流程。通过批量发送，可以显著减少 HTTP 请求数量，提高性能。

## 特性

### 1. 异步非阻塞

- **同步模式**：每个 Hook 触发时立即发送 HTTP 请求（阻塞 10-200ms）
- **异步模式**：事件加入队列，后台任务异步发送（延迟 < 1ms）

### 2. 批量聚合

- 达到批量大小后自动发送
- 定时刷新（默认 1 秒）
- 减少 HTTP 请求数量

### 3. 自动重试

- 指数退避重试策略
- 失败不中断主流程

## 配置示例

### 基本异步 Webhook

```toml
[[async_webhooks]]
name = "slack-async"
description = "异步发送到 Slack"
url = "https://hooks.slack.com/services/XXX"
enabled = true

# 批量配置
batch_size = 10           # 每 10 个事件发送一次
flush_interval_ms = 1000  # 每 1 秒刷新一次

# HTTP 配置
timeout_secs = 10
retries = 2
```

### 高性能批量

```toml
[[async_webhooks]]
name = "audit-batch"
description = "高性能批量审计日志"
url = "https://log-service.example.com/batch"
enabled = true

# 大批量配置
batch_size = 50
flush_interval_ms = 500  # 0.5 秒刷新

[async_webhooks.headers]
Authorization = "Bearer AUDIT_TOKEN"
```

### 低延迟模式

```toml
[[async_webhooks]]
name = "realtime-notify"
description = "低延迟通知"
url = "https://notify.example.com/realtime"
enabled = true

# 小批量，快速刷新
batch_size = 1             # 每个事件立即发送
flush_interval_ms = 100    # 100ms 刷新
```

## 配置参数说明

| 参数 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `batch_size` | usize | 10 | 批量大小（达到此数量后发送） |
| `flush_interval_ms` | u64 | 1000 | 刷新间隔（毫秒） |
| `timeout_secs` | u64 | 10 | HTTP 超时时间（秒） |
| `retries` | u32 | 2 | 重试次数 |
| `method` | String | "POST" | HTTP 方法 |
| `headers` | Map | {} | 自定义 Header |
| `enabled` | bool | true | 是否启用 |

## 工作原理

### 同步 vs 异步对比

#### 同步模式（默认）

```
Hook 触发 → HTTP 请求（10-200ms）→ 继续执行
           ↑ 阻塞主流程
```

#### 异步模式

> ⚠️ 异步模式的定时 flush 当前未激活（参见顶部警告）。以下流程描述的是目标行为。

```
Hook 触发 → 加入队列（< 1ms）→ 继续执行
              ↓
         后台任务批量发送（10-200ms）
              ↓
         不阻塞主流程
```

### 批量发送流程

```
1. Hook 触发 → 事件加入内存队列
2. 检查队列长度
   - 达到 batch_size → 立即发送
   - 未达到 → 等待
3. 定时刷新（flush_interval_ms）
   - 队列非空 → 强制发送
   - 队列为空 → 跳过
4. 后台任务发送批量数据
   - 成功 → 记录日志
   - 失败 → 重试（指数退避）
```

## 使用示例

### 示例 1：Slack 批量通知

```toml
# 同步模式（每次工具调用都发送）
[[webhooks]]
name = "slack-sync"
trigger = "tool_call_start"
url = "https://hooks.slack.com/services/XXX"
enabled = true

# 异步模式（批量发送）
[[async_webhooks]]
name = "slack-batch"
url = "https://hooks.slack.com/services/XXX"
enabled = true
batch_size = 20
flush_interval_ms = 2000
```

**性能对比**：

| 场景 | 同步模式 | 异步模式 |
|------|---------|---------|
| 100 次工具调用 | 100 次 HTTP 请求，总延迟 1-20 秒 | 5 次 HTTP 请求，总延迟 < 1ms |
| 阻塞主流程 | ✅ 是 | ❌ 否 |
| 丢失风险 | ❌ 无 | ⚠️ 进程崩溃时可能丢失 |

### 示例 2：云端审计日志

```toml
[[async_webhooks]]
name = "audit-log"
description = "批量发送审计日志"
url = "https://log-service.example.com/audit"
enabled = true

batch_size = 50           # 每 50 个事件发送一次
flush_interval_ms = 1000  # 每秒刷新一次
retries = 5               # 重试 5 次

[async_webhooks.headers]
Authorization = "Bearer AUDIT_TOKEN"
Content-Type = "application/json"
```

### 示例 3：多目标批量通知

```toml
# Slack 异步批量
[[async_webhooks]]
name = "slack"
url = "https://hooks.slack.com/services/XXX"
enabled = true
batch_size = 10
flush_interval_ms = 1000

# 钉钉异步批量
[[async_webhooks]]
name = "dingtalk"
url = "https://oapi.dingtalk.com/robot/send?access_token=XXX"
enabled = true
batch_size = 10
flush_interval_ms = 1000

# 企业微信异步批量
[[async_webhooks]]
name = "wechat"
url = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=XXX"
enabled = true
batch_size = 10
flush_interval_ms = 1000
```

## 请求格式

### 批量请求格式

异步 Webhook 发送的是批量请求，格式如下：

```json
{
  "url": "https://your-webhook-url",
  "method": "POST",
  "headers": {
    "Authorization": "Bearer TOKEN"
  },
  "events": [
    {
      "event": "on_tool_call_start",
      "hook_name": "audit-log",
      "trigger": "tool_call_start",
      "context": {
        "tool_name": "edit_file",
        "tool_args": "{...}",
        "working_dir": "/path/to/project"
      },
      "timestamp_ms": 1234567890123
    },
    {
      "event": "post_tool_execution",
      "hook_name": "audit-log",
      "trigger": "post_tool",
      "context": {
        "tool_name": "edit_file",
        "result": "File updated",
        "success": true,
        "duration_ms": 150
      },
      "timestamp_ms": 1234567890456
    }
  ]
}
```

### 批量请求 Header

| Header | 说明 |
|--------|------|
| `X-AtomCode-Version` | AtomCode 版本 |
| `X-AtomCode-Webhook` | Webhook 名称 |
| `X-AtomCode-Batch-Size` | 批量中的事件数量 |
| `Content-Type` | `application/json` |

## 服务端实现示例

### Python Flask 服务端

```python
from flask import Flask, request, jsonify

app = Flask(__name__)

@app.route('/atomcode/batch', methods=['POST'])
def handle_batch():
    data = request.json
    
    # 处理批量事件
    events = data.get('events', [])
    print(f"Received {len(events)} events")
    
    for event in events:
        # 处理单个事件
        print(f"  Event: {event['event']}")
        print(f"  Tool: {event['context'].get('tool_name', 'N/A')}")
        print(f"  Time: {event['timestamp_ms']}")
    
    # 返回成功响应
    return jsonify({
        "status": "ok",
        "processed": len(events)
    })

if __name__ == '__main__':
    app.run(port=5000)
```

### Node.js Express 服务端

```javascript
const express = require('express');
const app = express();

app.use(express.json());

app.post('/atomcode/batch', (req, res) => {
    const { events } = req.body;
    
    console.log(`Received ${events.length} events`);
    
    for (const event of events) {
        console.log(`  Event: ${event.event}`);
        console.log(`  Tool: ${event.context.tool_name || 'N/A'}`);
        console.log(`  Time: ${event.timestamp_ms}`);
    }
    
    res.json({
        status: 'ok',
        processed: events.length
    });
});

app.listen(3000, () => {
    console.log('Server running on port 3000');
});
```

## 性能调优

### 批量大小选择

| 场景 | 推荐 batch_size | 说明 |
|------|----------------|------|
| 高频工具调用 | 20-50 | 减少 HTTP 请求数量 |
| 低频工具调用 | 5-10 | 避免长时间等待 |
| 实时通知 | 1-5 | 低延迟优先 |
| 审计日志 | 50-100 | 高吞吐优先 |

### 刷新间隔选择

| 场景 | 推荐 flush_interval_ms | 说明 |
|------|----------------------|------|
| 高频工具调用 | 500-1000 | 1 秒内刷新 |
| 低频工具调用 | 2000-5000 | 2-5 秒刷新 |
| 实时通知 | 100-500 | 100-500ms 刷新 |
| 审计日志 | 1000-2000 | 1-2 秒刷新 |

### 性能对比测试

**测试环境**：100 次工具调用

| 配置 | HTTP 请求数 | 总延迟 | 内存使用 |
|------|------------|--------|---------|
| 同步 | 100 | 10-20 秒 | 低 |
| 异步 (batch=10, interval=1s) | 10 | < 1ms | 中 |
| 异步 (batch=50, interval=2s) | 2 | < 1ms | 高 |

## 故障排查

### 事件丢失

**症状**：部分 Webhook 事件未发送

**排查步骤**：

1. 检查批量配置：
   ```toml
   batch_size = 10  # 是否太大？
   flush_interval_ms = 1000  # 是否太长？
   ```

2. 查看日志：
   ```bash
   atomcode -p "test" 2>&1 | grep -i async
   ```

3. 减小批量大小和刷新间隔：
   ```toml
   batch_size = 5
   flush_interval_ms = 500
   ```

### 内存占用过高

**症状**：AtomCode 内存使用持续增长

**原因**：批量大小设置过大，事件积压

**解决方案**：

```toml
# 减小批量大小
batch_size = 10

# 缩短刷新间隔
flush_interval_ms = 1000
```

### 服务端接收不到批量数据

**症状**：服务端日志显示接收到空 events 数组

**排查**：

1. 检查 AtomCode 日志：
   ```
   [AsyncWebhook] Sent 0 events to ...
   ```

2. 确认 flush 触发：
   - 批量大小未达到
   - 刷新间隔未到

3. 手动触发 flush（在代码中）：
   ```rust
   batcher.flush().await;
   ```

## 最佳实践

### 1. 根据场景选择模式

```toml
# 高频场景：异步批量
[[async_webhooks]]
name = "high-frequency"
batch_size = 20
flush_interval_ms = 1000

# 低频场景：同步
[[webhooks]]
name = "low-frequency"
trigger = "session_end"
```

### 2. 合理配置批量大小

- **不要太大**（> 100）：内存占用高，延迟增加
- **不要太小**（< 5）：失去批量优势
- **推荐**：10-50

### 3. 设置合理的刷新间隔

- **不要太长**（> 5s）：事件积压
- **不要太短**（< 100ms）：频繁 HTTP 请求
- **推荐**：500-2000ms

### 4. 监控和告警

```toml
# 错误通知（同步，确保不丢失）
[[webhooks]]
name = "error-alert"
trigger = "error"
url = "https://alert.example.com/error"
enabled = true

# 常规审计（异步批量）
[[async_webhooks]]
name = "audit-batch"
url = "https://log.example.com/audit"
enabled = true
batch_size = 20
flush_interval_ms = 1000
```

## 完整配置示例

```toml
# ~/.atomcode/hooks/hooks.toml

# 同步 Webhook（关键通知）
[[webhooks]]
name = "error-alert"
description = "错误实时通知"
trigger = "error"
url = "https://alert.example.com/error"
enabled = true
timeout_secs = 5

# 异步 Webhook（批量审计）
[[async_webhooks]]
name = "audit-log"
description = "批量审计日志"
url = "https://log.example.com/audit"
enabled = true

# 批量配置
batch_size = 20
flush_interval_ms = 1000

# HTTP 配置
timeout_secs = 10
retries = 3

# 自定义 Header
[async_webhooks.headers]
Authorization = "Bearer AUDIT_TOKEN"
Content-Type = "application/json"

# 异步 Webhook（批量通知）
[[async_webhooks]]
name = "slack-batch"
description = "Slack 批量通知"
url = "https://hooks.slack.com/services/XXX"
enabled = true
batch_size = 10
flush_interval_ms = 2000
```

## 相关文档

- [Webhook 使用指南](./webhook-guide.md)
- [Hook 系统总览](./hooks.md)
- [完整 Hook 时机列表](./hook-timing-complete.md)
