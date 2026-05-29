---
title: Memory System
category: Features
keywords: [memory, remember, forget, how, use, save, delete, persistent, store, recall, global, view]
---

# Memory System

AtomCode provides cross-session persistent memory, allowing AI to remember important facts across different sessions.

## Commands

### /remember <content>
Save a memory. Memories are automatically injected into system prompts in subsequent sessions.

```
/remember This project uses Rust, tests with cargo test
/remember --global I prefer functional coding style
```

- Default saves as **project-level memory**, only effective in sessions under the current working directory
- `--global` flag saves as **global memory**, shared across all projects

### /forget <keyword>
Delete memories containing the specified keyword.

```
/forget coding style
```

### /memory
View all saved memories (project-level + global).

## Storage Locations

- Project-level memory: `.atomcode/memory.md` (project root)
- Global memory: `~/.atomcode/memory.md`

Memories are stored in Markdown format and can also be edited manually.

> **Note**: Memories are stored in plain text on disk. Do not store passwords, API keys, tokens, or other sensitive information in memories. Memory content is stored locally and is not automatically uploaded to the cloud.

## Use Cases

- Remember project tech stack, coding standards, test commands
- Remember user preferences (language, framework, style)
- Remember frequently used paths, URLs, configurations
- Maintain context continuity across sessions
