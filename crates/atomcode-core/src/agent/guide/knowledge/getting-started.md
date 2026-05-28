---
title: 快速入门
category: 入门
keywords: [安装, 登录, 开始, 首次, 初始化, 配置, 入门, 快速, codingplan, 咋用, 咋办, 啥是, 第一次]
---

# 快速入门

## 安装

AtomCode 支持 macOS、Linux、Windows、HarmonyOS。

安装后在终端输入 `atomcode` 启动。

## 首次使用

### /setup — 安装向导
首次运行推荐执行 `/setup`，自动安装默认技能包并引导基本配置。

### /codingplan — 编码计划
如果已有 CodingPlan 账号，`/codingplan` 可一键配置模型列表。

### /login — 登录
使用 AtomGit OAuth 登录，解锁云端功能。

### /welcome — 重新引导
随时可重新运行首次引导流程。

## 基本使用

1. 启动后直接用自然语言描述任务
2. AI 会自动读写文件、执行命令
3. 输入 `/` 查看所有可用命令
4. 输入 `/help` 查看命令帮助
5. 输入 `/guide <问题>` 提问使用方法

## 配置文件

- 全局配置：`~/.atomcode/config.toml`
- 项目指令：`CLAUDE.md` 或 `ATOMCODE.md`（项目根目录）
- 项目配置：`.atomcode/config.toml`

## VS Code 插件

AtomCode 提供 VS Code 扩展，共享相同的配置和会话数据。
插件地址：https://atomcode.atomgit.com/index.html#editor-plugins
