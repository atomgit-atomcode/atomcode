# Plugin Recommendations

Plugins are installable collections of skills, commands, agents, and hooks. AtomCode supports plugin installation to extend functionality.

**Note**: These are common plugin patterns. Use web search to discover additional community plugins.

---

## Core Plugins

### AtomCode Official

| Plugin | Best For | Key Features |
|--------|----------|--------------|
| **atomcode** | AtomCode usage & documentation Q&A | Offline docs index, install/config/troubleshooting answers, `/skills ask` command |

### Development & Code Quality

| Plugin | Best For | Key Features |
|--------|----------|--------------|
| **plugin-dev** | Building AtomCode plugins | Skills for creating skills, hooks, commands, agents |
| **pr-review-toolkit** | PR review workflows | Specialized review agents (code, tests, types) |
| **code-review** | Automated code review | Multi-agent review with confidence scoring |
| **code-simplifier** | Code refactoring | Simplify code while preserving functionality |
| **feature-dev** | Feature development | End-to-end feature workflow with agents |

### Git & Workflow

| Plugin | Best For | Key Features |
|--------|----------|--------------|
| **commit-commands** | Git workflows | /commit, /commit-push-pr commands |
| **hookify** | Automation rules | Create hooks from conversation patterns |

### Frontend

| Plugin | Best For | Key Features |
|--------|----------|--------------|
| **frontend-design** | UI development | Production-grade UI, avoids generic aesthetics |

### Learning & Guidance

| Plugin | Best For | Key Features |
|--------|----------|--------------|
| **explanatory-output-style** | Learning | Educational insights about code choices |
| **learning-output-style** | Interactive learning | Requests contributions at decision points |
| **security-guidance** | Security awareness | Warns about security issues when editing |

### Language Servers (LSP)

| Plugin | Language |
|--------|----------|
| **typescript-lsp** | TypeScript/JavaScript |
| **pyright-lsp** | Python |
| **gopls-lsp** | Go |
| **rust-analyzer-lsp** | Rust |
| **clangd-lsp** | C/C++ |
| **jdtls-lsp** | Java |
| **kotlin-lsp** | Kotlin |
| **swift-lsp** | Swift |
| **csharp-lsp** | C# |
| **php-lsp** | PHP |
| **lua-lsp** | Lua |

---

## Quick Reference: Codebase -> Plugin

| Codebase Signal | Recommended Plugin |
|-----------------|-------------------|
| Any project (first-time setup) | atomcode |
| Building plugins | plugin-dev |
| PR-based workflow | pr-review-toolkit |
| Git commits | commit-commands |
| React/Vue/Angular | frontend-design |
| Want automation rules | hookify |
| TypeScript project | typescript-lsp |
| Python project | pyright-lsp |
| Go project | gopls-lsp |
| Security-sensitive code | security-guidance |
| Learning/onboarding | explanatory-output-style |

---

## When to Recommend Plugins

**Recommend plugin installation when:**
- User wants to install AtomCode automations from a shared repository
- User needs multiple related capabilities
- Team wants standardized workflows
- First-time AtomCode setup
