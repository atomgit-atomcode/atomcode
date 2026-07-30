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
  <a href="#贡献指南">贡献</a> ·
  <a href="#社区交流">社区</a>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/version-5.0.3-blue" alt="version">
  <img src="https://img.shields.io/badge/rust-1.88%2B-orange" alt="rust">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="license">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20HarmonyOS%20PC%20%7C%20Windows-lightgrey" alt="platform">
    <a href="https://atomgit.com/atomgit_atomcode/atomcode" target="_blank">
    <img src="https://atomgit.com/atomgit_atomcode/atomcode/star/badge.svg" alt="AtomGit Star"/>
  </a>
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

### 模式与自主

- **Plan / Build 模式** —— `/plan` 切换到只读探索模式（agent 只调研、不改文件），`/build` 切回完整执行
- **目标模式** —— `/goal <目标>` 设定完成条件后，agent 会一轮接一轮自动循环执行，直到目标达成
- **代码审查** —— `/review` 审查当前改动，`/review staged` 审查暂存区，`/review <base>` 对比某个基准 ref
- **后台会话** —— `/bg` 把任务放到分离的槽位执行，长任务进行时你仍可继续使用 TUI

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
| DeepSeek | 支持 | DeepSeek V3、DeepSeek R1、DeepSeek V4 |
| 智谱（GLM） | 支持 | GLM-4、GLM-5、GLM-5.2（AtomCode Pro 套餐专属模型） |
| 通义千问（阿里） | 支持 | Qwen-Plus、Qwen-Max |
| SiliconFlow | 支持 | 多种开源模型 |
| Ollama（本地） | 部分支持 | Llama 3、Qwen2 等 |
| 任意 OpenAI 兼容接口 | 支持 | — |

### 会话与登录

- **持久化会话** —— 每次对话都会保存；命令行可用 `atomcode --continue` 或 `-c` 继续上一次会话，在 TUI 内可用 `/resume` 恢复或切换
- **AtomGit OAuth 登录** —— `/login`（或 `atomcode login`）将 CLI 与你的 AtomGit 账号绑定
- **SSO 登录** —— `/login-with-sso`，GitCode 内部用户使用
- **Headless 模式** —— `atomcode -p "..."` 非交互式跑一条 prompt，结果直接输出到 stdout（类似 Claude Code 的 `-p`）；需要确认的 `bash` 会自动批准，其他需要确认的工具会被拒绝
- **Daemon 模式** —— `atomcode-daemon` 提供 HTTP API，用于查询会话历史和 SSE 流式对话

### 终端 UI

- **实时流式输出** —— Markdown 渲染 + 语法高亮
- **代码块** —— 语言标签、行号、`base16-ocean.dark` 主题
- **多行输入** —— Shift+Enter 或 `\` + Enter 换行、高度自适应、历史记录
- **任务完成通知** —— 长任务结束后优先走终端原生通知协议，必要时回退到系统通知
- **文本选择** —— 鼠标拖选、自动滚动、复制到剪贴板
- **斜杠命令** —— `/model`、`/provider`、`/resume`、`/bg`、`/diff`、`/undo`、`/cost`、`/clear`、`/compact` 等（完整列表见下）
- **文件附加** —— 粘贴文件路径即可把内容作为上下文带入
- **Bracketed paste** —— 长文本粘贴自动折叠为紧凑的指示器
- **Skills** —— 从 skill 目录加载的用户自定义命令，像普通斜杠命令一样调用

### Web UI

- **`/webui`**（TUI 内）或 **`atomcode webui`**（命令行）会在浏览器里打开一个本地 Web 界面，作为终端界面之外的另一种选择——同一个 agent、同一份会话，渲染在浏览器中
- **仅本地回环** —— server 绑定 `127.0.0.1` 并使用一次性 token，不对网络暴露
- **`/webui stop`** 停止进程内 server（之后再次 `/webui` 会重新启动）

### App 远程访问

- **`/app`**（TUI 内）开启移动端远程访问，终端打印二维码，用手机 GitCode App 扫码即可在任意网络下连入当前对话
- **任意网络可达** —— 电脑通过反向 WSS 隧道连接到公网中继，手机经中继访问电脑，不需要公网 IP、DDNS 或路由器端口映射
- **双向实时同步** —— 任一端发消息，另一端实时显示（AI 流式回复、工具调用卡片、token 用量）
- **远程命令** —— 手机端支持 `/status`、`/cost`、`/diff`、`/whoami` 等斜杠命令，在桌面端执行并回显
- **切项目 / 切会话** —— 手机端切换项目或点开历史对话，桌面端跟随切换
- **模型双向同步** —— 任一端切换模型，另一端同步跟随
- **`/app stop`** 断开远程访问

### 安全性

- **破坏性命令检测** —— `rm -rf`、`git push --force`、`DROP TABLE` 等需要显式确认
- **按路径分层确认** —— 工作区外读取、敏感路径访问、以及所有工作区外写入会按风险等级请求确认
- **敏感文件保护** —— 系统保护路径、凭证目录、shell 配置、`.env` 文件、密钥/证书文件会触发更强的确认规则
- **Shell 绕过防护** —— `cat`、`head`、`ls`、`cp`、`mv`、`tee` 等常见 shell 文件命令会继承和文件工具一致的路径审批模型
- **按会话的权限授予** —— 单条工具模式一次授权，或设为始终允许
- **源码文件删除必须确认** —— 对代码文件执行 `rm` 从不自动放行
- **撤销** —— `/undo` 通过文件历史快照回滚上一轮的所有文件编辑

完整设计与当前边界见 [权限模型](./docs/security/permission-model.md)。

### 隐私

- 📊 匿名遥测（默认开启，可关闭）— 详见 [docs/telemetry.md](docs/telemetry.md)

## 安装

### 从源码构建（推荐）

```bash
git clone https://atomgit.com/atomgit_atomcode/atomcode.git
cd atomcode
cargo install --path crates/atomcode-cli --locked
```

编译产物位于 `target/release/atomcode`。在 macOS / Linux / HarmonyOS PC 其被安装到 `~/.cargo/bin/atomcode`，
在 Windows 系统上其被安装到 `$env:USERPROFILE/.cargo/bin/atomcode.exe`。请确保 `~/.cargo/bin`
（或 `%USERPROFILE%\.cargo\bin`）已经被添加到 `PATH` 环境变量中。

如果只想要编译，不要安装，运行：

```bash
cargo build --release
```

编译产物会在 `target/release/atomcode` 生成。

### 包管理器安装

除了从源码构建外，AtomCode CLI 也可以通过以下包管理器安装：

```bash
# 使用 npm 安装
npm install -g @atomgit.com/atomcode

# 使用 Homebrew 安装
brew install --cask atomcode
```

### Shell 补全

AtomCode 可为 Bash、Zsh、Fish、PowerShell 和 Elvish 生成补全脚本。例如：

```bash
# Bash（当前会话）
source <(atomcode completion bash)

# Zsh（持久生效）
mkdir -p ~/.zfunc
atomcode completion zsh > ~/.zfunc/_atomcode
# 同时在 ~/.zshrc 的 `compinit` 之前加入：fpath=(~/.zfunc $fpath)

# Fish（持久生效）
mkdir -p ~/.config/fish/completions
atomcode completion fish > ~/.config/fish/completions/atomcode.fish
```

PowerShell 可运行 `atomcode completion powershell | Out-String |
Invoke-Expression`。完整 Shell 列表见 `atomcode completion --help`。该能力只作用于
外部命令行，不会改变 TUI 内 `Tab` 的模式切换行为。

### 依赖

- Rust 1.88+（用于构建；更旧的 Cargo 无法解析当前 lock 文件）
- 任一支持的模型提供方的 API Key（或使用 `/login` 的 AtomGit 账号）

### 权限 —— 不要用 `sudo` 启动

请用**普通用户**运行 AtomCode，切勿 `sudo`。AtomCode 把配置、会话、日志都放在
`~/.atomcode`；一旦用 root 跑过一次，就会在那里留下 root 属主的文件，之后非 root
启动会在运行时初始化阶段报错：

```
coding runtime assemble failed: Permission denied (os error 13)
```

（提示里可能是 `prepare` 而非 `assemble`——同一个原因。）遇到这种情况，把属主收回
并停止使用 `sudo`：

```bash
sudo chown -R "$(id -un):$(id -gn)" ~/.atomcode
atomcode        # 不要再加 sudo
```

在 Linux 客户机上，工作目录若在 VirtualBox 共享文件夹（`/media/sf_*`，属主
`root:vboxsf`）也会导致权限错误——用 `sudo usermod -aG vboxsf "$USER"` 把自己加进
该组（重新登录后生效），而不是用 `sudo`。

### 卸载

移除 AtomCode 及（可选）其数据：

```bash
atomcode uninstall                # 交互模式：分组询问
atomcode uninstall --keep-data    # 仅删除二进制 + PATH 配置
atomcode uninstall --purge        # 一并删除 ~/.atomcode/
atomcode uninstall --dry-run      # 仅打印计划，不实际删除
```

二进制已损坏或丢失时使用兜底脚本：

```bash
curl -fsSL https://raw.atomgit.com/atomgit_atomcode/atomcode/raw/main/scripts/uninstall.sh | sh
# Windows:
irm https://raw.atomgit.com/atomgit_atomcode/atomcode/raw/main/scripts/uninstall.ps1 | iex
```

默认保留凭据（`auth.toml`、`mcp.json`、`config.toml`、`ATOMCODE.md`），传 `--purge` 才会一起清除。

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

在 headless 模式下，需要确认的 `bash` 会自动批准并写到 stderr；其他需要确认的工具会被拒绝。

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
| `Shift+Enter` | 换行（需要终端支持 Kitty 键盘协议） |
| `Ctrl+Enter` | 换行（需要终端支持 Kitty 键盘协议） |
| `Alt+Enter` | 换行（多数终端可用，见下方兼容性说明） |
| `\` + `Enter` | 换行（所有终端通用——输入一个 `\` 后按回车，`\` 会被自动删除） |
| `Esc` | 清空输入 / 取消流式输出 |
| `Esc` ×2 | 撤销上一轮 |
| `Up/Down` | 浏览输入历史 |
| `Tab` | 接受补全 |
| `Ctrl+U` | 清空当前行 |
| `Ctrl+W` | 删除一个单词 |
| `Ctrl+K` | 删除到行尾 |
| `Ctrl+V` | 从剪贴板粘贴图片（Windows 下请改用 `/paste`，见下方说明） |

> **换行快捷键的终端兼容性：**
> - `Shift+Enter`、`Ctrl+Enter` 需要终端支持 Kitty 键盘协议 — kitty、WezTerm、Alacritty、iTerm2 ≥3.5、Windows Terminal ≥1.21。不支持的终端（以及 Windows，atomcode 在其上不启用该协议）会把它们退化成普通 `Enter`（直接发送消息）—— 请改用 `\` + `Enter`，它在所有终端都生效。
> - `Alt+Enter` 在多数终端的字节层面就能工作，但 **Windows Terminal 默认把它绑给"切换全屏"** — 在 设置 → 操作 中删掉那条绑定即可释放。
> - Xshell 不支持 Kitty 协议；可在键盘映射设置中把某个空闲组合映射为发送 `ESC, Enter`（`\x1b\r`）达到同样效果，或直接从剪贴板粘贴多行文本（已启用 bracketed paste）。

> **Windows 下粘贴图片：**
> Windows Terminal 和 conhost 默认把 `Ctrl+V` 绑给它们自己的 `paste` action — 这个 action 只会从剪贴板读 `CF_UNICODETEXT`，剪贴板上只有图片时它什么都不会发，应用里的 `Ctrl+V` 处理器根本收不到事件。两种解法：
> 1. 使用 **`/paste`** —— 这个斜杠命令直接读取剪贴板图片并以 `[Image #N]` 的形式附加到输入框，在 Windows Terminal、PowerShell 7、conhost、git bash 等所有终端里都能正常工作。Windows 版的 TUI 右下角会自动显示 `剪贴板有图片 · /paste 粘贴` 作为提示。
> 2. 若想保留 `Ctrl+V` 的肌肉记忆：打开 Windows Terminal 的 `settings.json`（`Ctrl+,` → 右下角"打开 JSON 文件"），在 `"actions"` 数组里删掉 `{ "command": "paste", "keys": "ctrl+v" }`，或把它改绑到 `ctrl+shift+v`。重启 Windows Terminal 后，`Ctrl+V` 就能透传给 atomcode 了。
>
> Git Bash（MinTTY）不拦截 `Ctrl+V`，开箱即用。

### 导航

| 键位 | 动作 |
|-----|--------|
| `Ctrl+Up/Down` | 滚动聊天区（3 行） |
| `PageUp/PageDown` | 滚动聊天区（一页） |
| `Ctrl+L` | 清空对话 |
| `Ctrl+Shift+C` | 复制选中内容 |
| `Ctrl+C` | 取消当前操作（连按两次退出） |

### 斜杠命令

在 TUI 中输入 `/` 即可浏览完整列表并实时补全；`/help` 会列出命令与快捷键。

**会话与工作区**

| 命令 | 动作 |
|---------|--------|
| `/resume` | 恢复或切换会话 |
| `/session` | 创建新会话 |
| `/rename <名称>` | 重命名当前会话 |
| `/clear` | 开始新对话（清空上下文与屏幕） |
| `/bg` | 将当前会话放到后台；子命令：`/bg list`、`/bg <N>`、`/bg drop <N>`、`/bg help` |
| `/background <task>` | 兼容入口：在 `/bg` 槽位中启动一次性后台任务 |
| `/cd` | 切换工作目录并开启新建对话 |
| `/worktree` | Git worktree 隔离（`create` / `list` / `done` / `cleanup`） |
| `/webui` | 启动浏览器 webui（子命令：`stop`、`lan`、`--host <地址>`） |
| `/sync` | 连接到实时 webui 会话（`/sync off` 断开） |

**模式、自主与审查**

| 命令 | 动作 |
|---------|--------|
| `/plan` | 切换到 Plan 模式（只读探索） |
| `/build` | 切换到 Build 模式（完整执行） |
| `/goal <目标>` | 设置完成目标——agent 自动循环执行直到条件满足 |
| `/review` | 代码审查当前改动（`/review` · `/review staged` · `/review <base>`） |
| `/think` | 控制深度思考（on / off / budget N） |
| `/effort` | DeepSeek 推理努力控制（high / max / off） |

**Provider 与账号**

| 命令 | 动作 |
|---------|--------|
| `/model` | 切换模型 / provider |
| `/provider` | 管理 provider（添加 / 编辑 / 删除） |
| `/proxy` | 切换出站代理模式 |
| `/login` | 通过 AtomGit OAuth 登录并申领 CodingPlan 免费模型 |
| `/logout` | 退出 AtomGit 登录 |
| `/whoami` | 查看当前登录用户 |
| `/status` | 查看登录状态和模型信息 |

**文件、编辑与上下文**

| 命令 | 动作 |
|---------|--------|
| `/diff` | 显示当前修改的 git diff |
| `/undo` | 撤销某一轮的文件编辑（`/undo` 或 `/undo N`） |
| `/view <文件路径>` | 在浮层窗口中查看文件内容 |
| `/paste` | 从剪贴板粘贴图片（Windows 下 Ctrl+V 被终端拦截时的备用入口） |
| `/copy` | 从上一条回复复制代码块（`/copy`、`/copy N`、`/copy all`） |
| `/cost` | 显示本次会话的 token 消耗 |
| `/context` | 查看上下文预算占用明细 |
| `/compact` | 压缩对话历史 |

**记忆**

| 命令 | 动作 |
|---------|--------|
| `/remember <事实>` | 保存一条记忆（`--global` 对所有项目生效） |
| `/forget <关键词>` | 删除匹配的记忆 |
| `/memory` | 查看所有已保存的记忆 |

**扩展**

| 命令 | 动作 |
|---------|--------|
| `/mcp` | MCP 服务状态（子命令：`reload`、`tools`、`login`、`logout`） |
| `/plugin` | 插件市场（`marketplace` / `install` / `uninstall` / `list`） |
| `/skills` | 浏览已加载的 skills |

**项目与系统**

| 命令 | 动作 |
|---------|--------|
| `/init` | 根据工作目录生成 `.atomcode.md` 项目指令 |
| `/config` | 显示配置文件路径 |
| `/reload` | 从磁盘重新加载 `~/.atomcode/config.toml` |
| `/upgrade` | 升级 atomcode 到最新版（子命令：`rollback`） |
| `/setup` | 首次运行：安装推荐 skill 并执行 |
| `/welcome` | 重新运行引导向导 |
| `/language` | 切换显示语言及默认 Git 提交消息语言 |
| `/guide <问题>` | 向 atomcode-guide 询问使用方式 |
| `/keys` | 查看键盘快捷键 |
| `/help` | 查看命令与快捷键 |
| `/quit`、`/exit` | 退出 AtomCode（或连按 Ctrl+C） |

> **AtomGit Issue**：`/issue` 已移除。执行 `/login` 后，直接用自然语言提出需求即可，例如“为这个 Bug 创建一个 AtomGit Issue”，AtomCode 会调用内置的 `atomgit_issue` 工具。读取 Issue 可直接执行；创建 Issue，以及新增、编辑或删除评论仍需权限确认。
>
> **插件命令**：除了上面的内置命令，插件还能注册自己的斜杠命令。例如安装官方频道插件后即可使用 `/wechat`（显示 AtomCode 微信用户群二维码）：
>
> ```text
> /plugin marketplace add https://atomgit.com/atomgit_atomcode/AtomCode-Channel
> /plugin install weixin@atomcode-channel
> ```

### 自定义命令

除了内置命令和插件命令，你还可以通过 `.md` 模板文件定义自己的斜杠命令，适用于频繁使用的提示词。

**存放位置**（按优先级从低到高）：

| 位置 | 作用域 |
|-----|--------|
| `$ATOMCODE_HOME/commands/`（默认为 `~/.atomcode/commands/`） | 全局 —— 所有项目生效 |
| `<project>/.atomcode/commands/` | 项目级 —— 覆盖同名的全局命令 |
| `plugins/<name>/commands/` | 插件贡献 —— 通过 `/plugin install` 安装 |

**文件格式**：

```markdown
---
name: explain
description: 解释指定函数或模块的工作原理
args: required
---

请详细解释以下代码的工作原理：

$ARGUMENTS

包括：函数签名与参数含义、核心业务逻辑、数据流与副作用。
```

- **`name`** —— 必填。命令名，输入 `/explain` 触发。
- **`description`** —— 可选。Tab 补全时显示。
- **`args`** —— 可选。控制参数期望与交互行为：

  | 值 | 菜单 Enter 行为 | 空参提交 |
  |-----|----------------|---------|
  | `none`（默认） | 立即执行 | 允许（替换为空字符串） |
  | `optional` | 补全到 `/name `，等待输入 | 允许 |
  | `required` | 补全到 `/name `，等待输入 | 拒绝并提示错误 |

  模板变量 `$ARGUMENTS` / `${ARGUMENTS}` 始终替换为用户在命令名后输入的内容（未输入则为空字符串）。

- **模板正文** —— 输入命令后发送给 AI 的提示词。`$ARGUMENTS` 或 `${ARGUMENTS}` 会被替换为用户输入的命令参数。

**示例：创建一个审查命令**

```bash
mkdir -p .atomcode/commands

cat > .atomcode/commands/codereview.md << 'EOF'
---
name: codereview
description: 对当前 git diff 进行代码审查
args: optional
---

请对当前 git diff 中的所有改动进行代码审查。
如有指定文件则只审查: $ARGUMENTS
EOF
```

输入 `/help commands` 可查看所有已加载的自定义命令。

> **优先级规则**：自定义命令名不能覆盖同名内置命令。如果内置已有 `/review`，项目级自定义的 `review.md` 不会出现在补全菜单中，也不会被 dispatch。

## 架构

AtomCode 是一个分层的 Rust workspace：

```
atomcode/
  crates/
    atomcode-kernel/        # 中立 agent 循环与运行时 trait
    atomcode-capabilities/  # provider、tools、MCP、skills、session、memory
    atomcode-coding/        # coding 专业化与 CodingRuntime 生命周期
    atomcode-review/        # 代码评审专业化
    atomcode-tuix/          # 终端 UI
    atomcode-cli/           # TUI 与 headless 入口
    atomcode-daemon/        # HTTP/SSE/WebSocket 传输层及历史 session importer
```

coding 主调用链是 `CLI/TUI/daemon → CodingRuntime → kernel`。已经退役的 core agent
协议和 `atomcode-bridge` 不再位于运行时路径中。

### 设计原则

1. **技术栈无关** —— 核心引擎不硬编码任何特定语言的逻辑，通过 `package.json`、`Cargo.toml`、`pyproject.toml`、`pom.xml` 等描述文件动态探测项目类型。

2. **单一运行时所有者** —— `CodingRuntime` 统一拥有 live coding agent、provider/session 生命周期、pending request、snapshot 和 controller。driver 只负责输入、展示和传输，不重建第二套 agent runtime。

3. **工具安全** —— 所有破坏性操作必须经用户显式确认。工具失败会作为 observation 返回给模型，绝不 panic。

4. **上下文感知** —— token 预算感知的会话窗口、项目文件树注入、每轮系统提醒，在不超出上下文限制的同时让模型保持专注。

5. **依赖单向** —— kernel 保持中立；capabilities 和 coding 保持 core-free；历史 session 数据只在显式兼容边界处理，不作为 runtime fallback。

## 项目指令文件

在项目根目录创建 `.atomcode.md` 文件，给 AtomCode 提供持久化上下文：

```markdown
# Project Instructions

本项目是 Vue 3 + TypeScript，使用 Pinia 做状态管理。

- 始终使用 `<script setup>` 风格的 Composition API
- 样式使用 TailwindCSS，不写内联样式
- 编辑 .vue/.ts 文件后运行 `npm run lint`
```

AtomCode 会自动读取这个文件并注入到系统提示中。AtomCode 也支持 `AGENTS.md`（AI 编程代理的[开放标准](https://agents.md/)）作为替代——如果两个文件同时存在，`.atomcode.md` 优先。

## 开发

### 前置条件

- **Rust 1.88+** —— 通过 [rustup](https://rustup.rs/) 安装
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
cargo test -p atomcode-capabilities
cargo test -p atomcode-tuix

# 运行指定的用例
cargo test -p atomcode-capabilities test_name
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

- **新增工具** —— 在 `crates/atomcode-capabilities/src/tools/` 下实现 `Tool` trait
- **新增模型提供方** —— 在 `crates/atomcode-capabilities/src/provider/` 下实现 `LlmProvider`
- **改进 UI** —— 渲染相关代码在 `crates/atomcode-tuix/src/render/`
- **修 Bug** —— 到 [Issues](https://atomgit.com/atomgit_atomcode/atomcode/issues) 上挑一个

### 非 Rust 贡献者

不会 Rust？没关系！有很多方式可以不写 Rust 代码就能参与贡献：

- **📝 文档** — 改进 README、修正错别字、完善[官方文档站](https://atomcode.atomgit.com/docs/zh/)、添加使用示例。文档位于 `site/` 目录和 README 文件中。
- **🌐 本地化与翻译** — 帮助将文档站、README 或界面文案翻译成更多语言。查看 `site/docs/` 了解现有翻译。
- **🧩 Skills 与插件** — 创建新的 [skill](https://gitcode.com/atomgit_atomcode/atomcode-skills)（Markdown + JSON，无需 Rust），扩展 AtomCode 的能力。Skill 从 `~/.atomcode/skills/` 加载。
- **🐛 Bug 报告** — 发现 Bug？在 [Issues](https://atomgit.com/atomgit_atomcode/atomcode/issues) 中提交清晰的复现步骤、截图和环境信息。高质量的 Bug 报告非常宝贵。
- **🧪 测试用例与示例** — 添加测试场景、示例项目或使用演示，帮助验证功能并帮助新用户上手。
- **💬 社区支持** — 在社区群中回答问题、编写教程或制作视频指南。

每一份贡献，无论是代码还是非代码，都能让 AtomCode 变得更好。不确定从哪里开始？开一个 Issue 或发起讨论吧！

## 社区交流
---

用微信扫描下方二维码加入 AtomCode 用户群，反馈问题、分享使用心得，和其他用户、维护者一起交流：

<p align="center">
  <img src="https://cdn-news.gitcode.com/news/AtomCode_qun.png" alt="AtomCode 微信用户群二维码" width="220">
</p>


## 打赏
---

☕ AtomCode 免费用，Coding Plan 也不收费。如果它帮你省下了一点时间，欢迎请作者喝杯咖啡，让我们更有动力把它做下去。

<p align="center">
  <img src="https://cdn-news.gitcode.com/news/alipay_1782981974317.png" alt="AtomCode 支付宝赞赏码" width="220">
  <img src="https://cdn-news.gitcode.com/news/wechatpay_1782982603403.png" alt="AtomCode 微信赞赏码" width="240">
</p>


## 许可证
MIT License。详见 [LICENSE](LICENSE)。

---

<p align="center">
  用 Rust、ratatui 以及无数个深夜构建而成。
</p>
