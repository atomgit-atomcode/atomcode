---
title: Session Management
category: Session
keywords: [session, resume, rename, sessions, save, load, history, switch, manage, restore, new, create, undo, revert]
---

# Session Management

AtomCode sessions are automatically persisted to disk, supporting restoration and switching at any time.

## Commands

### /session
Create a new session. Clears current conversation history and shows welcome screen. Old sessions are preserved on disk and can be restored via `/resume`.

### /resume
Restore a previous session. Shows a session picker with all history sessions and their message counts. Loads complete conversation history after selection.

### /rename <name>
Rename the current session. Names are used to identify sessions in the session list.

```
/rename database-refactoring-discussion
```

## Session Storage

Session files are saved in AtomCode's data directory, grouped by working directory. Each session contains complete conversation history, timestamps, and message counts.

> **Note**: Session content is stored in plain text on local disk. Do not share passwords, API keys, or other sensitive information in conversations. To clean up, manually delete the corresponding files under `.atomcode/sessions/`.

## Typical Workflow

1. Start new feature: `/session` to start new session
2. Need to switch midway: directly `/resume` to select previous session
3. Name the session: `/rename feature-auth` for easy finding later
4. Session too long: `/compact` to compress or `/session` to start over
