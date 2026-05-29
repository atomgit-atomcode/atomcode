---
title: MCP 集成
category: 扩展
keywords: [mcp, 扩展, 配置, 连接, 安装, 部署, 服务器, 外部, 报错, 错误, 失败, 超时, 调试, install]
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

## 安全注意事项

- **仅安装可信来源**：MCP 服务器可以执行任意命令（`command` + `args`），仅安装来自可信来源的 MCP 服务器
- **权限范围**：MCP 服务器与 AtomCode 进程具有相同的文件系统和网络权限
- **审查配置**：安装前检查 MCP 的 `command` 和 `args` 字段，确认不会执行意外操作
- **沙箱建议**：对于不可信的 MCP 服务器，建议在容器或沙箱环境中运行
- **Node.js 替代**：MCP 服务器不强制依赖 Node.js，可以用任意语言编写的可执行文件（Python 脚本、Rust 二进制等）
