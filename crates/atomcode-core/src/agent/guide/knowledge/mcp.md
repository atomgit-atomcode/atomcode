---
title: MCP 集成
category: 扩展
keywords: [mcp, 扩展, 配置, 怎么, 如何, 连接, 安装, 服务器, 外部, 报错, 错误, 失败, 超时, 调试]
---

# MCP 集成

MCP (Model Context Protocol) 让 AtomCode 连接外部工具和服务。

## 配置

在 `.atomcode/settings.json` 中添加 `mcpServers`:

```json
{
  "mcpServers": {
    "my-server": {
      "command": "npx",
      "args": ["-y", "@scope/mcp-server-name"]
    }
  }
}
```

## 管理命令
- `/mcp` — 查看 MCP 服务器状态
- `/mcp reload` — 重载 MCP 配置
- `/mcp tools` — 列出 MCP 提供的工具
- `/mcp login/logout` — OAuth 登录/登出

## 可用的 MCP 服务器

MCP 服务器通过插件市场 (`/plugin marketplace`) 或手动配置安装。常见的 MCP 包括文件系统访问、数据库查询、天气/地图服务等。安装后通过 `/mcp` 查看状态。
