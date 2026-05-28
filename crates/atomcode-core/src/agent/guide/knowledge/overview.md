---
title: AtomCode 概览
category: 概览
keywords: [overview, 概览, 介绍, 指南, 入门, 使用, 功能, atomcode, 是什么, 怎么, 如何, 怎么用, 快速开始, 帮助, help, 安装, 更新, 支持, 系统, 平台, 版本, 终端, vscode, 插件, 快捷键, 键盘, hotkey, keybindings, 键位, 日志, log, debug, 调试, 报错, 错误, error, bug, 搜索, search, grep, 咋用, 咋办, 啥是]
---

# AtomCode 使用指南

AtomCode 是开源终端 AI 编程助手，Rust 编写，支持 macOS/Linux/Windows/HarmonyOS。

## 使用方式

AtomCode 提供终端 (CLI) 和 VS Code 插件两种使用方式，共享相同的 Provider 配置和会话数据。
- VS Code 插件: https://atomcode.atomgit.com/index.html#editor-plugins

## 快速开始
- 用自然语言描述编程任务，AI 自动读写文件、执行命令
- 输入 `/` 查看所有可用命令

## 核心功能

**AI 对话编程** — 终端内自然语言交互，多模型/多 Provider 切换，流式输出，思考链

**代码操作** — 读写编辑、并行多文件编辑、代码图索引、LSP 诊断

**工具系统** — Bash 执行、文件搜索 (grep/glob)、Web 搜索、权限控制 (自动/交互/审批)

**扩展生态** — MCP 协议连接外部工具、Skill 自定义模板、Plugin 插件市场、Hooks 钩子

**工作流** — Git worktree 隔离、/bg 后台任务、/plan 计划模式、/codingplan 编码计划

**会话管理** — 上下文压缩、会话持久化、记忆系统 (/remember /forget /memory)

## 常用命令速查

| 类别 | 命令 |
|------|------|
| 账户 | `/login` `/logout` `/whoami` |
| 模型 | `/model` `/provider` `/config` `/language` |
| 会话 | `/clear` `/session` `/resume` `/compact` `/context` |
| 工作流 | `/bg` `/diff` `/undo` `/cd` `/init` `/plan` `/build` |
| 扩展 | `/skills` `/plugin` `/mcp` |
| 帮助 | `/help` `/guide` `/keys` `/status` |

## 更多资源
- 文档站: https://atomcode.atomgit.com/docs/zh/
- 仓库: https://atomgit.com/atomgit_atomcode/atomcode
- 反馈: `/issue` 提交 bug 或功能请求
