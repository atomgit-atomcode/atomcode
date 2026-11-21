# Coding plan 刷新后的手动继续提示

基线：release/v5.2.7，4be315e07，工作区干净。

第一阶段仅改 TUI `/login`（含已合并的 codingplan 刷新入口）的展示。CodingRuntime 继续拥有任务、provider 和 controller 生命周期；不新增待恢复任务存储，不自动 submit，不修改 goal/loop 预算和持久化格式。

配置 reload 成功后，使用现有 HostCommand::Usage 查询新配置对应 provider 的实时额度。只有非空且全部未耗尽的窗口才认为当前额度可用；查询失败、未知或窗口耗尽均不提示。只对刷新前后仍在同一 lead 会话、日志序号未变且最新回合因 RateLimited 停止的情况显示手动继续说明。排除 MaxRounds 等安全停止。提示只说明当前额度可用，不断言历史 429 一定由额度耗尽引起。

继续入口沿用正常输入：用户发送“继续”。已结束的 loop 需要用户手动重新启动。刷新本身不恢复任务，不覆盖输入框。CLI 登录没有交互任务会话，因此不展示此提示；其他 driver 不新增自动恢复行为。

验证：额度窗口判定与最新回合终态测试、CLI crate 测试、workspace compile gate、格式门。后续若支持 goal 自动恢复，需独立设计同 provider/account 的恢复标记、取消失效和重试预算处理。

后续实现：goal 的自然重置核验与自动恢复见 [goal quota recovery](2026-11-07-goal-quota-recovery.md)。本提示本身仍不提交任务；中英文案已改为仅提示“若任务仍暂停，可手动继续”，避免否定 goal 的自动恢复。
