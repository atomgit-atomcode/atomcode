# 项目能力检查与可选工具配置

TUI 中运行 `/setup` 查看当前项目的能力清单。检查复用项目技术栈扫描，列出 ast-grep、代码项目可选的 Context7，以及 Rust、Python、JavaScript/TypeScript、Java、Go 的原生 LSP 配置情况。它不会启动语言服务器、连接 MCP 或调用模型。

首次在识别到的代码项目显示欢迎区时，会提示可用 `/setup` 检查能力。提示按项目、推荐项及推荐版本记录；依赖清单的小改动不会重复提示。新增适用推荐可再次提示。`/setup skip` 记录当前推荐已知晓；`/setup` 或 `/setup --force` 随时重新检查。这个记录独立于种子安装状态。

## TUI

- `/setup` / `/setup check`：打开能力清单，选择条目查看详情或安装计划。
- `/setup install ast-grep`：查看包管理器、命令和作用范围。读完计划后按 Enter 确认执行，Esc 取消。
- `/setup cancel`：取消正在执行的安装操作；已有的安装副作用不会回滚。
- `/setup install context7`：查看将写入的项目配置位置及默认禁用说明，再确认。
- `/setup automations [focus]`：保留原有的模型自动化推荐流程；首次使用会安装内置 setup skill 并重载。
- `/setup hooks`、`/setup mcp`、`/setup skills`、`/setup all` 仍转发到原有 skill。

能力检查和安装跟随当前会话的工作目录。切换目录或修改配置后，旧计划不能用于确认新的写入。

## CLI

```sh
atomcode setup --check
atomcode setup --install ast-grep
atomcode setup --install ast-grep --yes
atomcode setup --install context7
atomcode setup --install context7 --yes
atomcode setup --skip
```

不带 `--yes` 的 `--install` 只展示计划，不执行。CLI 裸 `atomcode setup` 保留种子 skill 安装，并补充能力报告。`--force` 保留强制重装种子的既有语义；与 `--check` 一起使用时仅重新检查。可用全局 `--config` 指定要检查的设置文件。

## 状态与边界

- ast-grep 校验 `--version` 的程序身份，避免把系统的同名 `sg` 当成 AST 工具。macOS 优先使用 Homebrew，否则使用 Cargo；其他平台首版支持 Cargo。缺少受支持安装器时给出官方手动安装说明。安装使用包管理器的常规位置、不添加 sudo，并在项目之外的临时目录执行，避免加载项目本地包管理配置。十分钟超时会报告失败，取消或超时会清理安装进程组（Windows 使用现有 Job Object 支持）；成功后再次检查当前 PATH 上的二进制。CLI 安装可用 Ctrl-C 取消。
- LSP 检查复用内置服务端表和用户覆盖；“找到二进制”不代表连接已验证。`lsp.enabled`、`lsp.auto_detect` 或显式服务端配置仍由用户管理，后续可用 `atomcode lsp probe` 验证。IDE 绑定独立于本地 LSP，此处不探测。
- Context7 按服务端身份识别已有配置，包括用户级别、其他名称和禁用条目。已配置不会重复添加；同名的其他服务不会被替换。它提供第三方库文档检索，不替代本地语义导航。
- 新增 Context7 只写项目 `.mcp.json`，不添加 key、工具自动审批权限或项目信任，并始终设置 `disabled: true`。即使项目已被信任，也不会因此自动连接。用户随后在 `/mcp` 中重载、显式启用并处理信任。启用后使用远程文档服务会将查询发送到 Context7。
- 配置损坏、不可读、存在注释或名称冲突时拒绝写入，保留原文件。既有服务器、凭据和顶层设置不被替换。安装失败与配置失败显式报告，不会作为能力可用返回。

首版不提供自动安装的语言 MCP 目录，不修改 `/init`，不增加 kernel 命令或第二套连接生命周期。
