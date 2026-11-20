# 本地 WebUI 服务失联与后台 poll 可观测性

报告版本：Windows 11，5.2.7/dd97065f。服务端最后记录为 tool_started/bash_poll；
后续未记录工具结果，观察时未发现 AtomCode 进程。仅凭这些记录不能证明 poll 死锁、
运行时饥饿、OOM 或某一种退出原因。无转储/无 panic 日志也不能排除进程被外部终止。

当前 poll 有定时等待，且不跨 await 持有 job store 锁；定时等待依赖运行时继续调度，
不是进程失去调度或退出时的硬实时保证。加入 INFO 开始/结束记录，使用 pid + wait_id
配对，保留安全 job_id、请求/有效等待时长、outcome 和耗时；不记录命令、路径或输出。
正常返回、超时、取消和 future 丢弃会有收尾；进程被强制终止时不能承诺收尾记录。

WebUI 共享 fetch 层区分浏览器到本地服务/中继的网络失败。HTTP 响应与用户取消保持原义；
不把 URL、令牌或原始网络错误复制到提示。网络失败并不证明服务进程已退出，也可能是
地址、网络或浏览器访问限制。聊天提示明确 WebUI/AtomCode 通信边界，避免误导更换 provider。

不改 ChatRecoveryState，不允许失联后直接发送或自动排队；恢复连接后的权威活动检查、
停止成功或回合终态仍是解锁依据。浏览器刷新后可重新探测。既有 native session 冷恢复
已追加 SessionInterrupted/缺失工具结果未知/TurnEnd，不依据旧 snapshot.inflight 新增状态机。

验证：并发 poll 虚拟时钟验证 60 秒 clamp、开始结束配对、取消与丢弃及日志隐私；
WebUI 网络失败、HTTP 错误、取消、凭据隔离和既有恢复策略测试。未复现真实 Windows
多视频任务的进程退出原因，仍需故障现场的进程退出/转储证据。

验证结果：capabilities nextest 1,370 通过、1 跳过；WebUI 455 项测试、typecheck 与 build 通过；完整编译门、格式及 diff 检查通过。
