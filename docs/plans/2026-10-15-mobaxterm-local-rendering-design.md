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
