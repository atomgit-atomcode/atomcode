# PuTTY / SSH 原生复制粘贴兼容

基线：`release/v5.2.5`，`52d370e6c4106a9243a37b0d5ff44473bcee30fa`。保留工作区已有的后台构建等待改动。

## 依据与边界

- 5.2.2 和 5.2.3 都启用了应用鼠标接管；不能把所有现象归因于 5.2.3 某一个提交。5.2.3 的滚动条、异步复制和菜单修复并没有解决终端原生选择与应用鼠标互斥的问题。
- PuTTY 的鼠标报告模式会让普通选中/右键交给远端应用，而远端无法直接读取客户端 Windows 剪贴板。只隐藏菜单无法恢复宿主的原生操作。
  官方说明：[PuTTY 鼠标报告与原生复制粘贴](https://www.puttyssh.org/0.83/htmldoc/Chapter4.html#config-features-mouse)。
- 本地 Codex `806d9732c974bc8a51b8317c1bd8985544fe627c`：`app/right_click_paste.rs` 禁止 SSH 本地剪贴板读取；`clipboard_paste/text.rs` 同样拒绝 SSH；`tui/alternate_screen.rs` 按界面模式接管鼠标，并没有 PuTTY 专用默认策略。`bottom_pane/paste_burst.rs` 另有输入缓冲和 Enter 保护，不等于修复鼠标所有权。

## 修复

1. SSH（含无法识别客户端、只报告 xterm 的连接）及显式标识为 PuTTY 的终端，启动时默认把鼠标留给宿主。普通本地终端保持现状。
2. 这是启动偏好，不是“不支持鼠标”的能力判定；Ctrl+G 仍可主动取回应用鼠标。已有 `ATOMCODE_MOUSE=1` 可显式开启，`ui.mouse=false` / `--no-mouse` 仍禁止启动接管。
3. 不增加终端查询和输入协议，不读取远端服务器剪贴板模拟客户端粘贴。统一 SSH 检测供启动与剪贴板边界使用。
4. 沿用已有 bracketed paste、非 bracketed 输入聚合和 CRLF 归一化，不猜测性转码。用中文、Emoji、多行和末尾换行验证交还鼠标后的编辑框输入完整且不提交。
5. 交还终端时像 Codex/crossterm 一样完整关闭鼠标跟踪（含可能继承的 1000/1015），丢弃接管结束后迟到的鼠标报告，避免重新打开应用菜单或粘贴服务端剪贴板。

状态 owner 是 TUI terminal surface；不改 runtime、session、provider、持久化格式或命令协议。实际 PuTTY 客户端的终端配置/编码仍需实机验收。

## 验证

- 针对性 41 项测试通过；TUI 完整 nextest 共 1,823 项全部通过。
- `bash gates/compile.sh` 通过（workspace 全目标、feature 门和阴性对照）。
- `cargo fmt --all -- --check` 与 `git diff --check` 通过；Cargo.lock 未修改，三个禁止的 package 名均为 0。
- 未进行 Windows PuTTY 实机验收；不把事件级 Unicode/多行回归等同于对客户端字符集配置和任意网络分包的验证。
