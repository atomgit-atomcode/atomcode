---
title: 配置参考
category: 配置
keywords: [配置, 设置, 怎么, 如何, 模型, 切换, 修改, auto_commit, lsp, subagent, 文件, 报错, 错误, 日志, 调试]
---

# 配置参考

配置文件: `~/.atomcode/config.toml` (全局) 或 `.atomcode/config.toml` (项目)

## 示例配置

```toml
default_provider = "AtomGit-DeepSeek-V4-pro"
auto_commit = false       # 每回合自动 git commit
auto_update = true        # 每小时检查更新
language = "zh_CN"        # 界面语言 (zh_CN / en_US)

[providers."AtomGit-DeepSeek-V4-pro"]
type = "openai"           # claude / openai / ollama
model = "DeepSeek-V4-pro"
api_key = "..."
base_url = "https://llm-api.atomgit.com/v1"

[subagent]                # 子代理策略
enabled = true            # 启用并行文件编辑
initial_turns = 4         # 初始轮次预算
max_turns = 12            # 最大轮次上限
max_concurrent = 3        # 最大并发数
timeout_secs = 300        # 单任务超时 (秒)

[lsp]                     # LSP 集成
enabled = true
auto_detect = false       # 自动检测并启动语言服务器

[plugin]                  # 插件自动更新
auto_update_marketplaces = true
```

## 项目指令文件
- `CLAUDE.md` / `ATOMCODE.md` — AI 行为指令 (多层加载: ~/.atomcode/ + 项目根)
- `AGENTS.md` — Agent 配置
- `.atomcode.user.md` — 用户个人指令
