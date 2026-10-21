# Windows 图片粘贴崩溃诊断

目标：定位 5.2.3 中输入文字、按两次 Ctrl+V、再按 Ctrl+Alt+V 后的
`0xc0000005`。本轮不改变读取顺序、不加互斥、不隔离子进程，避免先改变复现条件。
诊断日志同步落盘会改变时序；未复现不等于已修复。

## 1. 构建并保存配套产物

在 Windows 源码目录的 PowerShell 中运行。需要现有项目的 Rust/MSVC 构建环境。
先确认已取得包含本诊断改动的提交；有自己的未提交改动时先保留，不要强制重置。

```powershell
git rev-parse HEAD
cargo build -p atomcode --bin atomcode --profile clipboard-diagnostic --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw "构建失败，停止测试" }
$diagExe = (Resolve-Path '.\target\x86_64-pc-windows-msvc\clipboard-diagnostic\atomcode.exe').Path
$diagPdb = (Resolve-Path '.\target\x86_64-pc-windows-msvc\clipboard-diagnostic\atomcode.pdb').Path
$diagDir = Join-Path $env:TEMP ('atomcode-clipboard-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Path $diagDir | Out-Null
Copy-Item $diagExe,$diagPdb -Destination $diagDir
git rev-parse HEAD | Set-Content (Join-Path $diagDir 'commit.txt')
Get-FileHash (Join-Path $diagDir 'atomcode.exe') | Format-List | Out-File (Join-Path $diagDir 'exe-sha256.txt')
$diagExe = Join-Path $diagDir 'atomcode.exe'
$env:ATOMCODE_CLIPBOARD_DIAGNOSTICS = Join-Path $diagDir 'clipboard-diagnostic.log'
```

独立 profile 继承 release 的 opt-level、LTO 和 panic=abort，仅开启 debug=2、
strip=none。普通 release 发布配置不变。后续重编时勿覆盖这次保存的 exe/PDB。
此 PDB 只用于本次构建，不能用于解析旧版本的 `0xE61866`。

## 2. 采集崩溃转储

从[微软官方页面](https://learn.microsoft.com/en-us/sysinternals/downloads/procdump)
下载并解压 ProcDump。以下假设解压到了 `C:\Tools\Procdump`，请按实际目录修改。
保留上一步的 PowerShell 窗口和变量；切换到原先复现问题的工作目录后启动：

```powershell
# 例如：Set-Location 'C:\Users\CSDN\ppt-master'
& 'C:\Tools\Procdump\procdump64.exe' -ma -e -x $diagDir $diagExe
```

首次启动按 ProcDump 提示阅读并接受许可。`-e` 捕获未处理异常，`-ma` 保存完整转储，
`-x` 启动指定 exe 并监控。不要通过全局 PATH 启动旧的 atomcode。
不需要安装系统全局崩溃调试器，也不用改注册表。

在 TUI 中：复制原来会触发问题的图片 → 输入文字 → Ctrl+V 两次 → Ctrl+Alt+V。
记录准确时间和图片来源（截图工具/微信等）。出现异常后等待 ProcDump 显示 dump 写入完成。
ProcDump 自身的退出码不是 AtomCode 退出码，以它报告的异常和 dump 为准。

如果监控启动改变了终端行为，在第一个窗口直接运行 `& $diagExe`，第二个窗口使用
`Get-Process atomcode | Select-Object Id,Path` 确认诊断 exe 的 PID，然后运行：

```powershell
& 'C:\Tools\Procdump\procdump64.exe' -ma -e <实际PID> $diagDir
```

等第二个窗口确认已附加后，再在第一个窗口操作粘贴。若提示权限不足，确保 ProcDump
与目标进程具有匹配权限。只测试一个诊断进程，便于对应事件。

## 3. 保留证据与关闭诊断

需要同一次构建的 exe、PDB、commit.txt、exe-sha256.txt、clipboard-diagnostic.log、
生成的 .dmp，以及异常时间附近的常规 atomcode.log。保留整个 `$diagDir` 即可。
完整 dump 包含进程内存，可能含会话、凭据和剪贴板数据，请通过私有渠道提供，勿公开上传。

日志只记录时间、PID、线程、调用编号、阶段 begin/end，不记录文字、图片像素或凭据。
同一 PID/call 的 end 表示函数已返回（包括返回 None/Err），不代表成功；不同线程的日志
可交错，不能只凭文件最后一行断定根因，需结合 dump。若文件创建/写入失败，诊断静默失效。

```powershell
Remove-Item Env:ATOMCODE_CLIPBOARD_DIAGNOSTICS -ErrorAction SilentlyContinue
```

若同步日志开启后不再复现，可关闭此变量，仍用带 PDB 的同一 exe 在 ProcDump 下测试。

本地验证覆盖 macOS 编译/测试；Windows PDB 生成、终端交互及异常捕获需实机验证。
