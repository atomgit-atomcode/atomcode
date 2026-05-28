---
title: Git Worktree Isolation
category: Workflows
keywords: [worktree, isolation, parallel, development, git, checkout, branch, isolated, create, list, done, cleanup]
---

# Git Worktree Isolation (/worktree)

`/worktree` uses git worktree to create isolated working directories, allowing experimental development without affecting the main branch.

## Subcommands

| Command | Description |
|---------|-------------|
| `/worktree create [name]` | Create new worktree from current branch |
| `/worktree list` | List all worktrees |
| `/worktree done` | Mark current worktree as complete |
| `/worktree cleanup` | Clean up completed worktrees |

## Typical Flow

1. When working on a feature branch, enter `/worktree create feature-x`
2. AtomCode creates an independent git worktree directory
3. Freely modify and test in the isolated environment
4. When done, `/worktree done` to mark complete
5. `/worktree cleanup` to clean up temporary directory

## Difference from /bg

- `/worktree` = Filesystem isolation (different directory, different branch)
- `/bg` = Session isolation (same directory, different conversation)
- Both can be combined: run bg tasks within a worktree
