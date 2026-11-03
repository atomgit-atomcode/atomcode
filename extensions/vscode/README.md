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

**🧠 代码理解与解释** — 对选中代码或整个文件做结构化解释，快速讲清逻辑与上下文。

**✏️ 编辑辅助建议** — 基于当前上下文给出修改建议，是否应用完全由你决定。

**🔍 上下文感知分析** — 自动带上当前文件、选区、附件与工作区信息，局部分析不出圈。

**🔌 多模型接入（用户配置）** — 支持 Claude、OpenAI、DeepSeek、GLM、Qwen、Ollama 及任意 OpenAI 兼容 API；也可通过 AtomGit 登录同步 CodingPlan 模型。

**🎛 模型选择器** — 输入区直接切换模型，档位跟随服务端 `/models` 返回的可用清单，默认模型带标记。

**🎚 推理强度（Effort）** — 按当前模型**声明的档位**提供 `low / medium / high / xhigh / max`，模型不支持时该选项自动隐藏，不会出现"选了 Max 却没变化"。

**🛡 四种运行模式** — `Build` / `Accept Edits` / `Auto` / `Plan` 逐级放宽或收紧审批；未选过时沿用 config.toml 的 `default_mode`，新建或切换会话会重新对齐。

**💬 会话管理** — 历史会话按时间分组，支持搜索、恢复、重命名、删除；会话记录落盘，重开 VS Code 可继续。

**📎 附件与图片** — 可附加文件作为上下文，也可直接粘贴图片，点击可放大查看。

**🔗 @ 文件提及** — 输入 `@` 检索工作区路径，把文件/目录精确投喂给模型。

**🧾 变更预览与产物** — 代码变更以 diff 展示、由你确认后落盘；模型输出的代码、表格、artifactId 可在对话内直接展开查看。

**✅ Todo 面板** — 把当前回合的待办计划显示在对话上方，随时看到执行到哪一步。

**🧩 Thinking / reasoning 参数** — 支持配置模型的 thinking 参数（budget、type、keep）。

**🖥 IDE 语言服务桥接** — 把 VS Code 的定义跳转、引用、符号、工作区符号查询能力暴露给 AtomCode，让 agent 用 IDE 自己的语言服务而不是另建索引。

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

**打开与输入**

| 命令 | 说明 |
|------|------|
| `AtomCode: Open in Side Bar` | 侧边栏打开 |
| `AtomCode: Open in New Tab` | 新标签页打开 |
| `AtomCode: Open in Preferred Location` | 按设置的位置打开（`atomcode.preferredLocation`） |
| `AtomCode: Focus Input` | 聚焦输入框 |
| `AtomCode: New Conversation` | 新建会话 |
| `AtomCode: Stop Generation` | 停止当前生成 |

**编辑器操作**

| 命令 | 说明 |
|------|------|
| `AtomCode: Explain Selection` | 解释选中代码 |
| `AtomCode: Fix Selection` | 提供修复建议 |
| `AtomCode: Optimize Selection` | 提供优化建议 |
| `AtomCode: Add to Chat` | 把选中代码作为上下文加入对话 |

**IDE 语言服务桥接**

| 命令 | 说明 |
|------|------|
| `AtomCode：启用 IDE 语言服务桥接（复制配置）` | 开启桥接并复制对端配置 |
| `AtomCode：停用 IDE 语言服务桥接` | 关闭桥接 |
| `AtomCode: IDE 语言服务桥接状态` | 查看桥接是否开启、当前绑定 |
| `AtomCode: 复制当前 IDE 窗口绑定` | 复制当前窗口的绑定信息 |

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

输入框键入 `/` 打开命令菜单。

**内置命令**

| 命令 | 说明 |
|------|------|
| `/login` | 登录 AtomGit，同步 CodingPlan 模型 |
| `/logout` | 退出登录 |
| `/whoami` | 显示当前登录用户 |
| `/status` | 显示会话状态 |
| `/config` | 显示配置文件路径 |
| `/reload` | 重新读取配置文件 |

**技能（Skills）** — 菜单里还会列出当前环境安装的技能（`/<skill-name>`），由本地服务动态提供，带一行说明。技能没声明描述时显示为「运行该技能」。

**菜单交互**：键入文字即按名称过滤（前缀优先、支持子序列匹配，例如 `/kg` 能命中 `karpathy-guidelines`）；`↑/↓` 选择、`Enter`/`Tab` 补全到输入框、`Esc` 收起，选中行会高亮并自动滚动到可视区。

---

## 设置

| 设置项 | 默认值 | 说明 |
|--------|--------|------|
| `atomcode.daemon.port` | `13456` | 首选本地服务端口 |
| `atomcode.daemon.autoStart` | `true` | 是否启用本地服务 |
| `atomcode.daemon.binaryPath` | `""` | 自定义服务路径（留空则自动检测） |
| `atomcode.preferredLocation` | `sidebar` | 默认打开位置 |
| `atomcode.chat.fontFamily` | `""` | 代码块/工具输出/输入框字体，留空跟随 `editor.fontFamily`；可填 CSS font-family 列表 |
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
- 插件只复用与当前内置版本及构建兼容的服务。首选端口被其他版本或无响应服务占用时，会保留已有进程并使用稳定的备用端口；状态栏会提示实际连接端口。同一构建的多个窗口会优先复用服务，不会为了连接而关闭其他 daemon。
- `atomcode.daemon.autoStart=false` 时只查找并连接已有兼容服务，不会启动新服务。

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
