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
  <img src="https://img.shields.io/badge/version-4.25.9-blue" alt="version">
  <img src="https://img.shields.io/badge/rust-1.88%2B-orange" alt="rust">
  <img src="https://img.shields.io/badge/license-MIT-green" alt="license">
  <img src="https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20HarmonyOS PC%20%7C%20Windows-lightgrey" alt="platform">
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
| 智谱（GLM） | 支持 | GLM-4、GLM-5、GLM-5.2（AtomCode Pro套餐专属模型） |
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
- **工具审批** —— 电脑执行到需要授权的工具时，手机弹出审批卡片，可直接点允许/拒绝
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