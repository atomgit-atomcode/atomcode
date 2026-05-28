---
title: AtomCode Overview
category: Overview
keywords: [overview, introduction, guide, getting started, about, atomcode, what, is, help, features, terminal, vscode, plugin, extension, hotkey, keybinding, keyboard, shortcut, log, debug, error, bug, search, grep]
---

# AtomCode User Guide

AtomCode is an open-source terminal AI programming assistant written in Rust, supporting macOS/Linux/Windows/HarmonyOS.

## Usage

AtomCode provides both terminal (CLI) and VS Code plugin interfaces, sharing the same Provider configuration and session data.
- VS Code plugin: https://atomcode.atomgit.com/index.html#editor-plugins

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

## More Resources
- Documentation: https://atomcode.atomgit.com/docs/en/
- Repository: https://atomgit.com/atomgit_atomcode/atomcode
- Feedback: `/issue` to submit bugs or feature requests
