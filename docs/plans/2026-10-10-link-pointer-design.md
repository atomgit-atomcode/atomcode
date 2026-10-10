# 链接悬停鼠标形状

参考本地 Codex `codex-rs/tui/src/tui/link_pointer.rs`：直接连接的 Ghostty 与 Kitty 支持 OSC 22 的 pointer/default 形状；Kitty 用空形状释放覆盖，Ghostty 恢复 text。复用 TUI `Frame::link_at`，只提示当前已绘制的链接，不扩展为所有可点击控件。

Surface 负责环境检测、保存最后绘制帧和鼠标位置、请求自由移动事件及输出形状。事件循环只把输入交给 Surface；悬停本身无需重绘。重绘重新命中已有位置，滚动和覆盖层不会留下错误手形。按钮按下与拖选清除位置；失焦、窗口尺寸变化、交出鼠标、/raw、挂起、退出及 panic 都恢复形状。信号退出使用静态字节与原子状态清理，保持 async-signal-safe。

未识别终端及 tmux/screen/Zellij 不输出 OSC 22，保持现有鼠标行为。验证覆盖终端白名单、手形与默认形状切换、重复悬停去重、重绘更新命中、焦点和尺寸失效、拖选、写入失败后清理与终端特有恢复字节。
