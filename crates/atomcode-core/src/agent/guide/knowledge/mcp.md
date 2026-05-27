---
title: 常用 MCP 配置
category: 扩展
keywords: [mcp, 配置, server, tool, 扩展]
---

# 常用 MCP 配置

MCP (Model Context Protocol) 允许 AtomCode 连接外部工具和服务。

## 配置方式
在 `.atomcode/settings.json` 中添加 `mcpServers` 字段。

## 示例配置
```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@anthropic-ai/mcp-server-filesystem", "/path"]
    }
  }
}
```

## 管理命令
- `/mcp` — 查看已配置的 MCP 服务器
- `/mcp add` — 添加 MCP 服务器
- `/mcp remove` — 移除 MCP 服务器
