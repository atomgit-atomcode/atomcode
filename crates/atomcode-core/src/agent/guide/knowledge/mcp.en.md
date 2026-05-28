---
title: MCP Integration
category: Extensions
keywords: [mcp, server, tool, model, context, protocol, connect, setup, configure, how, external, service, integration, error, timeout, debug]
---

# MCP Integration

MCP (Model Context Protocol) allows AtomCode to connect to external tools and services.

## Configuration

Add `mcpServers` in `.atomcode/settings.json`:

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

## Management Commands
- `/mcp` — View MCP server status
- `/mcp reload` — Reload MCP configuration
- `/mcp tools` — List tools provided by MCP
- `/mcp login/logout` — OAuth login/logout

## Available MCP Servers

MCP servers are installed via plugin marketplace (`/plugin marketplace`) or manual configuration. Common MCPs include filesystem access, database queries, weather/map services, etc. Check status with `/mcp` after installation.
