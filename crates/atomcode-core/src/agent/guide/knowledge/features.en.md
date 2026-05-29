---
title: Core Features
category: Features
keywords: [features, feature, introduction, overview, platform, platforms, tool, coding, code, programming, AI, debug, log, search, grep, error, permission, control, thinking, chain, reasoning]
---

# AtomCode Core Features

Open-source terminal AI programming assistant written in Rust, supporting macOS/Linux/Windows/HarmonyOS.

## AI Conversation
- Natural language programming in terminal, multi-model/multi-provider switching
- Streaming output + thinking chain (reasoning models like DeepSeek-R1)
- Context compression, session persistence
- Memory system (/remember, /forget, /memory)

## Code Operations
- Read/write/edit files, parallel multi-file editing (SubAgentPool)
- Code graph index for project structure understanding
- LSP integration for diagnostics

## Tool System
- Bash command execution
- File search (grep/glob/list_dir)
- Web search and content fetching
- Permission control: three modes available
  - **Auto mode**: AI executes commands directly without confirmation (for fully trusted scenarios)
  - **Interactive mode** (recommended): Requests user confirmation before each execution
  - **Approval mode**: Requires explicit user approval, suitable for security-sensitive environments

## Extensions
- MCP protocol for external tools
- Skill templates for custom AI behavior
- Plugin marketplace + auto-update
- Hooks system (event-triggered custom scripts)

## Workflows
- Git worktree isolation for parallel tasks
- /bg background tasks
- /plan planning mode (read-only exploration)
- /codingplan coding plan
