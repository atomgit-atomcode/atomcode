---
title: Slash 命令参考
category: 命令
keywords: [命令, /bg, /help, /model, /config, /mcp, /skills, /guide, 快捷键, 切换, 模型, 会话, 记忆, 插件, codingplan, 键盘, 模式, 费用, 压缩, 隔离, 后台, 计划, 思考, 构建, 搜索, 调试]
---

# Slash 命令 (共 41 个)

> **新手推荐**：刚接触 AtomCode 先记住这几个：`/help`（查看所有命令）、`/model`（切换模型）、`/clear`（清屏）、`/session`（新建会话）、`/guide <问题>`（提问使用方法）、`/keys`（查看快捷键）。其他命令慢慢熟悉即可。

## 账户
- `/login` — AtomGit OAuth 登录
- `/logout` — 退出登录
- `/whoami` — 查看当前用户

## 会话
- `/clear` — 清屏
- `/session` — 新建/切换会话
- `/rename` — 重命名当前会话
- `/resume` — 恢复之前的会话
- `/compact` — 压缩对话历史
- `/context` — 查看上下文使用量
- `/cost` — 查看 token 费用

## 模型与配置
- `/model [name]` — 切换模型
- `/provider [name]` — 切换 Provider
- `/config` — 打开配置文件
- `/reload` — 重载配置
- `/language` — 切换界面语言
- `/setup` — 首次安装向导
- `/upgrade` — 升级 atomcode

## 工作流
- `/bg <任务>` — 后台执行任务 (别名: /background)
- `/diff` — 查看 git diff
- `/undo` — 撤销上次修改
- `/cd <目录>` — 切换工作目录
- `/init` — 初始化项目指令文件
- `/worktree` — git worktree 隔离
- `/plan` — 计划模式 (只读探索)
- `/build` — 构建模式 (完整执行)
- `/think [on/off/N]` — 扩展思考控制
- `/codingplan` — 编码计划 (已合并入 `/login` 流程)

## 记忆
- `/remember <内容>` — 保存事实到记忆 (--global 全局)
- `/forget <关键词>` — 删除匹配的记忆
- `/memory` — 查看所有记忆

## 扩展
- `/skills` — 浏览已安装技能
- `/plugin` — 插件市场 (install/uninstall/list)
- `/mcp` — MCP 服务器状态
- `/paste` — 粘贴剪贴板图片

## 帮助
- `/help` — 显示所有命令
- `/guide <问题>` — 向指南提问
- `/keys` — 键盘快捷键
- `/issue` — 报告 bug / 请求功能
- `/welcome` — 首次引导
- `/status` — 会话状态
- `/quit` (或 `/exit`) — 退出
