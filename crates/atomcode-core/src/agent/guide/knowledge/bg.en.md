---
title: Background Tasks
category: Workflows
keywords: [bg, background, task, parallel, run, execute, jobs, queue, how, slot]
---

# Background Tasks (/bg)

`/bg` allows you to run tasks without blocking the current session. Ideal for time-consuming operations like batch refactoring, test runs, and code generation.

## Usage

| Command | Description |
|---------|-------------|
| `/bg <task description>` | Move current session to background, create new session in foreground |
| `/bg` | No arguments: show help |
| `/bg list` | List all background tasks and their status |
| `/bg <N>` | Switch to background slot N |
| `/bg drop <N>` | Terminate and remove background slot N |

## How It Works

1. Current conversation is saved as a background session
2. A new empty session is created in the foreground
3. Background task runs independently, not affected by foreground
4. Check progress with `/bg list`, switch back with `/bg <N>`

## Limitations

- Maximum 4 concurrent background tasks (`MAX_BACKGROUND_SLOTS`)
- Background tasks share the same Provider quota
- `/background <task>` is an alias, same effect as `/bg <task>`
