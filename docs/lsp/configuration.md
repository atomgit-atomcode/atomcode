# LSP 初始化配置与自调试

本地 LSP 仍默认关闭，服务器必须由用户安装。启用后按需启动，不自动下载安装。

## 最小配置

将复杂初始化信息放在用户数据目录的 `LSP_config` 中，TOML 只引用文件：

```toml
[lsp]
enabled = true

[lsp.servers.cj]
command = "C:/CangJie_HOME/cangjie/tools/bin/LSPServer.exe"
root_markers = ["cjpm.toml"]
init_options_file = "LSP_config/cangjie.json"
```

相对 JSON 文件路径以用户配置目录为基准（默认 `~/.atomcode`，宿主自定义目录时随之变化），不随会话工作目录变化。也可以使用绝对路径或 `~/...`。目录和文件由用户按需创建，不安装包含无效项目路径的活动配置。

下面是反馈中已验证字段的**单包仓颉示例**，name 请替换成实际包名：

```json
{
  "multiModuleOption": {
    "${workspaceUri}": {
      "name": "jian_lang_server",
      "src_path": "${workspaceUri}/src"
    }
  },
  "conditionCompileOption": {},
  "singleConditionCompileOption": {},
  "conditionCompilePaths": [],
  "targetLib": "${workspaceFolder}/.cache/lsp"
}
```

当前通用实现不自动解析 cjpm.toml 生成项目模型。多模块、src-dir、条件编译和 SDK 差异仍需通过对应 JSON 配置，并在真实仓颉环境验证。

也支持 TOML 内联 `init_options = { ... }`，不能与 init_options_file 同时使用。初始化值按 JSON 整体发送，没有隐式合并。

## 变量

JSON 的对象键和字符串值均支持以下变量，数值和布尔值保持类型：

| 变量 | 含义 |
| --- | --- |
| `${workspaceFolder}` | 按 root_markers 确定的服务器项目根目录，本机路径 |
| `${workspaceUri}` | 项目根目录的 file URI，处理空格、中文和 Windows 盘符编码 |
| `${fileDir}` / `${fileDirUri}` | 当前查询文件的父目录，分别为路径 / URI |
| `${HOME}` | 用户 home 目录 |
| `${env:VAR}` | 已定义的环境变量 VAR；未定义时报错 |

不能把本机路径直接当作 file URI。未知变量、替换后的重复对象键、坏 JSON 和不可读文件均报错，不静默发送空对象。每个 JSON 文件最大 1 MiB。配置文件路径本身支持 `~`，不展开上述 JSON 模板。

## 工作区设置

初始化参数与工作区设置是两个独立入口：

```toml
[lsp.servers.py]
command = "pylsp"
root_markers = ["pyproject.toml"]
settings_file = "LSP_config/python-settings.json"
```

也可内联 settings 对象，与 settings_file 互斥。settings 必须是对象；在 initialized 后通过 workspace/didChangeConfiguration 发送，并用于回答服务端 workspace/configuration。支持精确 section 键与嵌套对象查找。各语言服务的设置结构需要依其自身契约填写，不能假设 VS Code 插件的全部配置都属于 initializationOptions。

## 开关

```toml
[lsp]
enabled = true
trace = "off"

[lsp.features]
definition = "auto"
hover = "auto"
references = "auto"
diagnostics = "auto"
document_symbols = "auto"

[lsp.servers.cj]
command = "C:/CangJie_HOME/cangjie/tools/bin/LSPServer.exe"
root_markers = ["cjpm.toml"]
disabled = false
init_options_mode = "auto"
init_options_file = "LSP_config/cangjie.json"

[lsp.servers.cj.features]
hover = "on"
```

这些字段都可省略。服务级 features 覆盖全局默认。

- init_options_mode：off 不读、不发送初始化参数；auto（默认）有配置来源才发送，文件缺失则跳过并在 probe 中说明；on 要求能够得到初始化参数，否则报错。文件存在但无效时，auto 也报错。
- disabled=true：停用该语言服务并保留声明。
- 能力 off：明确返回 configured_disabled，不调用该能力。auto：本地定义/悬停/引用按服务端静态或动态能力声明判断，不支持返回 unsupported；on：即使服务未声明也尝试调用，由实际响应决定结果。
- diagnostics 支持服务端推送，不要求 diagnosticProvider；关闭时不拉取、不存储或展示该服务的诊断。
- document_symbols 当前仍需要 IDE backend。本地能力开关不会使未实现的操作变得可用。
- IDE 与本地查询都遵守配置禁用。inspect 若包含被禁用的子能力，整体拒绝并指出该能力；可改为单独调用允许的查询。
- `[lsp] enabled` 继续控制本地进程；IDE 绑定保持原有独立启用语义。

本次不增加初始化脚本、任意 RPC、重命名或格式化。现有只读工具仍拒绝服务端 workspace/applyEdit。

## 调试

只初始化并检查能力：

```sh
atomcode lsp probe --file src/main.cj
```

检查具体位置，并记录实际协议消息：

```sh
atomcode lsp probe --file src/main.cj --line 13 --col 26 --method definition --trace verbose
```

method 只允许 definition、references、hover。line/col 从 1 开始，col 按 Unicode 字符计算，客户端转换为服务端协商的编码。输出连接根目录、初始化来源、服务端能力、有效策略、耗时、日志位置及查询结果；失败返回非零退出码。此入口复用模型的正式 LSP 工具。

一次性二分排查：

```sh
atomcode lsp probe --file src/main.cj --no-lsp-init-options
```

或者在启动 AtomCode 前设置 `ATOMCODE_LSP_INIT_OPTIONS=off`。环境覆盖由宿主读取，对该次运行有效，不修改文件。

模型的 lsp 工具提供：

- `operation: "status", backend: "local"`：查看连接、配置扩展名和失败数量，不启动服务器。
- `operation: "probe", file_path: "src/main.cj", backend: "local"`：检查正式初始化和能力，不执行任意 RPC。
- `operation: "reload", backend: "local"`：关闭本地连接、清理启动失败缓存。初始化/settings 文件会在后续调用中重读。

文件内容改变会参与新连接身份，不会复用采用旧参数的连接；旧连接可用 reload 统一释放。command、策略、文件引用等 TOML 声明在 runtime 装配时读取，修改它们后需重启会话/runtime。reload 不是重读整个 config.toml。不会自动反复重启失败服务器或隐藏失败。

trace 默认为 off；messages 记录初始化参数、能力及消息元数据，省略普通查询正文；verbose 记录更多请求响应和服务 stderr。日志写入用户数据目录的 logs/lsp.log，常见凭据键、其对应字符串值和本次 JSON 环境替换值会脱敏（包括服务端错误文本中的回显），verbose 仍可能含源代码和其他业务信息。单条日志最大 256 KiB，日志约 8 MiB 后停止追加；排查下一次前可归档日志并 reload。服务器 stderr 在握手期间保留最近 8 KiB，持续消费以避免堵塞进程。

当前仓颉 Windows SDK 尚需人工验收；模拟协议测试不能替代真实跨包查询验证。
