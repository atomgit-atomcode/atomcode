---
title: Configuration Reference
category: Configuration
keywords: [config, configuration, settings, model, provider, switch, modify, how, customize, setup, language, error, log, debug, configure, api, key]
---

# Configuration Reference

Config file: `~/.atomcode/config.toml` (global) or `.atomcode/config.toml` (project)

## Minimal Configuration

Just a few lines to get started:

```toml
default_provider = "AtomGit-deepseek-v4-flash"

[providers.AtomGit-deepseek-v4-flash]
type = "openai"
model = "deepseek-v4-flash"
api_key = "your-api-key"
base_url = "https://llm-api.atomgit.com/v1"
```

## Full Example Configuration

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

## Security Notes

- **API Key Protection**: `api_key` is stored in plain text. Set file permissions to `600` (`chmod 600 ~/.atomcode/config.toml`) to prevent other users from reading it
- **Don't commit to version control**: Ensure `config.toml` is in `.gitignore` to avoid committing API keys to git history
- **Environment variable alternative**: You can use `ATOMCODE_API_KEY` environment variable instead of writing it in the config file. Recommended for CI environments
- **Provider type**: The `type` field specifies the API protocol format (`openai` = OpenAI-compatible API), not the model vendor name. For example, DeepSeek uses an OpenAI-compatible interface, so `type = "openai"`

## Local Model Configuration (Ollama)

To use models locally or offline, configure Ollama:

```toml
default_provider = "ollama-local"

[providers."ollama-local"]
type = "ollama"
model = "codellama:7b"
base_url = "http://localhost:11434/v1"
```

Prerequisites: Install [Ollama](https://ollama.com) and pull a model (`ollama pull codellama:7b`). No `api_key` needed for local models.

## CI/CD & Non-Interactive Environments

When using AtomCode in CI/CD pipelines or Docker containers:

- **Environment variables**: Use `ATOMCODE_API_KEY` env var instead of writing API keys in config files
- **Non-interactive mode**: Disable interactive features in CI (e.g., `auto_commit = false`, disable auto-update)
- **Proxy**: Set `http_proxy` / `https_proxy` env vars for corporate network environments
- **Exit codes**: In non-interactive mode, AtomCode returns standard exit codes (0 = success, non-zero = failure)
- **Logging**: Set `RUST_LOG=info` to view runtime logs

## Low-Resource Device Tuning

When running on resource-constrained devices like Raspberry Pi, reduce sub-agent concurrency:

```toml
[subagent]
enabled = true
max_concurrent = 1    # Limit to single task
timeout_secs = 600    # Extend timeout appropriately
```
