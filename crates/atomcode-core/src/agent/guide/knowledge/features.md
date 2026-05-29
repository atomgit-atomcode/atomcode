---
title: 核心功能
category: 功能
keywords: [功能, 介绍, 特性, 支持, 平台, 系统, 使用, 编程, 代码, 工具, 日志, 调试, 搜索, 报错, 错误, 权限, 权限控制, 审批, 思考链, 钩子, hooks]
---

# AtomCode 核心功能

开源终端 AI 编程助手，Rust 编写，支持 macOS/Linux/Windows/HarmonyOS。

## AI 对话
- 终端内自然语言编程，多模型/多 Provider 切换
- 流式输出 + 思考链 (DeepSeek-R1 等推理模型)
- 上下文压缩、会话持久化
- 记忆系统 (/remember, /forget, /memory)

## 代码操作
- 读写编辑文件、并行多文件编辑 (SubAgentPool)
- 代码图索引辅助理解项目结构
- LSP 集成获取诊断信息

## 工具系统
- Bash 命令执行
- 文件搜索 (grep/glob/list_dir)
- Web 搜索和内容抓取
- 权限控制：三种模式可选
  - **自动模式**：AI 直接执行命令，无需确认（适合完全信任的场景）
  - **交互模式**（推荐）：每次执行前请求用户确认，可逐条审查
  - **审批模式**：需要用户明确批准才能执行，适合安全敏感环境

## 扩展
- MCP 协议连接外部工具
- Skill 模板自定义 AI 行为
- Plugin 插件市场 + 自动更新
- Hooks 钩子系统 (事件触发自定义脚本)

## 工作流
- Git worktree 隔离并行任务
- /bg 后台任务
- /plan 计划模式 (只读探索)
- /codingplan 编码计划
