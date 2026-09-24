# Webhook Hook 使用指南（已移除）

> **这套 TOML webhook 系统已不再触发。** `hooks.toml` 里的 `[[webhooks]]` /
> `[[async_webhooks]]`（以及 `[[hooks]]` 脚本钩子、内置 Rust 钩子）在当前运行时
> **不会被执行**。唯一生效的钩子系统是 JSON 版 `hooks.json` / `.hooks.json`，
> 见 [Hooks 指南](./hooks.md)。

## 想要 webhook 效果怎么办

用一个 `.hooks.json` 的 shell 钩子去 `curl` 你的 HTTP 端点即可 —— 钩子在 stdin
收到事件的 JSON 载荷，直接转发出去：

```json
{
  "hooks": {
    "audit-webhook": {
      "event": "PostToolUse",
      "command": "/usr/local/bin/forward-webhook.sh",
      "timeout_ms": 10000
    }
  }
}
```

```bash
#!/bin/bash
# /usr/local/bin/forward-webhook.sh
payload=$(cat)                         # 事件载荷(JSON)从 stdin 进来
curl -sS -m 8 -X POST \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $YOUR_TOKEN" \
  --data-binary "$payload" \
  https://your-endpoint.example.com/hook >/dev/null 2>&1 || true
# 钩子失败是 fail-open,不会阻断回合;要"异步/不阻塞"就在末尾加 `&` 或交给队列。
```

- **同步 / 阻塞**：像上面这样直接 `curl`（受 `timeout_ms` 约束）。
- **异步 / 高频**：让脚本把载荷丢进本地队列/文件，由你自己的后台进程批量投递，
  钩子本身立即返回 —— 比原来的 `[[async_webhooks]]` 更可控，也不依赖已移除的引擎。

字段、事件列表、载荷结构、退出码契约等，一律以 [Hooks 指南](./hooks.md) 为准。
