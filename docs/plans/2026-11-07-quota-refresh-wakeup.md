# Quota refresh wakeup and presentation

基线：release/v5.2.7 / 2e7df1065。runtime.rs 与 runtime_criteria.rs 已有 staged 的统计超时修复，保留不覆盖。

继续沿用 CodingRuntime / GoalState 的唯一生命周期。新增明确的 refresh_quota_wait 控制：仅提前唤醒正在等待额度的 goal，不提交普通消息、不重置恢复预算，不恢复人工暂停或安全上限停止的任务。提前核验使旧 timer/查询 epoch 失效，并保留最初账号绑定。重复刷新在核验进行中合并为一次。

TUI SignIn 在有额度等待时只触发核验，明确说明运行中配置不重载；无额度等待时沿用原有配置 reload。WebUI CodingPlan setup 持久化成功后通知当前 live runtime。两者都以实时核验结果决定是否续跑，刷新成功不等于额度恢复。

GoalRecoveryProgress 与 Running 增加 serde-default 的 waiting_for_quota 布尔字段，旧数据可读，旧客户端可忽略。账号身份仍仅在 GoalState 内存中，不入协议/持久化。TUI 显示额度等待倒计时/正在核验；WebUI 按采样时间本地递减倒计时，到零仍显示核验中而非宣告恢复。

验证：虚拟时间覆盖提前唤醒、重复刷新、额度仍不足、账号切换、取消/人工暂停/预算边界；覆盖 host 投影、TUI 文案和 WebUI 计时。运行相关 crate 测试、WebUI 类型检查/测试和 compile/format 门。

## Implementation and validation

额度核验使用独立 runtime 控制、原账号绑定和同一恢复预算。核验进行中重复刷新合并；普通对话、人工暂停和非额度恢复均不启动新工作。TUI 通过既有 DoneWithNote 明确“当前任务使用原设置，配置稍后重载”，WebUI setup 成功后通知已绑定 runtime。未启用 loop 自动恢复。

状态字段在 false 时省略，旧消息仍能逐字段往返。WebUI ticker 位于独立子组件中，不每秒重绘消息列表；重连使用服务器采样时间校正，旧 GoalChanged 样本不进入回放覆盖新倒计时。

验证：

- 49 项相关 Rust 测试通过（额度提前核验、连续刷新合并、账号绑定、人工暂停、host 投影、TUI、重连计时及 wire 兼容）。
- WebUI 407 项测试、typecheck 和 production build 通过。
- `bash gates/compile.sh`、格式门、diff 检查通过。
- 扩大至受影响五个 crate 后，初轮 4250 通过、4 失败；其中新增字段的旧消息兼容断言已修复并复测通过。其余三项单独复测仍失败：`a_turn_stopped_before_any_answer_can_be_undone_by_its_number`（RewindPointNotFound）、`shared_navigation_prepares_the_target_project_before_resuming`（SessionInUse）、`shared_navigation_to_a_missing_project_keeps_the_old_foreground`（缺失目录未被拒绝）。本次不修改这些会话撤销/导航路径；工作区有其他进行中的会话修改，均保留。
- 先前记录的五项固定抖动等待时间断言在扩展测试中明确排除，未宣称全套通过。真实账号的提前额度恢复尚未联调。

Loop 的同轮续跑、刷新和展示已由 [loop 后续实现](2026-11-07-loop-quota-recovery.md) 补齐；统一使用 refresh_quota_wait 控制，原账号绑定和预算不变。
