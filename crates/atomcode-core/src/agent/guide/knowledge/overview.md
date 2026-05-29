---
title: AtomCode 概览
category: 概览
keywords: [概览, 介绍, 指南, 入门, 使用, 功能, 是什么, 快速开始, 帮助, 安装, 更新, 支持, 系统, 平台, 版本, 终端, 插件, vscode, 快捷键, 键盘, 键位, 日志, 调试, 报错, 错误, 搜索, 钩子, hooks, 思考链, 语言, copilot, 迁移, 术语, token, lsp, mcp, provider, 啥是]
---

# AtomCode 使用指南

AtomCode 是开源终端 AI 编程助手，Rust 编写，支持 macOS/Linux/Windows/HarmonyOS。

## 使用方式

AtomCode 提供终端 (CLI) 和 VS Code 插件两种使用方式，共享相同的 Provider 配置和会话数据。

### 终端 (CLI)
在终端中直接输入 `atomcode` 启动，所有功能通过斜杠命令调用。

### VS Code 插件
在 VS Code 扩展市场搜索 "AtomCode" 安装。安装后在侧边栏打开 AtomCode 面板即可使用。
- 下载地址: https://atomcode.atomgit.com/index.html#editor-plugins
- 插件与终端共享 Provider 配置、会话数据和记忆
- 大部分斜杠命令在插件中同样可用（如 `/model`、`/guide`、`/clear`）
- 插件中的快捷键与 VS Code 原生快捷键融合（如 `Cmd+Shift+P` 打开命令面板）

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

## 支持的语言

AtomCode 支持所有主流编程语言，包括但不限于：
- **后端**: Rust、Go、Java、Python、C/C++、C#
- **前端**: TypeScript、JavaScript、React、Vue、HTML/CSS
- **移动端**: Swift (iOS)、Kotlin (Android)、Dart (Flutter)
- **数据科学**: Python (pandas/numpy)、R、SQL
- **脚本**: Bash、PowerShell、Python

LSP 代码补全支持取决于对应语言服务器是否已安装。

## 从 GitHub Copilot 迁移

如果你从 GitHub Copilot 迁来，主要区别：
- AtomCode 是终端优先的 AI 编程助手，支持完整的项目级操作（读写文件、执行命令）
- 支持多 Provider / 多模型切换（`/model` 命令）
- 有完整的工作流系统（后台任务、计划模式、worktree 隔离）
- 可通过 MCP 连接外部工具，通过 Skill/Plugin 扩展能力
- 费用透明：`/cost` 可查看实时 token 用量和费用

## 术语表

| 术语 | 解释 |
|------|------|
| Provider | AI 模型服务提供商（如 AtomGit、OpenAI、Ollama） |
| MCP | Model Context Protocol，连接外部工具的协议 |
| LSP | Language Server Protocol，提供代码补全/诊断 |
| Skill | 可复用的 AI 提示模板 |
| Plugin | 包含多个 Skill 的打包分发形式 |
| Worktree | Git 隔离工作目录，用于并行开发 |
| SubAgent | 子代理，并行执行文件编辑等子任务 |
| 思考链 | 推理模型的中间推理过程 |
| 上下文 | AI 当前能"记住"的对话内容总量 |
| Token | LLM 的文本计量单位，约等于英文 4 个字符或中文 1 个字符 |

## 更多资源
- 文档站: https://atomcode.atomgit.com/docs/zh/
- 仓库: https://atomgit.com/atomgit_atomcode/atomcode
- 反馈: `/issue` 提交 bug 或功能请求
