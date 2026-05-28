---
title: 核心功能
category: 功能
keywords: [features, 功能, 介绍, atomcode, 特性, 支持, 平台, 系统, 怎么, 如何, 使用, AI, 编程, 代码, 工具, 日志, log, debug, 调试, 搜索, search, grep, 报错, 错误]
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
- 权限控制: 自动/交互/审批三种模式

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
