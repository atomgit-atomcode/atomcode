<div align="center">
<pre>
      _   _                  ____          _
     / \ | |_ ___  _ __ ___ / ___|___   __| | ___
    / _ \| __/ _ \| '_ ` _ \ |   / _ \ / _` |/ _ \
   / ___ \ || (_) | | | | | | |__| (_) | (_| |  __/
  /_/   \_\__\___/|_| |_| |_|\____\___/ \__,_|\___|
</pre>
</div>

<p align="center">
  <strong>用 Rust 编写的开源终端 AI 编码助手</strong>
</p>

<p align="center">
  <a href="./README.md">English</a> · 简体中文
</p>

<p align="center">
  <a href="#安装">安装</a> ·
  <a href="#快速开始">快速开始</a> ·
  <a href="#功能特性">功能</a> ·
  <a href="#架构">架构</a> ·
  <a href="#开发">开发</a> ·
  <a href="#贡献指南">贡献</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-4.15.0-blue" alt="version">
  <img src="https://img.shields.io/badge/rust-1.75%2B-orange" alt="rust">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="license">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows-lightgrey" alt="platform">
</p>

---

> **本项目 100% 由 AI 生成。** 每一行代码、每一个架构决策的实现、每一次提交都由 AI 完成。人类开发者仅担任决策者和产品经理的角色——定义"要做什么"，而不是"怎么做"。

---

AtomCode 是一款住在你终端里的 AI 编码助手。用自然语言给它一个任务，它会自动阅读代码、编辑文件、执行命令、验证结果——全程自主完成。

你可以把它理解为 Claude Code / Cursor Agent 的开源替代品，完全运行在终端里，并且可以接入任何兼容 OpenAI 接口的模型。

## 功能特性

### Agent 循环

- **自主多步执行** —— 读文件、改代码、跑测试、修错误，循环直到完成
- **验证回路** —— 每次编辑后自动跑语法检查确认无误，才算任务完成
- **动态步数预算** —— 根据编辑文件数动态放宽步数上限，同时封顶以控成本
- **循环检测** —— 识别并打破重复调用同一工具的死循环
- **三层 JSON 修复** —— 修复畸形工具调用参数
- **Turn 级 datalog** —— 结构化记录每一轮工具调用，便于回放、调试和评测

### 内置工具

文件与 Shell：

- `read_file`、`write_file`、`edit_file`、`search_replace`
- `bash`、`grep`、`glob`、`list_directory`、`change_dir`
- `web_search`、`web_fetch`

代码图谱（语言感知的代码智能）：

- `list_symbols`、`read_symbol`、`find_references`
- `trace_callers`、`trace_callees`、`trace_chain`
- `file_deps`、`blast_radius`

自动化：

- `auto_fix` —— 自动 lint / 类型检查修复循环
- `use_skill` —— 调用用户自定义 skill

### 多模型支持

支持任何实现了 OpenAI function calling 接口的模型：

| 提供方 | Function Calling | 已验证模型 |
|----------|:---:|---|
| Claude（Anthropic） | 支持 | Claude Sonnet 4.5/4.6、Opus 4.6 |
| OpenAI | 支持 | GPT-4o、GPT-4.1 |
| DeepSeek | 支持 | DeepSeek V3、DeepSeek R1 |
| 智谱（GLM） | 支持 | GLM-4、GLM-5 |
| 通义千问（阿里） | 支持 | Qwen-Plus、Qwen-Max |
| SiliconFlow | 支持 | 多种开源模型 |
| Ollama（本地） | 部分支持 | Llama 3、Qwen2 等 |
| 任意 OpenAI 兼容接口 | 支持 | — |

### 会话与登录

- **持久化会话** —— 每次对话都会保存，用 `/resume` 恢复或切换
- **AtomGit OAuth 登录** —— `/login`（或 `atomcode login`）将 CLI 与你的 AtomGit 账号绑定
- **SSO 登录** —— `/login-with-sso`，GitCode 内部用户使用
- **Headless 模式** —— `atomcode -p "..."` 非交互式跑一条 prompt，结果直接输出到 stdout（类似 Claude Code 的 `-p`）
- **Daemon 模式** —— `atomcode-daemon` 提供 HTTP API，用于查询会话历史和 SSE 流式对话

### 终端 UI

- **实时流式输出** —— Markdown 渲染 + 语法高亮
- **代码块** —— 语言标签、行号、`base16-ocean.dark` 主题
- **多行输入** —— Shift+Enter 换行、高度自适应、历史记录
- **文本选择** —— 鼠标拖选、自动滚动、复制到剪贴板
- **斜杠命令** —— `/model`、`/provider`、`/resume`、`/diff`、`/undo`、`/cost`、`/clear`、`/compact` 等（完整列表见下）
- **文件附加** —— 粘贴文件路径即可把内容作为上下文带入
- **Bracketed paste** —— 长文本粘贴自动折叠为紧凑的指示器
- **Skills** —— 从 skill 目录加载的用户自定义命令，像普通斜杠命令一样调用

### 安全性

- **破坏性命令检测** —— `rm -rf`、`git push --force`、`DROP TABLE` 等需要显式确认
- **敏感文件保护** —— 写入 `/etc`、`~/.ssh`、shell 配置文件需要确认
- **按会话的权限授予** —— 单条工具模式一次授权，或设为始终允许
- **源码文件删除必须确认** —— 对代码文件执行 `rm` 从不自动放行
- **撤销** —— `/undo` 通过文件历史快照回滚上一轮的所有文件编辑

## 安装

### 从源码构建（推荐）

```bash
git clone https://atomgit.com/atomgit_atomcode/atomcode.git
cd atomcode
cargo build --release
```

产物位于 `target/release/atomcode`，将其加入 PATH：

```bash
# macOS / Linux
cp target/release/atomcode ~/.local/bin/
# 或
sudo cp target/release/atomcode /usr/local/bin/
```

### 依赖

- Rust 1.75+（用于构建）
- 任一支持的模型提供方的 API Key（或使用 `/login` 的 AtomGit 账号）

## 快速开始

### 1. 首次运行

```bash
atomcode
```

首次运行会有一个向导帮你配置模型：

```
Welcome to AtomCode! Let's set up your first provider.

Select provider:
  [1] Claude (Anthropic)
  [2] OpenAI
  [3] OpenAI Compatible (DeepSeek, Qwen, Zhipu, Moonshot...)
  [4] Ollama (local)
```

### 2. 配置

配置文件位于 `~/.atomcode/config.toml`，最小单 provider 样例：

```toml
default_provider = "deepseek"

[providers.deepseek]
type           = "openai"
api_key        = "sk-..."
model          = "deepseek-chat"
base_url       = "https://api.deepseek.com/v1"
context_window = 64000
```

可以配置多个 provider，用 `/model` 或 `/provider` 切换。完整示例
（涵盖 Claude / OpenAI / OpenAI-兼容 endpoint 如 DeepSeek / GLM /
SiliconFlow / OpenRouter / Ollama，以及 `[datalog]` 段）见
[`docs/config.example.toml`](docs/config.example.toml)——拷出来按需改。

手动改完 `config.toml` 后，在 atomcode 里执行 `/reload` 重新加载配置，
不用重启。

### 3. 开始编码

```bash
# 在项目目录下启动
cd your-project
atomcode

# 或指定目录
atomcode -C /path/to/project

# 或指定模型
atomcode --model gpt-4o

# Headless 模式（单条 prompt，结果输出到 stdout）
atomcode -p "简要说明这个仓库的 agent loop"

# 从文件读取 prompt
atomcode --prompt-file task.md
```

然后直接用自然语言描述你想做的事：

```
> 修复 OAuth 回调后用户被重定向到 404 的登录 bug

> 给设置页加一个深色模式切换

> 把数据库模块重构为使用连接池

> 给支付处理模块写单元测试
```

## 快捷键

### 输入

| 键位 | 动作 |
|-----|--------|
| `Enter` | 发送消息 |
| `Shift+Enter` | 换行 |
| `Esc` | 清空输入 / 取消流式输出 |
| `Up/Down` | 浏览输入历史 |
| `Tab` | 接受补全 |
| `Ctrl+U` | 清空当前行 |
| `Ctrl+W` | 删除一个单词 |
| `Ctrl+K` | 删除到行尾 |

### 导航

| 键位 | 动作 |
|-----|--------|
| `Ctrl+Up/Down` | 滚动聊天区（3 行） |
| `PageUp/PageDown` | 滚动聊天区（一页） |
| `Ctrl+L` | 清空对话 |
| `Ctrl+Shift+C` | 复制选中内容 |
| `Ctrl+C` | 取消当前操作（连按两次退出） |

### 斜杠命令

| 命令 | 动作 |
|---------|--------|
| `/resume` | 恢复或切换会话 |
| `/session` | 创建新会话 |
| `/provider` | 管理 provider |
| `/model` | 切换模型 / provider |
| `/login` | 通过 AtomGit OAuth 登录 |
| `/cd` | 切换工作目录 |
| `/undo` | 撤销上一轮的文件编辑 |
| `/diff` | 显示当前修改的 git diff |
| `/cost` | 显示本次会话的 token 消耗 |
| `/copy` | 复制 AI 的最后一条回复 |
| `/clear` | 清空对话 |
| `/issue` | 在 AtomGit 上创建 issue |
| `/config` | 编辑配置文件 |
| `/status` | 查看登录状态和模型信息 |
| `/logout` | 退出 AtomGit 登录 |
| `/help` | 查看命令与快捷键 |
| `/quit` | 退出程序（或连按 Ctrl+C） |

## 架构

AtomCode 是一个 Rust workspace，由四个 crate 组成：

```
atomcode/
  crates/
    atomcode-core/     # 无头核心库，不依赖 TUI
      agent/           # AgentLoop：自主工具调用循环
      turn/            # TurnRunner、datalog、权限决策器
      config/          # 配置加载、provider 配置
      conversation/    # 消息类型、窗口化上下文
      provider/        # LlmProvider trait + OpenAI/Claude/Ollama
      tool/            # Tool trait + 内置工具实现
      session/         # 持久化会话
      skill.rs         # 用户自定义 skill

    atomcode-tui/      # 终端 UI（ratatui + crossterm）
      app.rs           # App 状态机
      ui/              # 渲染：聊天、输入、状态栏、Markdown

    atomcode-cli/      # 可执行入口（TUI + headless -p 模式）
      main.rs          # CLI 参数、首次运行向导、启动
      auth/            # AtomGit OAuth 客户端

    atomcode-daemon/   # 基于 atomcode-core 的 HTTP/SSE API 服务
```

### 设计原则

1. **技术栈无关** —— 核心引擎不硬编码任何特定语言的逻辑，通过 `package.json`、`Cargo.toml`、`pyproject.toml`、`pom.xml` 等描述文件动态探测项目类型。

2. **Agent 解耦** —— `AgentLoop` 作为独立的异步任务运行，通过 channel（`AgentCommand` / `AgentEvent`）与 TUI 通信。核心库完全不依赖 TUI，这也是 daemon 得以存在的基础。

3. **工具安全** —— 所有破坏性操作必须经用户显式确认。工具失败会作为 observation 返回给模型，绝不 panic。

4. **上下文感知** —— token 预算感知的会话窗口、项目文件树注入、每轮系统提醒，在不超出上下文限制的同时让模型保持专注。

## 项目指令文件

在项目根目录创建 `.atomcode.md` 文件，给 AtomCode 提供持久化上下文：

```markdown
# Project Instructions

本项目是 Vue 3 + TypeScript，使用 Pinia 做状态管理。

- 始终使用 `<script setup>` 风格的 Composition API
- 样式使用 TailwindCSS，不写内联样式
- 编辑 .vue/.ts 文件后运行 `npm run lint`
```

AtomCode 会自动读取这个文件并注入到系统提示中。

## 开发

### 前置条件

- **Rust 1.75+** —— 通过 [rustup](https://rustup.rs/) 安装
- **Git**
- 任一支持的模型 API Key（用于运行时测试）

### 从源码构建

```bash
git clone https://atomgit.com/atomgit_atomcode/atomcode.git
cd atomcode

# Debug 构建（编译快、运行慢）
cargo build

# Release 构建（编译慢、运行快）
cargo build --release
```

### 开发时运行

```bash
# 直接运行 TUI（debug 模式）
cargo run -p atomcode-cli

# 带参数
cargo run -p atomcode-cli -- -C /path/to/project
cargo run -p atomcode-cli -- --model gpt-4o

# Headless 模式
cargo run -p atomcode-cli -- -p "总结一下这个仓库"

# Daemon（HTTP API）
cargo run -p atomcode-daemon
```

### 测试

```bash
# 运行全部测试
cargo test

# 运行指定 crate 的测试
cargo test -p atomcode-core
cargo test -p atomcode-tui

# 运行指定的用例
cargo test -p atomcode-core test_name
```

### 常用命令

```bash
# 只做类型检查，不生成产物
cargo check

# 格式化代码
cargo fmt

# 运行 linter
cargo clippy

# 构建并安装到 ~/.cargo/bin
cargo install --path crates/atomcode-cli
```

## 贡献指南

欢迎贡献！AtomCode 正在积极迭代中。

### 如何贡献

1. 在 AtomGit 上 **Fork** 仓库
2. 克隆你的 fork：
   ```bash
   git clone https://atomgit.com/<你的用户名>/atomcode.git
   cd atomcode
   ```
3. 创建分支：
   ```bash
   git checkout -b feat/your-feature
   # 或
   git checkout -b fix/your-bugfix
   ```
4. 修改代码，确保能编译、测试通过：
   ```bash
   cargo build && cargo test && cargo clippy
   ```
5. 清晰地写 commit：
   ```bash
   git commit -m "feat: add xxx support"
   ```
6. **Push** 并向 `main` 分支提交 **Pull Request**

### 分支命名

| 前缀 | 用途 |
|--------|---------|
| `feat/` | 新功能 |
| `fix/` | Bug 修复 |
| `refactor/` | 重构（不改变行为） |
| `docs/` | 仅文档 |
| `chore/` | 构建、CI、工具链 |

### 约定

- 遵守项目的核心原则，尤其是 **技术栈中立**
  （核心引擎中不写任何针对特定语言/框架的逻辑；通过
  `package.json` / `Cargo.toml` / `pom.xml` 等探测，并通过 adapter 分发）
- 工具失败必须优雅处理——把错误作为 observation 返回给模型，绝不 panic
- 破坏性操作必须需要用户确认
- 系统提示保持紧凑（约 1.5K tokens）
- 提交前先跑 `cargo fmt` 和 `cargo clippy`

### 从哪里上手

- **新增工具** —— 在 `crates/atomcode-core/src/tool/` 下实现 `Tool` trait
- **新增模型提供方** —— 在 `crates/atomcode-core/src/provider/` 下实现 `LlmProvider`
- **改进 UI** —— 渲染相关代码在 `crates/atomcode-tui/src/ui/`
- **修 Bug** —— 到 [Issues](https://atomgit.com/atomgit_atomcode/atomcode/issues) 上挑一个

## 许可证

MIT License。详见 [LICENSE](LICENSE)。

---

<p align="center">
  用 Rust、ratatui 以及无数个深夜构建而成。
</p>
