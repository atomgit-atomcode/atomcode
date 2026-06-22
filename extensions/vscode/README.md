# AtomCode for VS Code

AtomCode 是一款集成在 VS Code 中的开发辅助工具，用于帮助开发者更高效地理解、编辑与管理代码。

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="license">
  <img src="https://img.shields.io/badge/vscode-1.85%2B-blue" alt="vscode">
</p>

---

## 概述

AtomCode 提供面向开发流程的辅助能力，包括代码理解、上下文分析与编辑建议生成，使开发者能够在编辑器中更高效地完成日常开发任务。所有能力均在用户控制下运行，不改变 VS Code 的默认行为模式。

---

## 核心能力

**🧠 代码理解与解释** — 支持对选中代码或文件进行结构化解释，帮助快速理解逻辑与上下文。

**✏️ 编辑辅助建议** — 基于上下文提供代码修改建议，开发者可手动选择是否应用。

**🔍 上下文感知分析** — 结合当前文件与工作区信息提供局部分析结果，用于辅助决策。

**🔌 多模型接入（用户配置）** — 支持接入不同的模型服务提供方（由用户自行配置 API Key 与服务地址），包括 Claude、OpenAI、DeepSeek、GLM、Qwen、Ollama 及任意 OpenAI 兼容 API。

**📄 修改对比预览** — 所有代码变更均以差异对比形式展示，由用户确认后生效。

**💬 会话管理** — 按时间分组的历史会话，支持搜索、恢复、重命名和删除。

**🧩 Thinking 模式** — 支持配置模型的 thinking/reasoning 参数（budget、type、keep）。

**📎 文件附件** — 在对话中附加文件作为额外上下文。

---

## 快速开始

1. 安装扩展并打开 Activity Bar 中的 AtomCode 面板
2. 首次使用选择模型配置方式：
   - **AtomGit 登录** → 同步 CodingPlan 模型（推荐）
   - **手动添加** → 填写 provider name / model / base URL / API key
3. 在输入框描述任务，或选中代码使用右键菜单
4. 查看建议并手动确认是否应用变更

---

## 命令

| 命令 | 说明 |
|------|------|
| `AtomCode: Open in Side Bar` | 侧边栏打开 |
| `AtomCode: Open in New Tab` | 新标签页打开 |
| `AtomCode: New Conversation` | 新建会话 |
| `AtomCode: Explain Selection` | 解释选中代码 |
| `AtomCode: Fix Selection` | 提供修复建议 |
| `AtomCode: Optimize Selection` | 提供优化建议 |
| `AtomCode: Stop Generation` | 停止当前操作 |

---

## 快捷键

| 快捷键 | 说明 |
|--------|------|
| `Cmd+Esc` / `Ctrl+Esc` | 聚焦输入框 |
| `Cmd+Shift+Esc` / `Ctrl+Shift+Esc` | 新标签页打开 |
| `Cmd+Shift+Alt+E` / `Ctrl+Shift+Alt+E` | 解释选中代码 |
| `Cmd+Shift+Alt+N` / `Ctrl+Shift+Alt+N` | 新建会话（输入框聚焦时） |

---

## 斜杠命令

输入框键入 `/` 打开命令菜单：

`/login` · `/logout` · `/whoami` · `/status` · `/config` · `/reload`

---

## 设置

| 设置项 | 默认值 | 说明 |
|--------|--------|------|
| `atomcode.daemon.port` | `13456` | 本地服务端口 |
| `atomcode.daemon.autoStart` | `true` | 是否启用本地服务 |
| `atomcode.daemon.binaryPath` | — | 自定义服务路径 |
| `atomcode.preferredLocation` | `sidebar` | 默认打开位置 |
| `atomcode.autoSave` | `true` | 操作前自动保存 |
| `atomcode.sendWithCtrlEnter` | `false` | Ctrl+Enter 发送 |
| `atomcode.fontSize` | `13` | 聊天面板字号 |
| `atomcode.showInlineHints` | `true` | 显示内联 diff 提示 |

---

## 运行机制说明

AtomCode 通过本地运行的辅助服务与 VS Code 进行通信，用于处理上下文、会话和模型请求。

- 所有操作均由用户触发
- 扩展不会在后台执行未授权行为
- 用户发送消息、使用选区命令或附加文件时，相关输入、选区或文件内容会进入请求上下文
- 本地服务会根据用户配置调用对应的模型服务提供方
- 本地服务仅用于提升交互体验，随扩展自动启动，也可通过 `atomcode daemon` 手动运行

---

## 权限与行为边界

**AtomCode 可能会：**
- 读取当前打开文件内容（用于上下文分析）
- 读取用户主动附加的文件内容（用于请求上下文）
- 根据用户操作生成修改建议
- 调用用户配置的外部模型服务

**AtomCode 不会：**
- 在未授权情况下执行系统级操作
- 静默修改文件内容
- 在用户未触发对话/命令或未配置模型服务时主动发送代码内容
- 内置任何第三方 API Key 或模型能力

---

## 隐私说明

- 所有模型请求均由用户提供的 API Key 发起
- 用户输入、选区内容和主动附加的文件内容可能作为请求上下文发送至当前配置的模型服务
- AtomCode 不会向未配置或未授权的第三方服务主动发送用户代码内容
- 第三方模型服务的数据策略遵循其自身隐私政策

---

## 安全设计原则

- 用户始终拥有最终控制权
- 模型建议不会静默写入文件；用户通过界面执行应用或插入操作后才会写入当前编辑器
- 所有外部调用均依赖用户配置
- 无后台自动执行逻辑

---

## 链接

- [官网](https://atomcode.atomgit.com/)
- [源码仓库](https://atomgit.com/atomgit_atomcode/atomcode)
- [MIT License](https://atomgit.com/atomgit_atomcode/atomcode/blob/main/LICENSE)
