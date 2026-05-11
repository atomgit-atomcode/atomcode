# AtomCode for VS Code

开源 AI 编程 Agent，在编辑器里直接对话、执行工具、修改代码并验证结果。

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="license">
  <img src="https://img.shields.io/badge/vscode-1.85%2B-blue" alt="vscode">
</p>

---

## 功能一览

**对话式 Agent** — 描述任务，AtomCode 自动阅读代码、编辑文件、执行命令、验证结果。

**工具执行可视化** — 实时展示工具调用、参数、输出和耗时；敏感操作需手动确认。

**编辑器深度集成** — 选中代码右键 Explain / Fix / Optimize，自动携带文件名、语言和行号上下文。

**多模型支持** — Claude、OpenAI、DeepSeek、GLM、Qwen、Ollama 及任意 OpenAI 兼容 API。

**会话管理** — 按时间分组的历史会话，支持搜索、恢复、重命名和删除。

**Thinking 模式** — 支持配置模型的 thinking/reasoning 参数（budget、type、keep）。

**Diff 预览** — AI 编辑文件后可查看内联 diff 对比。

**文件附件** — 在对话中附加文件作为额外上下文。

---

## 快速开始

1. 安装扩展并打开 Activity Bar 中的 AtomCode 面板
2. 首次使用选择模型配置方式：
   - **AtomGit 登录** → 同步 CodingPlan 模型（推荐）
   - **手动添加** → 填写 provider name / model / base URL / API key
3. 在输入框描述任务，或选中代码使用右键菜单

---

## 命令

| 命令 | 说明 |
|------|------|
| `AtomCode: Open in Side Bar` | 侧边栏打开 |
| `AtomCode: Open in New Tab` | 新标签页打开 |
| `AtomCode: New Conversation` | 新建会话 |
| `AtomCode: Explain Selection` | 解释选中代码 |
| `AtomCode: Fix Selection` | 修复选中代码 |
| `AtomCode: Optimize Selection` | 优化选中代码 |
| `AtomCode: Stop Generation` | 停止生成 |

---

## 快捷键

| 快捷键 | 说明 |
|--------|------|
| `Cmd+Esc` / `Ctrl+Esc` | 聚焦输入框 |
| `Cmd+Shift+Esc` / `Ctrl+Shift+Esc` | 新标签页打开 |
| `Cmd+Shift+E` / `Ctrl+Shift+E` | 解释选中代码 |
| `Cmd+N` / `Ctrl+N` | 新建会话（输入框聚焦时） |

---

## 斜杠命令

输入框键入 `/` 打开命令菜单：

`/explain` · `/fix` · `/test` · `/refactor` · `/docs` · `/review` · `/login` · `/codingplan`

---

## 设置

| 设置项 | 默认值 | 说明 |
|--------|--------|------|
| `atomcode.daemon.port` | `13456` | Daemon HTTP 端口 |
| `atomcode.daemon.autoStart` | `true` | 自动启动 daemon |
| `atomcode.daemon.binaryPath` | — | 自定义 daemon 路径 |
| `atomcode.preferredLocation` | `sidebar` | 默认打开位置 |
| `atomcode.autoSave` | `true` | AI 读取文件前自动保存 |
| `atomcode.sendWithCtrlEnter` | `false` | Ctrl+Enter 发送 |
| `atomcode.fontSize` | `13` | 聊天面板字号 |
| `atomcode.showInlineHints` | `true` | 显示内联 diff 提示 |

---

## 工作原理

扩展通过本地 AtomCode daemon（HTTP + SSE）通信。Daemon 负责模型调用、Agent 循环、工具执行和会话持久化；扩展提供 IDE 图形界面和编辑器上下文。

Daemon 随扩展自动启动，也可通过 `atomcode daemon` 手动运行。

如需终端体验、MCP 集成或脚本化工作流，请使用 [AtomCode CLI](https://atomgit.com/atomgit_atomcode/atomcode)。

---

## 链接

- [官网](https://atomcode.atomgit.com/)
- [源码仓库](https://atomgit.com/atomgit_atomcode/atomcode)
- [MIT License](./LICENSE)
