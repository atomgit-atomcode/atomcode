---
title: Getting Started
category: Getting Started
keywords: [setup, install, getting, started, first, login, sign, begin, new, how, start, quick, codingplan, initialize, configure]
---

# Getting Started

## Installation

AtomCode supports macOS, Linux, Windows, and HarmonyOS.

After installation, start by typing `atomcode` in your terminal.

**Installation methods**:
- **macOS**: `brew install atomcode`
- **Linux**: Download binary or use `curl -fsSL https://atomcode.atomgit.com/install.sh | bash`
- **Windows**: Download `.msi` installer
- **Docker**: `docker run -it --rm -v $(pwd):/workspace atomcode/atomcode:latest`

## First Use

### /setup — Setup Wizard
For first-time users, run `/setup` to automatically install default skill packages and guide basic configuration.

### /codingplan — Coding Plan
If you have a CodingPlan account, `/codingplan` can configure your model list with one click.

### /login — Sign In
Use AtomGit OAuth to sign in and unlock cloud features. Login credentials (OAuth Token) are stored encrypted locally, never in plain text. Use `/logout` to revoke the login at any time.

### /welcome — Re-run Onboarding
You can re-run the onboarding wizard at any time.

## Basic Usage

1. After starting, describe tasks in natural language
2. AI will automatically read/write files and execute commands
3. Type `/` to see all available commands
4. Type `/help` to view command help
5. Type `/guide <question>` to ask about usage

## Configuration Files

- Global config: `~/.atomcode/config.toml`
- Project instructions: `CLAUDE.md` or `ATOMCODE.md` (project root)
- Project config: `.atomcode/config.toml`

## VS Code Plugin

AtomCode provides a VS Code extension that shares the same configuration and session data.
Plugin: https://atomcode.atomgit.com/index.html#editor-plugins
