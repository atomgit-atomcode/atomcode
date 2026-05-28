---
title: Git Worktree 隔离
category: 工作流
keywords: [worktree, 隔离, 分支, 并行开发, git, checkout, branch, 并行]
---

# Git Worktree 隔离 (/worktree)

`/worktree` 用 git worktree 创建隔离的工作目录，让你在不影响主分支的情况下进行实验性开发。

## 子命令

| 命令 | 说明 |
|------|------|
| `/worktree create [名称]` | 从当前分支创建新的 worktree |
| `/worktree list` | 列出所有 worktree |
| `/worktree done` | 标记当前 worktree 为完成 |
| `/worktree cleanup` | 清理已完成的 worktree |

## 典型流程

1. 在功能分支上工作时，输入 `/worktree create feature-x`
2. AtomCode 创建独立的 git worktree 目录
3. 在隔离环境中自由修改、测试
4. 完成后 `/worktree done` 标记完成
5. `/worktree cleanup` 清理临时目录

## 与 /bg 的区别

- `/worktree` = 文件系统隔离（不同目录、不同分支）
- `/bg` = 会话隔离（同一目录、不同对话）
- 两者可组合使用：在 worktree 中运行 bg 任务
