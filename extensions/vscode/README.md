# AtomCode for VS Code

AtomCode 是一个开源 AI 编程 Agent。这个 VS Code 扩展把 AtomCode 的对话、工具执行、模型选择和会话历史带进 IDE，让你可以在编辑器里直接让 Agent 阅读代码、执行命令、修改文件并验证结果。

它适合想要 Claude Code 类似 Agent 工作流，同时希望使用 AtomCode、AtomGit CodingPlan、本地或 OpenAI 兼容模型的开发者。

## 主要功能

### 原生 VS Code 面板

- 在 Activity Bar 中打开 AtomCode 会话列表。
- 在侧边栏或编辑器标签页中运行聊天会话。
- 通过状态栏查看 daemon 连接状态和当前模型。
- 支持流式回答、Markdown 渲染、代码高亮和代码块复制。

### Agent 工具执行

- 实时显示工具调用、参数、输出、耗时和失败状态。
- 对需要确认的操作显示 Allow / Deny 权限提示。
- 对 bash 等潜在破坏性操作进行更醒目的确认展示。
- 支持停止当前生成任务。

### 编辑器上下文

- 自动读取当前打开文件和选区信息作为上下文。
- 在选中代码后使用右键菜单或 Code Action 调用：
  - `AtomCode: Explain`
  - `AtomCode: Fix`
  - `AtomCode: Optimize`
- 支持把选中的代码片段包装成带文件名、语言和行号的上下文提示。

### 快捷任务和斜杠菜单

欢迎页提供常用任务入口：

- Explain Code
- Fix Issues
- Write Tests
- Refactor
- Add Docs
- Code Review

输入框支持 `/` 打开命令菜单，快速插入常见任务命令，例如 `/explain`、`/fix`、`/test`、`/refactor`、`/docs`、`/review` 和 `/optimize`。

### 模型和提供商

- 支持从 daemon 返回的 providers / models 中选择当前默认模型。
- 支持 AtomGit 登录并同步 CodingPlan 模型。
- 支持手动添加 OpenAI 兼容 provider，包括 provider name、model、base URL 和 API key。

AtomCode 本体支持 Claude、OpenAI、DeepSeek、GLM、Qwen、Ollama 以及 OpenAI 兼容 API；扩展通过本地 AtomCode daemon 使用这些能力。

### 会话历史

- 支持创建新会话。
- 支持按时间分组查看历史会话。
- 支持搜索会话并恢复已有会话内容。
- 会话关联当前工作区，恢复后继续使用同一上下文。

## 快速开始

1. 安装并启用 AtomCode 扩展。
2. 打开 AtomCode Activity Bar 面板，或运行命令 `AtomCode: Open in New Tab`。
3. 首次使用时，选择一种模型配置方式：
   - 使用 AtomGit 登录并同步 CodingPlan 模型。
   - 手动添加 OpenAI 兼容 provider。
4. 在输入框里描述任务，例如：

```text
/review 检查当前未提交代码
```

或选中一段代码后右键选择 `AtomCode: Explain` / `AtomCode: Fix` / `AtomCode: Optimize`。

## 常用命令

| 命令 | 说明 |
| --- | --- |
| `AtomCode: Open in Side Bar` | 打开侧边栏会话列表 |
| `AtomCode: Open in New Tab` | 在编辑器标签页中打开 AtomCode |
| `AtomCode: Focus Input` | 聚焦输入框 |
| `AtomCode: New Conversation` | 创建新会话 |
| `AtomCode: Explain Selection` | 解释当前选中代码 |
| `AtomCode: Fix Selection` | 修复当前选中代码 |
| `AtomCode: Optimize Selection` | 优化当前选中代码 |
| `AtomCode: Stop Generation` | 停止当前生成 |

## 快捷键

| 快捷键 | macOS | 说明 |
| --- | --- | --- |
| `Ctrl+Esc` | `Cmd+Esc` | 聚焦 AtomCode 输入框 |
| `Ctrl+Shift+Esc` | `Cmd+Shift+Esc` | 在新标签页打开 AtomCode |
| `Ctrl+Shift+E` | `Cmd+Shift+E` | 解释选中代码 |
| `Ctrl+N` | `Cmd+N` | 创建新会话，需 AtomCode 输入框聚焦 |

## 扩展设置

| 设置项 | 默认值 | 说明 |
| --- | --- | --- |
| `atomcode.daemon.port` | `13456` | AtomCode daemon HTTP 端口 |
| `atomcode.daemon.autoStart` | `true` | daemon 未运行时自动启动 |
| `atomcode.daemon.binaryPath` | 空 | 自定义 AtomCode daemon 二进制路径 |
| `atomcode.preferredLocation` | `sidebar` | 默认打开位置 |
| `atomcode.autoSave` | `true` | Agent 读取文件前自动保存 |
| `atomcode.sendWithCtrlEnter` | `false` | 使用 Ctrl/Cmd+Enter 发送消息 |
| `atomcode.fontSize` | `13` | 聊天面板字体大小 |
| `atomcode.showInlineHints` | `true` | 显示内联 diff 提示 |

## 与 AtomCode CLI 的关系

这个扩展通过本地 AtomCode daemon 工作。daemon 负责模型、会话、工具调用和 Agent 执行；VS Code 扩展提供 IDE 内的图形界面、编辑器上下文和快捷操作。

如果你需要完整的终端体验、更多 slash commands、headless 模式、MCP/Skills 深度配置或脚本化工作流，可以继续使用 AtomCode CLI。

## 项目链接

- 官网: https://atomcode.atomgit.com/
- 仓库: https://atomgit.com/atomgit_atomcode/atomcode.git
- License: MIT
