# 一次性新会话 follow-up

基线：release/v5.2.6 @ ca887e19a；开始修改时 worktree 干净。

状态 owner：原任务与待触发登记仍由 CodingRuntime 持有。原任务正常终止时消费登记，通过现有 atomcode-schedule Service.launch 交给独立 CLI worker；worker 的 CodingRuntime 是新 agent 的唯一 owner。原 live binding、generation、流和审批保持原 owner，不在后台擅自 fresh/rebind。

复用近期 automation 的持久 admission、请求 claim、租约、session link 和执行历史；增加事件触发 Manual 类型及可选 follow_up 执行配置，旧任务不序列化空字段，现有定时器不接受 Manual。新配置放在独立 followups 目录，与定时任务隔离。CLI/daemon 将独立的本地 dispatcher 注入 CodingAgentConfig.follow_up_service，复用现有 AutomationService 端口，且不暴露给模型工具。coding 生产代码不依赖 schedule，也不选择宿主可执行文件；schedule 不依赖 coding。

入口：/followup new <JSON> 与 CLI --follow-up-new-file；显式模型选择、已有绝对工作目录、plan/accept_edits、仅用户指定的 context。默认无上下文复制；使用现有 Submit.extra_context 通道。未知字段、过长内容、auto 模式拒绝。真实 worker 启动前验证模型选择，不使用默认模型静默替代。

失败语义：取消、异常终态、待审批/后台工作不触发；登记在派发前消费。持久回执在 spawn 前写 uncertain，使用同一执行 ID 去重，worker 要求原 admission ID/版本且只领取一次。无法确认启动时明确提示检查历史，不重试。headless 原操作只等待启动结果，不冒充新任务成功；已启动 worker 独立运行。

验证：配置/OS 适配器单测覆盖 Manual 的隔离与无人值守限制；coding 单测覆盖显式上下文、输入边界、持久 claim 去重及 session link；保留 phase-1 runtime 生命周期用例；CLI/TUI/WebUI 编译和相关测试，最后执行全量 compile gate 与格式门。另以真实 CLI/worker 加本地模拟 provider 验证独立会话、显式上下文隔离与重复启动拒绝；真实远端 provider 与 Windows 环境留待人工验收。
