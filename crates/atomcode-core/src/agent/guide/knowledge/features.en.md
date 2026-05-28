---
title: Core Features
category: Features
keywords: [features, feature, introduction, overview, platform, tool, coding, code, programming, AI, debug, log, search, grep, error]
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
- Permission control: auto/interactive/approval modes

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
