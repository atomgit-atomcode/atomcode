---
title: Configuration Reference
category: Configuration
keywords: [config, configuration, settings, model, provider, switch, modify, how, customize, setup, language, error, log, debug]
---

# Configuration Reference

Config file: `~/.atomcode/config.toml` (global) or `.atomcode/config.toml` (project)

## Example Configuration

```toml
default_provider = "AtomGit-DeepSeek-V4-pro"
auto_commit = false       # Auto git commit each turn
auto_update = true        # Check updates hourly
language = "en_US"        # UI language (zh_CN / en_US)

[providers."AtomGit-DeepSeek-V4-pro"]
type = "openai"           # claude / openai / ollama
model = "DeepSeek-V4-pro"
api_key = "..."
base_url = "https://llm-api.atomgit.com/v1"

[subagent]                # Sub-agent policy
enabled = true            # Enable parallel file editing
initial_turns = 4         # Initial turn budget
max_turns = 12            # Max turn limit
max_concurrent = 3        # Max concurrency
timeout_secs = 300        # Per-task timeout (seconds)

[lsp]                     # LSP integration
enabled = true
auto_detect = false       # Auto-detect and start language servers

[plugin]                  # Plugin auto-update
auto_update_marketplaces = true
```

## Project Instruction Files
- `CLAUDE.md` / `ATOMCODE.md` — AI behavior instructions (multi-layer loading: ~/.atomcode/ + project root)
- `AGENTS.md` — Agent configuration
- `.atomcode.user.md` — User personal instructions
