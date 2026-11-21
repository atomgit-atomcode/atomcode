# 移除连续思考时间限制

用户要求移除长思考时间限制，暂不增加提醒。基线为 release/v5.2.8、
63d7e9e1d56206b73003cba9b6b18dd07861ed7e；工作区已有其他修改，予以保留。

## 边界

CodingRuntime 继续拥有 agent 生命周期，harness agent-loop 继续拥有流读取，
capabilities StreamWatchdog 只负责首内容与内容空闲时限。没有新的 owner、timer 或 hook。
持续 reasoning 是内容进展，不因累计时间长中断或降低 effort。

删除连续思考预算的配置字段、环境变量读取、装配参数、watchdog deadline、错误码、
超时 checkpoint、自动降档恢复状态及 TUI 提示。成员和所有 coding driver 通过共同路径生效。
旧配置字段按照既有 serde 未知字段行为忽略、不再写回；历史会话事件格式不变，
旧错误字符串仍可作为历史内容读取，不新增 importer。

保留首响应/空闲超时、重复思考检测、输出截断恢复、取消和外部任务总时限。
断流失败与终态继续走现有路径；不修改 turn_complete、压缩或持久化语义。

## 验证

以虚拟时钟证明持续思考 20 分钟后单请求成功、effort 不变、没有询问或提醒；
同时证明停止思考输出仍受空闲超时约束。旧模型配置可读且不再挂载预算。
执行受影响 crate 的 nextest、workspace 编译门和格式门。

验证结果：capabilities、harness、config、kernel、i18n、coding、tui 共 7,007 个
nextest 测试通过，13 个跳过；`bash gates/compile.sh` 全部通过；
`cargo fmt --all -- --check` 和 `git diff --check` 通过。未运行真实 provider 长任务。
