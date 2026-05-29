---
title: AtomCode Overview
category: Overview
keywords: [overview, introduction, guide, getting started, about, atomcode, what, is, help, features, terminal, vscode, plugin, extension, hotkey, keybinding, keyboard, shortcut, log, debug, error, bug, search, grep, hooks, thinking, chain, language, languages, copilot, migrate, migration, glossary, term, token, lsp, mcp, provider, open, source, community, license, free, pricing, chinese, python, windows, student, beginner, learn]
---

# AtomCode User Guide

AtomCode is an open-source terminal AI programming assistant written in Rust, supporting macOS/Linux/Windows/HarmonyOS.

## Usage

AtomCode provides both terminal (CLI) and VS Code plugin interfaces, sharing the same Provider configuration and session data.

### Terminal (CLI)
Launch by typing `atomcode` in your terminal. All features are accessible via slash commands.

### VS Code Plugin
Search "AtomCode" in the VS Code extension marketplace to install. Open the AtomCode panel in the sidebar after installation.
- Download: https://atomcode.atomgit.com/index.html#editor-plugins
- Shares Provider config, session data, and memories with the terminal
- Plugin supports code-editing shortcuts (e.g., `/explain`, `/fix`, `/test`, `/refactor`) and `/login`, `/codingplan`. Terminal-specific commands (e.g., `/model`, `/guide`, `/bg`) require the terminal
- Shortcuts integrate with VS Code native keybindings (e.g., `Cmd+Shift+P` for command palette)

## Getting Started
- Describe programming tasks in natural language, AI automatically reads/writes files and executes commands
- Type `/` to see all available commands

## Core Features

**AI Conversational Programming** — Natural language interaction in terminal, multi-model/multi-provider switching, streaming output, thinking chain

**Code Operations** — Read/write/edit, parallel multi-file editing, code graph indexing, LSP diagnostics

**Tool System** — Bash execution, file search (grep/glob), web search, permission control (auto/interactive/approval)

**Extension Ecosystem** — MCP protocol for external tools, Skill custom templates, Plugin marketplace, Hooks

**Workflows** — Git worktree isolation, /bg background tasks, /plan planning mode, /codingplan coding plan

**Session Management** — Context compression, session persistence, memory system (/remember /forget /memory)

## Quick Command Reference

| Category | Commands |
|----------|----------|
| Account | `/login` `/logout` `/whoami` |
| Model | `/model` `/provider` `/config` `/language` |
| Session | `/clear` `/session` `/resume` `/compact` `/context` |
| Workflow | `/bg` `/diff` `/undo` `/cd` `/init` `/plan` `/build` |
| Extension | `/skills` `/plugin` `/mcp` |
| Help | `/help` `/guide` `/keys` `/status` |

## Supported Languages

AtomCode supports all mainstream programming languages, including but not limited to:
- **Backend**: Rust, Go, Java, Python, C/C++, C#
- **Frontend**: TypeScript, JavaScript, React, Vue, HTML/CSS
- **Mobile**: Swift (iOS), Kotlin (Android), Dart (Flutter)
- **Data Science**: Python (pandas/numpy), R, SQL
- **Scripting**: Bash, PowerShell, Python

LSP code completion depends on whether the corresponding language server is installed.

## Migrating from GitHub Copilot

If you're coming from GitHub Copilot, key differences:
- AtomCode is terminal-first, supporting full project-level operations (read/write files, execute commands)
- Multi-provider / multi-model switching (`/model` command)
- Full workflow system (background tasks, planning mode, worktree isolation)
- Extensible via MCP for external tools, Skill/Plugin for custom behaviors
- Transparent pricing: `/cost` shows real-time token usage and costs

## Pricing & Versions

AtomCode is open source and free — no subscription fees. Cloud AI model usage is billed by each model Provider based on token consumption (check with `/cost` in real time). You can also configure local models via Ollama for completely free, offline-capable use. There is currently no enterprise edition; all features are available to all users.

## Community & Support

- Repository (with issue tracking): https://atomgit.com/atomgit_atomcode/atomcode
- Documentation: https://atomcode.atomgit.com/docs/en/
- Submit issues in the repository for help or bug reports, or type `/issue` directly in a conversation

## Glossary

| Term | Explanation |
|------|-------------|
| Provider | AI model service provider (e.g., AtomGit, OpenAI, Ollama) |
| MCP | Model Context Protocol, for connecting external tools |
| LSP | Language Server Protocol, for code completion/diagnostics |
| Skill | Reusable AI prompt template |
| Plugin | Packaged distribution containing one or more Skills |
| Worktree | Isolated git working directory for parallel development |
| SubAgent | Sub-agent that executes tasks like parallel file editing |
| Thinking Chain | Intermediate reasoning process of reasoning models |
| Context | Total conversation content the AI can currently "remember" |
| Token | LLM text unit, roughly 4 English characters or 1 CJK character |

## More Resources
- Documentation: https://atomcode.atomgit.com/docs/en/
- Repository: https://atomgit.com/atomgit_atomcode/atomcode
- Feedback: `/issue` to submit bugs or feature requests
