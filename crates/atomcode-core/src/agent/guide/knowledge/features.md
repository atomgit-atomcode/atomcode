---
title: 核心功能
category: 功能
keywords: [features, 功能, 特性, 能力, atomcode, 介绍]
---

# AtomCode 核心功能

AtomCode 是一个开源终端 AI 编程助手，基于 Rust 编写。主要功能包括：

## AI 对话编程
- 在终端中与 AI 对话，获得代码生成、解释、重构帮助
- 支持多模型切换 (/model)
- 支持多 Provider (/provider)

## 文件编辑
- 自动读写和编辑项目文件
- 支持并行多文件编辑
- 代码图辅助理解代码结构

## 工具系统
- Bash 命令执行
- 文件搜索 (grep, glob)
- Web 搜索和抓取
- LSP 集成

## 会话管理
- 会话恢复 (/resume)
- 对话历史压缩 (/compact)
- 上下文窗口管理 (/context)

## 权限控制
- 工具权限管理
- 自动/交互/审批三种模式
