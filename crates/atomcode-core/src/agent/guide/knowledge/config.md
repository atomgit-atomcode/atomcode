---
title: 配置参考
category: 配置
keywords: [配置, 设置, 模型, 切换, 修改, auto_commit, lsp, subagent, 文件, 报错, 错误, 日志, 调试, 语言, locale, api key, key, lang]
---

# 配置参考

配置文件: `~/.atomcode/config.toml` (全局) 或 `.atomcode/config.toml` (项目)

## 最小配置

入门只需要这几行就能开始使用：

```toml
default_provider = "AtomGit-deepseek-v4-flash"

[providers.AtomGit-deepseek-v4-flash]
type = "openai"
model = "deepseek-v4-flash"
api_key = "你的API密钥"
base_url = "https://llm-api.atomgit.com/v1"
```

## 完整示例配置

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

## 安全注意事项

- **API Key 保护**：配置文件中的 `api_key` 以明文存储。建议将 `~/.atomcode/config.toml` 权限设为 `600`（`chmod 600 ~/.atomcode/config.toml`），防止其他用户读取
- **不要提交到版本控制**：确保 `config.toml` 已加入 `.gitignore`，避免将 API Key 提交到 git 历史
- **环境变量替代**：支持通过环境变量 `ATOMCODE_API_KEY` 传递 API Key，避免写在配置文件中。CI 环境推荐使用此方式
- **Provider type 说明**：`type` 字段指 API 协议兼容格式（`openai` 表示兼容 OpenAI API 格式），而非模型提供商名称。例如 DeepSeek 模型使用 OpenAI 兼容接口，所以 `type = "openai"`

## 本地模型配置 (Ollama)

如果需要在本地或离线环境使用模型，可以配置 Ollama：

```toml
default_provider = "ollama-local"

[providers."ollama-local"]
type = "ollama"
model = "codellama:7b"
base_url = "http://localhost:11434/v1"
```

前置条件：安装 [Ollama](https://ollama.com) 并拉取模型（`ollama pull codellama:7b`）。本地模型不需要 `api_key`。

## CI/CD 与非交互式环境

在 CI/CD pipeline 或 Docker 容器中使用 AtomCode 时，注意以下几点：

- **环境变量**：通过 `ATOMCODE_API_KEY` 环境变量传递 API Key，而不是写在配置文件中
- **非交互式模式**：CI 环境中建议关闭交互式功能，如 `auto_commit = false`、关闭自动更新
- **代理配置**：企业网络环境需设置 `http_proxy` / `https_proxy` 环境变量
- **退出码**：非交互式模式下 AtomCode 的命令执行结果通过标准退出码返回（0 成功，非 0 失败）
- **日志**：设置 `RUST_LOG=info` 查看运行日志

## 低资源设备调优

在树莓派等低资源设备上运行时，建议降低子代理并发参数：

```toml
[subagent]
enabled = true
max_concurrent = 1    # 限制为单任务
timeout_secs = 600    # 适当延长超时
```
