# Loop quota recovery

基线：release/v5.2.7 / cbdc851cd；已有 goal 提前核验与 UI 改动未提交，全部保留。

CodingRuntime / LoopState 继续持有唯一 loop 生命周期。只恢复已知账号、已知重置时间的额度暂停；复用现有 quota 查询与 provider 恢复预算实现，loop 沿用当前 goal.recovery 配置（默认 30 分钟，long_wait 显式延长），不扩大轮次或绕过 kernel 安全终止。

限流时保留 held turn、原 loop 轮次、cadence 及 pending wakeup。核验可用后发送同一轮的 synthetic continuation，轮次不增加；该轮完成后按原 cadence/model wakeup 进入下一轮。额度仍不足/未知则有界等待；账号改变、预算耗尽、持久化/策略失败则明确结束控制器并关闭 accepted operation。

同一 runtime 的刷新入口支持 goal 和 loop；新的用户消息会终止 loop 的额度等待并开启新的普通回合，取消、stop_loop、替换控制器、session/provider/shutdown 沿用父 cancellation token 和 generation/controller/epoch 保护。

LoopProgress 增加可选 recovery，host/TUI/WebUI 共享 quota 标记与倒计时。WebUI 单独维护 loop_changed 最新投影及重连样本；不把 controller 状态作为会话历史回放。

验证：虚拟时间覆盖自然恢复、提前刷新、轮次/cadence 保留、重复刷新、额度未知、取消/新输入/账号改变/预算/安全上限。检查 affected crate 测试、WebUI 类型/测试/build、compile 与 fmt 门。

## Completed behavior and validation

- goal / loop 共用 checked_quota 和 controller retry inbox，按 controller id、generation、epoch 分流；提前刷新不会授予新预算，也不会重放 loop 原始任务。
- loop 只继续被中断的当前轮；最后允许的轮次也能补完，但不会因此额外获得下一轮。固定 cadence 和模型的 pending wakeup 均保留；进入额度等待会使旧 cadence epoch 失效。
- 人的 Submit 结束旧额度等待并创建新回合。团队等自主消息开启 kernel turn 时，只使旧额度计时器失效，继续原 loop 当前轮，不误当成人取消。
- shutdown / owner exit 在公布退出前清除 loop 标记并取消计时任务。RuntimeLiveView 保留 loop 投影，并在读取时校正 goal / loop 的剩余等待时间，避免迟接入后重新显示整段旧倒计时；不改变执行状态或会话事实。
- TUI 显示 Loop 等待/核验；WebUI 增加可选 loop_progress snapshot 与 loop_changed 观察事件，复用独立计时组件。

验证：30 项核心 goal/loop/late-view 测试通过；受影响五个 crate 扩展测试 4263 通过、3 失败、16 跳过。失败仍是前阶段已记录的会话撤销和跨项目导航三项（RewindPointNotFound、SessionInUse、缺失目录未被拒绝），未混改；五项已知固定退避时长断言继续明确排除。新增 loop wire、TUI 两种模式文案、hub 重连测试均在扩展测试中通过。

WebUI 413 项测试、类型检查、production build 通过。bash gates/compile.sh、cargo fmt --all -- --check 和 diff 检查通过。真实 CodingPlan 重置/提前刷新仍需联调；本功能仅针对当前存活的 loop，不新增跨进程重启后自动恢复。

最终审查基线：48635d838（已合入循环/压缩变更）。当前基线针对 loop、goal recovery、live view、TUI tip 与 host wire 的 107 项回归全部通过。首次复跑编译门命中旧 host-api/i18n 编译产物；仅刷新源文件时间戳触发重新编译，未修改构建配置。完整回归的既有失败与排除项如上，真实账号额度联调仍未执行。
重新编译后的 bash gates/compile.sh 全部门通过（含 feature 编译和阴性对照）。
