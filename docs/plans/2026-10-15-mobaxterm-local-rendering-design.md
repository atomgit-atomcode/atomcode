# MobaXterm 本地 Windows 运行的显示兼容

基线：`release/v5.2.4`，`5de4088abb9699ddddf8a2bc01b5b6943e9e4c89`，开始时工作区干净。仅处理 CLI 启动和 TUI 能力/绘制，不改 runtime、会话、鼠标映射、版本号或发布流程。期间其他任务的改动保留。

用户实测：MobaXterm shell 自己输出中文正常，直接启动 Windows AtomCode 中文与边框乱码；Windows Terminal 正常。在 MobaXterm 经 `cmd.exe /c "chcp 65001 >nul & atomcode"` 启动后显示、上下键、滚轮、框选复制均正常，只有滚动条外观与吉祥物颜色异常。用户随后截图确认：两条命令均指向同一个 `atomcode.exe`；`TERM=xterm`，`TERM_PROGRAM`、`COLORTERM`、`MSYSTEM` 为空，`CYGWIN` 含 `disable_pcon`，活动代码页 936。

## 改动

- Windows console UTF-8 初始化从 `async_main`（日志之后）前移到 OS `main` 的常规启动入口（clipboard/补全快速返回之后），使常规启动的标准 I/O、Tokio 和日志均在设置之后运行；失败提示仍在常规启动中报告，helper/补全不改宿主 shell 的代码页。统一在 surface 的 `ensure_console_utf8` 执行；TUI 进入、写屏和 `/raw` 进出再次检查，若输出页被改变且修正成功，则清掉已绘制缓存并重绘。现有 UTF-8 manifest 已存在，不重复添加。此项是启动顺序修正，不能在没有 Windows 实测的情况下宣称已根治本地桥接编码。
- Windows 已启用 VT 的终端，`TERM=xterm` 与没有 TERM 一样按 256 色处理。旧代码反而因 plain xterm 降到 16 色，导致像素图的橙色 index 202 被近似成黄色。保留 NO_COLOR、dumb 和显式 truecolor 的优先级；Unix xterm 不升级。
- 滚动条使用一列空格的背景色绘制轨道/滑块，不再依赖 `┃`/`│` 的终端实际宽度；无色模式用 ASCII `#`/`|` 区分。滚动位置、拖动与鼠标接管逻辑不变。

## 验证

覆盖 Windows xterm 与 Unix xterm 的颜色能力差异、彩色滚动条每行只有一个空格并含两种背景、无色轨道与滑块区分；运行 TUI/CLI crate 测试、编译门、格式检查并尝试 Windows 目标编译。最终仍需在用户的 MobaXterm 本地窗口复测：直接启动中文、吉祥物颜色、连续滚动时轨道外观，以及正常复制/滚轮不回退。

## 外部依据与实测边界

Cygwin 的 `disable_pcon` 路径存在根据 Windows console code page 转换原生程序字节的历史实现，不能把 POSIX shell 能显示 UTF-8 当作原生 Windows 子进程能显示的证明。参考：<https://cygwin.com/pipermail/cygwin-patches/2020q3/010627.html>。当前修复不修改 CYGWIN 环境、不强行启用 ConPTY、不添加 CMD 重启层；仍以新构建在该机器的实测为最终依据。

初轮 TUI 1476 项、CLI 568 项测试通过。完整 Windows 交叉编译因本机缺 Windows C 头文件（ring 编译找不到 assert.h）受阻；对修改后的 Win32 console 模块独立执行 MSVC 目标元数据类型检查通过，不能替代 Windows 运行验证。

最终验证：TUI 全套 1666 项、CLI 全套 568 项均通过；`bash gates/compile.sh`、`cargo fmt --all -- --check` 与 `git diff --check` 通过。修改后的 Win32 console 模块单独以 `x86_64-pc-windows-msvc` 做元数据编译通过。未生成 Windows exe、未在 MobaXterm 实机运行新构建；乱码与颜色修复仍需该环境验收，不能以原版本的 CMD 绕过测试代替。没有修改鼠标事件映射或接管默认值。

## 提交前审计

审计将轨道背景由 Border 改为 PanelBg，避免用户把 cyan/bright-cyan 配成同一颜色时轨道与滑块融在一起。新增判据比较实际渲染 RGB，覆盖深浅主题及 16/256/truecolor，而不只比较颜色索引。保留 clipboard 与补全命令的纯快速返回，不在它们执行时修改共用控制台编码。

## 3eaafc911 构建的实机反馈与 Codex 对照

用户使用 `3eaafc911+dirty` 测试：吉祥物变为橙色，但直接启动中文与边框仍乱码。`--probe-terminal` 没有出现代码页失败警告，报告 `Ansi256`、`unicode: false`，报告中非 ASCII 分隔符仍乱码。因此不能把调用 SetConsoleCP/SetConsoleOutputCP 成功视为解决了外层 Cygwin 转码问题；编码修复仍未完成。

本地 Codex `806d9732c9` 对照：`tui/src/tui.rs::ensure_virtual_terminal_processing` 检查 stdout/stderr 的真实标准句柄，只对控制台句柄设置 processed-output/VT；`cli/src/doctor.rs::windows_console_details` 同时报告代码页和标准句柄模式。未找到 MobaXterm/disable_pcon 专用修复或 CMD 自动重启逻辑。`d52478c52e` 处理的是 batch alias 的 Unicode 可执行路径，不是正文输出。

先将同类诊断补入 AtomCode `--probe-terminal`：实际 input/output code page、stdin/stdout/stderr 的文件类型和 console mode、Windows API 获得的终端尺寸以及 disable_pcon 状态。不添加猜测性的编码转码或 CMD 启动层。下一步需对照同机 Codex 表现，以及直接启动/CMD 启动的句柄诊断，确认实际发生转码的边界。

用户继续实测 Codex CLI 0.162.1：直接启动返回 `os error 6`（Win32 ERROR_INVALID_HANDLE），错误消息也乱码，无法进入 TUI。这不是 Codex 已支持此场景的证据。诊断中进一步报告 GetConsoleMode 失败时的 Win32 错误码，供直接启动/CMD 启动对照；不把诊断增强当成乱码已修复。
