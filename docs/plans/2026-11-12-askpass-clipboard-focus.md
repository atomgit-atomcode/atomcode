# 密码输入期间的鼠标交还与粘贴

基线：release/v5.2.7，f4c53a980。保留当前版本文档与 changelog 日期的未提交修改。检查了 keymap、TUI 输入分发、secret buffer、鼠标切换和剪贴板路径及相关近期历史。

问题：askpass 字段优先接收全部按键，导致 Ctrl+G 在等待 SSH/sudo 密码时被吞掉；中键和菜单粘贴则绕过密码焦点进入普通草稿。

修复仅在 TUI driver：密码焦点放行现有绑定解析出的 ToggleMouse 和 CopySelection；AttachImage（Ctrl+V）在此处只粘贴文本。其他按键仍由密码字段处理，包括 Ctrl+C/Esc 拒绝请求。中键、菜单和 Ctrl+V 共用焦点感知粘贴；密码不进入草稿、附件或会话流。剪贴板临时文本在交给密码缓冲后清零。SSH 上继续拒绝读取服务器剪贴板，终端收到的 Paste 仍进密码缓冲。

不更改 runtime、session 协议、审批或凭据持久化。验证鼠标交还不取消任务/密码请求、三种粘贴不污染草稿或记录、SSH 不读服务器剪贴板、Enter/拒绝语义及既有 askpass/paste 用例。

验证：TUI 全部 1970 项测试通过（本地 askpass socket 用例在允许 Unix socket 的环境中运行）。新增覆盖密码等待中的 Ctrl+G、掩码选区复制、Ctrl+V/中键/菜单粘贴和 SSH 剪贴板边界。未连接真实 SSH 主机。
