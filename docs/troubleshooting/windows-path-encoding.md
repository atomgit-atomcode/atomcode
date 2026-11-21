# Windows 中文路径编码排查

## 状态与范围

`_atomcode_archive` 不是 AtomCode 内置的归档目录约定。本次报告符合 UTF-8 字节被按 CP936 解码的特征，但缺少原始工具调用和执行记录，尚未定位事故根因。已有乱码字符串可以合法地通过宽字符 API 创建目录，不能把“用了 UTF-16 API”等同于“输入语义正确”。目录回读同样无法判断已损坏的输入原本想表达什么。

当前包含诊断、回归和脚本编码风险提示；安装/卸载脚本使用纯 ASCII。不切换全局 code page、不自动猜测修复路径、不改权限或 shell 执行语义。

## 获取 AtomCode 启动诊断

在 cmd 中启动（无需管理员权限）：

```bat
set "RUST_LOG=info,atomcode::shell::windows=debug"
atomcode
```

复现后查看产品数据目录下的 `logs/atomcode.log`（默认在用户配置目录下；自定义数据目录则使用对应目录）。事件记录实际 shell、cwd、参数数量/是否含非 ASCII、EncodedCommand 标志和父进程的 ACP/OEM/控制台 code page。没有附着控制台时，控制台 code page 可以是 0；这些值不代表每个外部程序的编码。

新增日志不包含完整命令、环境变量值或子进程输出。实际工具命令仍从会话事件中的工具调用核对；把两者对照，确定路径首次损坏发生在命令输入还是输出解码之后。 cwd 会包含本机路径，分享日志前可以按需匿名化路径。

## 独立环境探针

在仓库根目录的 cmd 中执行：

```bat
powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts\probe-windows-path-encoding.ps1
```

脚本源文件只含 ASCII，避免探针本身先被 PowerShell 5.1 按错误编码读取。它只在系统临时目录创建一个 `atomcode-encoding-<随机值>`，比较：

- 原生 .NET 文件系统调用；
- cmd 直接命令参数；
- UTF-8 无 BOM 的批处理文件；
- PowerShell 5.1 的 UTF-16LE EncodedCommand；
- PowerShell 5.1 无 BOM / 带 BOM 的 UTF-8 脚本文件。

所有测试命令的写入目标均是各测试目录内的相对路径；即使生成的脚本被故意按错误编码读取，也不会在真实工作区或临时目录之外新建乱码树。探针不运行 AtomCode，不修改系统语言、控制台 code page 或持久执行策略。`ExecutionPolicy Bypass` 只对该探针进程及其测试子进程有效。

报告保存为 UTF-8/BOM 的 `report.json`，包含准确的目录名、正确目标文件是否存在、退出码和原始输出的 Base64。优先看报告文件，不只依赖控制台显示。脚本保留临时样本供检查，确认后可手动删除报告中 `root` 指定的整个临时目录。

无 BOM 脚本或批处理出现乱码只是环境负向对照，不证明历史事故经过这条路径。若直接命令正常、文件脚本异常，应继续查原始任务是否生成过 `.cmd/.bat/.ps1`；若 AtomCode 发出的命令已经带乱码，则继续查它前一次使用的目录列表、读取结果或外部程序输出。

## 回归执行与限制

在 Windows 使用：

```text
cargo nextest run -p atomcode-capabilities -E "test(windows_encoding_tests) | test(windows_powershell_versions) | test(chinese_workspace_paths) | test(encoded_command_preserves) | test(windows_diagnostics)"
```

Windows 专属用例覆盖原生创建、强制 cmd（即使安装 Git Bash）、实际选中 shell 的相对路径、PowerShell 5.1/可用的 7、已失效 cwd，以及根目录枚举以排除多余乱码同级目录。临时路径含中文和空格。跨平台测试覆盖输出解码与编码载荷，但不能替代 Windows 实机验证。

`.github/workflows/check.yml` 的 Windows Unicode path roundtrip job 在 Windows runner 上执行上述专属测试与环境探针。此入口是为了获得真实 Windows 证据；本地 macOS 通过的测试不代表该 job 已运行。

## 预防脚本文件串码

内置 PowerShell 工具直接以 UTF-16LE `-EncodedCommand` 传递命令，但从工具里再次运行
`powershell.exe -File script.ps1` 或批处理文件，会经过另一套脚本解码规则。

- 优先通过 `pwsh` 工具直接执行含中文路径的命令，无需生成临时脚本。
- 必须由 Windows PowerShell 5.1 执行的非 ASCII `.ps1`，明确保存为带 BOM 的 UTF-8；
  或在确认已安装时使用 PowerShell 7。
- 不依赖旧代码页下的 cmd 执行含中文的 UTF-8 `.cmd/.bat`。给批处理添加 BOM 不是通用修复。
- 修改 `OutputEncoding` 只涉及输出；`chcp 65001` 也不能修复 PowerShell 5.1 的脚本文件解码。

`write_file` 写入上述有风险的脚本时会向模型返回警告，文件内容保持调用方提供的原文。
Windows shell 工具描述同样包含执行指导。这是预防措施，不是执行拦截；已有脚本、
通过其他工具生成的文件及模型忽略警告的情况仍需核对。原始事故尚不能据此判定根因已闭合。

## 产品自建脚本入口排查（2026-11-10）

本次以 `a97c85b74` 为代码基线，核对 Windows 命令启动及 `.ps1/.cmd/.bat`
生成与执行位置，未发现生产路径自行写出无 BOM Windows 临时脚本后再执行的入口：

| 入口 | 当前行为 |
| --- | --- |
| `tools/powershell.rs` | UTF-16LE/Base64 `-EncodedCommand`，不落临时脚本 |
| `tools/bash.rs` / `process_utils.rs` | Git Bash 直接 `-c`，或 cmd 宽字符命令行 `raw_arg` |
| `notify.rs` Windows 通知 | PowerShell 直接 `-Command` |
| `cc_hooks.rs` / `mcp/transport_stdio.rs` | 调用配置的命令或外部脚本，不生成脚本正文；外部脚本仍由调用方负责编码 |
| daemon `workbench.rs` | 启动交互 PowerShell，设置 cwd，不生成脚本 |
| 安装/卸载与探针 | 安装/卸载脚本改为纯 ASCII，避免 5.1 `-File` 解码歧义；探针故意生成错误编码样本，不是产品执行路径 |

这不能证明所有外部程序或模型生成的脚本都安全。当前不需要新增产品临时脚本编码器；
将来若增加 PowerShell 5.1 的临时 `-File` 路径，应在该生成入口明确写 UTF-8 BOM，
并用 Windows 实机回归验证中文路径。不要把该策略应用到所有用户文件或 cmd 批处理。
