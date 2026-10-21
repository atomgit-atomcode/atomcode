# Windows 图片剪贴板重叠读取修复

基线：160def108。保留工作树其他人的 runtime/config/team/site 修改。
用户提供的诊断 exe 对应 PDB 与 dump 的 GUID/age 匹配；dump 在 memcpy
读取无效源地址时触发 0xc0000005，栈顶返回地址对应 clipboard_win::raw::get_vec
中的 memcpy 调用。日志显示提示检测与主动粘贴同时进入 arboard_get_image。
这些证据确认故障位置和读取重叠，不能单独证明关闭句柄导致内存失效的完整因果链。

本地 Codex 对照：tui/src/chatwidget/interaction.rs 有 250ms 图片粘贴抑制，
app/clipboard.rs 在复制 worker 忙时拒绝粘贴，且同步完成图片粘贴再处理下一按键。
clipboard_paste.rs 原生 Windows 同样用 arboard 3.6.1，没有用子进程隔离读取，
也未发现类似 AtomCode 的周期性整图提示探测。WSL 的 PowerShell fallback
不能当作原生 Windows 的隔离方案。

实施：

- Windows 图片原始读取、DIB fallback、文本读取、文本复制共享进程级互斥；
  guard 覆盖 native handle 打开、数据复制和关闭，不在 fallback 中重复加锁。
- Windows 提示仅查询 CF_DIB/CF_DIBV5/PNG 格式及剪贴板序号，不打开/读取/解码图片。
  查询前后序号不同或不可用时不生成标记，下一次探测重试；同序号不反复提示。
- 保留现有 1.5s 提示调度、实际粘贴格式支持和诊断日志。暂不增加子进程、
  第二状态机或仅针对按键的延迟：重复按键仍可粘贴，不能靠防抖掩盖原生访问违规。
- 此次不影响 runtime、会话、provider、图片发送格式和 macOS/Linux 读取策略。

验证：共享 guard 的跨线程排他单测、TUI crate 测试、格式检查、编译闸门。
macOS 验证不能覆盖 cfg(windows) 原生行为；用户需用保留 PDB 的诊断 profile
重新构建，在 ProcDump 下按原按键序列反复验证，并检查 image_mark 不再产生
arboard_get_image 读取。不将本地测试通过称为 Windows 崩溃已实机解决。

审计边界（21bea3b02）：CLI 仍有可进入 atomcode-tuix 的分支。用户明确要求
tuix 不改，准备删除，因此本次只修 atomcode-tui，tuix 原生剪贴板路径仍保留原状，
不宣称其同类风险已解决，也不将其称为已退役。

交付验证：`cargo fmt --all -- --check`、`git diff --check`、`bash gates/compile.sh`
均通过。完整 TUI 测试中 1497 个单测通过，1 个已有文案断言失败：
`content::tests::a_stop_reason_is_spoken_not_printed`。21bea3b02 中断言仍期待
“没挂停止策略”，但同一基线的 StopRunawayFuse 中文文案已是“已达到执行循环安全上限”。
未修改该无关测试/文案；显式跳过这一项后，其余 1687 个 TUI 测试全部通过
（1497 单测、178 e2e、7 + 5 集成测试）。Windows 实机复测仍待用户执行。
