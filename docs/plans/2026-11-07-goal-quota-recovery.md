# Goal quota recovery implementation plan

**Goal:** goal 因已知额度窗口耗尽停止后，等待重置时间、核验额度并自动继续，不依赖 /login。

**Architecture:** CodingRuntime / GoalState 继续持有唯一生命周期。复用现有 goal retry timer、held turn、generation/controller/epoch 检查和 provider 恢复预算；后台异步额度查询只返回数据，不驱动 agent。kernel 的 RateLimited 事件已携带 reset 秒数，不新增终态 hook 或持久化格式。

**Baseline:** release/v5.2.7，4be315e07；工作区已有额度提示和其他任务的未提交修改，全部保留。

1. 消费带恢复时间和窗口描述的 RateLimited 暂停事件，仅为当前有额度源的 goal 开启核验型恢复。普通对话、loop、通用未知 429 不因此改变。
2. 到点在可取消的 timer task 中查询当前额度源（3 秒超时），结果带 generation/controller/epoch 回 runtime。仅非空、明确未耗尽的窗口允许 synthetic continuation；仍耗尽按重置时间再等，未知/失败走现有有界退避。
3. 保持已有 goal 时间、轮次和恢复次数/时间预算。默认 provider 恢复窗口为 30 分钟，超过此预算仍暂停；更长等待沿用显式配置的 long_wait，不静默延长。用户 submit、cancel、pause、session/provider replacement 与 shutdown 继续使旧计时/查询失效。
4. 虚拟时间测试覆盖恢复时间、再次耗尽、查询失败/空结果、取消/用户提交、预算与安全停止。运行 coding crate 测试、workspace 编译门和格式门。

本阶段补自然重置路径。刷新配置时仍沿用原有 Busy / replacement 规则，不绕过运行中的生命周期边界；提前核验入口及展示现已由 [后续实现](2026-11-07-quota-refresh-wakeup.md) 补齐；使用独立的 runtime 核验命令，不绕过配置重建的 Busy 规则。

账号绑定：额度源新增默认未知的非敏感 account_id 查询；只有可识别账号允许自动核验。等待前后、查询完成以及 runtime 消费结果时检查身份；切换账号或退出登录后显式暂停并保留 goal。账号标识不进入配置树、协议、日志或会话持久化。CodingPlan 宿主读取 auth.user.id，token 刷新不等同于账号切换。

## Review and validation

审查统一了刷新提示与自动恢复的窗口规则：两者只检查有效的短滚动窗口，退役的月度窗口不阻止恢复，只有退役/未知窗口时不能断言可用。

- `cargo nextest run -p atomcode -p atomcode-coding -E 'test(tui_login::tests) | test(runtime::goal_recovery_tests)'`：25 项通过。
- `bash gates/compile.sh`、`cargo fmt --all -- --check`、`git diff --check`：通过。
- coding 全量测试未全绿：`host_rows` 的共享等待预算测试和 `tests/recovery.rs` 的 4 项既有测试仍断言固定退避时长，与已存在的随机抖动实现不一致；排除首项后运行结果为 1263 通过、4 失败、12 跳过。本次未修改这些断言或通用重试算法。
- 尚未使用真实 CodingPlan 账号进行额度重置联调；loop 与刷新提前唤醒不在本阶段范围。
