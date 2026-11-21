# Live 消息在快速切换会话时的归属

## 问题与边界

基线为 `release/v5.2.8` 的 `d4fb24f48`。报告使用 CLI v5.0.3；该版本的
`/health` 不声明 `session_scoped_live`。App 的旧版兼容分支既不发送
`session_id`，也不等待消息接收确认就执行 `/cd`。

桌面端 `/live/message` 在准备输入前取得绑定，却在提交时再次取得当前 runtime。
消息准备、会话切换、提交的交错可能把 A 的输入交给 B。

## 选择的修复

- `CodingRuntime` 仍是执行和会话的唯一 owner。daemon 捕获 `LiveBinding`，
  将它传过输入准备阶段；hub 验证绑定身份，runtime 在消费提交命令时验证捕获的
  generation。绑定过期、generation 变化或投影失败均明确拒绝，不转投当前会话。
- 本次请求自己成功完成的 provider reload 可以更新捕获的 generation；不能通过
  重新获取当前绑定来更新目标。持久化格式和 kernel 协议不变。
- App 始终携带发送时的会话 ID，排队输入也保存该 ID；新旧协议均等待 HTTP
  接收确认再切换，连续切换按顺序执行。只等待接收确认，不等待整个模型回合。
- App 需要收到目标视图快照后才能继续发送；HTTP 200 中的 `accepted:false`
  必须作为失败处理。旧会话的迟到事件不能写进新视图。
- SSE 重连按连接代次隔离；旧连接的响应和关闭操作不能覆盖新连接。`/cd` 已经
  完成切换时，重连只读取当前视图，不再携带会造成二次切换的会话参数。

## 验证

- runtime 回归：捕获 A 的 generation、切到 B，再提交旧输入；断言拒绝，B 的
  快照不变，随后 B 的合法输入正常执行。
- hub 回归：输入准备期间绑定改为 B；断言 A 的输入没有到达 agent，也没有在 B
  回显；投影失败仍拒绝提交。
- App 回归：分别使用新旧能力标记，延迟 A 的接收确认并连续切 B/C；检查切换顺序、
  排队输入归属、失败回执，以及 A 的 SSE 响应晚于 B 到达的情况。

验证结果：基线加本补丁的隔离检出中，coding/daemon 共 1,811 项通过、11 项跳过；
最后补充投影失败检查后，daemon 的 436 项再次全部通过。`gates/compile.sh` 和
Rust 格式检查通过。App 的 34 项测试通过；改动文件的静态检查无 error，保留
`chat_page.dart` 已有的 3 项未使用成员/变量 warning 和 SDK/lint info。

自动化测试不能代替 Windows 多 TUI、实际手机与官方中继的端到端验收。
完整防护需要同时更新桌面端和 App；App 的兼容处理无法修复旧 daemon 对其他客户端
并发切换缺少最终校验的问题。
