---
title: MCP Integration
category: Extensions
keywords: [mcp, server, tool, model, context, protocol, connect, connection, setup, configure, how, external, service, integration, error, timeout, debug, install, api]
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

## Security Considerations

- **Only install from trusted sources**: MCP servers can execute arbitrary commands (`command` + `args`). Only install from trusted sources
- **Permission scope**: MCP servers run with the same filesystem and network permissions as the AtomCode process
- **Review configuration**: Check the `command` and `args` fields before installing to ensure no unexpected operations
- **Sandbox recommendation**: Run untrusted MCP servers in a container or sandboxed environment
- **No Node.js requirement**: MCP servers are not limited to Node.js — they can be executables written in any language (Python scripts, Rust binaries, etc.)
