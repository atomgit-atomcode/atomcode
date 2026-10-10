# crossterm 0.29.0：终端颜色响应补丁

上游：https://github.com/crossterm-rs/crossterm

基线提交：`36d95b26a26e64b0f8c12edfe11f410a6d56a812`（0.29.0）。
许可证保留在 `LICENSE`。根 workspace 的 `[patch.crates-io]` 固定使用此源码，
不依赖开发机 Cargo 缓存。没有改变公共 `Event` 枚举。

## 补丁范围

- `src/event.rs`：Unix `query_terminal_colors(Duration)`，在正常事件读取器的锁下查询
  OSC 10、11 和 OSC 4 的 0–15 号颜色；必须在 raw mode 下、启动 EventStream 前调用。
- `src/event/read.rs`：查询时保留真实输入顺序；没有颜色查询接收的回复直接消费，
  不作为键盘事件，也不积压在跳过事件队列。查询等待结束不清除解析器状态。
- `src/event/source/unix.rs`：两个 Unix 后端共用增量解析器，识别 BEL/ST 结束的
  `rgb:` 和 `#RRGGBB` 颜色回复。括号粘贴中的内容保持字面文本。
- `src/event/source/unix/{mio,tty}.rs`：解析器期限参与轮询；每次读取后重新等待
  readiness，避免 blocking tty 上孤立 ESC 导致第二次 read 永久阻塞。
  mio 后端重新注册 readiness，保留超过单次缓冲容量的输入。

TUI 不再直接读取 stdin。启动查询最多等 200ms，未回答部分沿用默认调色板；
迟到响应不会动态改变已选择的调色板。Windows 颜色获取路径不变。

## 兼容与恢复边界

传统 ESC 编码与 Alt 按键存在歧义：单独 ESC / Alt+] 使用 30ms 前缀等待。
数字开头的 OSC 控制字符串整体消费，只有有效的 OSC 10/11/4 生成内部颜色事件。
无效或未知 OSC 不转为按键。OSC 缓冲上限 256 字节，超长部分丢弃到终止符；
新转义序列可结束损坏的 OSC。没有新字节的 OSC 在 1s 后恢复正常解析，
因此跨越该恢复期限的异常慢后缀不保证仍被识别为响应。
不同颜色查询没有协议级请求 ID，不支持并发查询或精确区分极迟的上一轮回复。

## 验证

默认后端及 `use-dev-tty` 后端均应运行依赖自身的 lib 测试；
`libc` / 默认 rustix 描述符实现都应编译并测试。新增测试覆盖逐字节分包、
查询超时后完整/半截回复、真实 Unix 输入源、blocking 描述符上的 Esc、
超过 1024 字节的输入、普通键顺序、字面粘贴以及过期响应不积压。
这些 UnixStream 测试经过真实系统轮询和读取，不等于真实终端模拟器的 PTY 验证。
另运行 atomcode-tui 测试及 `bash gates/compile.sh` 验证依赖消费者。

后续应将补丁提交上游；仅当上游版本通过相同回归判据后移除此目录和根 patch。
