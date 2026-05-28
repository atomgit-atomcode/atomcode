---
title: Mode Switching
category: Workflows
keywords: [plan, build, think, mode, switch, readonly, read, write, permission, reasoning, budget]
---

# Mode Switching

AtomCode has three working modes that control AI behavior boundaries.

## /plan — Planning Mode

Read-only exploration mode. AI can only read and analyze code, cannot modify files or execute commands.

Use cases:
- Understanding project structure
- Analyzing code logic
- Exploration before making modifications

## /build — Build Mode

Full execution mode (default). AI can read/write files, execute commands, and perform all operations.

Switch: `/build` to return to build mode.

## /think — Extended Thinking

Controls AI reasoning depth, suitable for complex problems requiring deep analysis.

| Command | Description |
|---------|-------------|
| `/think` | View current status |
| `/think on` | Enable extended thinking |
| `/think off` | Disable extended thinking |
| `/think budget N` | Set thinking token budget (default 10000) |

Models supporting extended thinking (like DeepSeek-R1) will perform deeper reasoning before answering.

## /codingplan — Coding Plan

Interactive wizard to select coding tasks from CodingPlan list and auto-configure models. Ideal for first-time use or quick initialization when switching projects.
