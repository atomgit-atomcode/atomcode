# P0 等待回归与 P1 命令结果指导

基线：release/v5.2.5，38e6a8381043aae498638899d1fc7353d994df99。

## 范围

P0 用同一 TaoToken/deepseek-v4.1-flash-1017，重跑 astropy-13236、13453。
原始 15 次对比不覆盖，独立产物位于 SWE-bench 的
`logs/comparison/20261022-p0-background-waits/`。每题新工作区、原题与基线，
每题 30 分钟，保持 high、1M context、32768 输出上限，先通过工具调用 smoke。
使用修改 P1 前固定的当前 dev 二进制，manifest 保存 SHA256、源码 SHA。
当前源码也包含后台完成通知和上下文预算，不是仅等待补丁的隔离实验；
dev 与原 release profile 不同，不根据本轮耗时断言等待补丁的加速比例。

P1 只在已有 tool-bash / tool-bash-world 行调整指导，两个替代装配路径一致：
构建/测试检查原退出码，过滤管道可能掩盖失败，长日志落盘后再提取摘要，
先保存退出状态、检查后返回原状态，遵循当前 shell 语法。
不新增工具、不自动改写命令、不启用全局 pipefail、不变更执行/审批/持久化。
日志是让模型显式创建的任务日志，不承诺工具自动保存未截断输出。
进程和结果 owner 仍为现有 capabilities shell；没有新的 runtime 状态或重试策略。

## 四家本地源码参考

| 项目与 commit | 相关位置 | 实际实现与本次取舍 |
| --- | --- | --- |
| opencode e5a557efee | packages/core/src/tool/plugin/shell.ts；packages/core/src/shell/result.ts | 提示依赖自动预览/完整输出文件；结构化 exit/signal/timeout 独立于输出。不把预览成功视为进程成功。 |
| codex 806d9732c9 | codex-rs/core/src/tools/context.rs::response_header；unified_exec/process_manager.rs | 结果头保留进程退出码/运行 session，与输出截断分离；没有从几行摘要推断成功。 |
| oh-my-pi 4585e9da4a | packages/coding-agent/src/tools/bash.ts::buildCompletedResult、throwIfUnfinished | 区分非零退出、取消、缺失状态与超时；带完整输出 artifact。本轮不复制其结果协议。 |
| deepseek-harness 687f282e86 | packages/shell/tool-bash/src/index.ts、render.ts | 长输出预览+spill 文件，独立退出标记；tool:bash prompt 要求检查退出码并调查失败。最直接参考其已有行内指导方式。 |

上述机制不能自动恢复任意管道左侧命令的退出码。检查目标执行路径未见普遍
强制全局 pipefail；项目 CI 脚本中的 set -euo pipefail 不作为工具行为的证据。

## 验证

P1 是提示文字改动，不增加字符串镜像测试。运行受影响 harness 回归、格式与 diff 检查。
P0 检查完整轨迹中的 start/poll、实际 wait_ms、工具循环警告、显式 shell sleep，
并保留推理补丁和统一评分；没有触发后台任务不能视为 poll 路径被实测覆盖。

P1 验证：harness 271/271、fmt 与 diff 检查通过。bash、zsh、sh 的真实子进程实验
均验证：失败命令接 tail 返回 0；先保存状态再展示摘要，保留原退出码 7 与完整日志。
不据此宣称模型一定遵循提示，也未做 Windows 实机验证。

P0 补充覆盖：coding 的 bounded_background_waits 集成回归通过（包含两道 guard，
默认/60000ms 等待可继续、0/1ms 快速查询仍拦截）。真实二进制同模型探针也通过：
启动安静任务、同 job 同 wait_ms=1000 连续四次等待，无循环警告，bash_kill 后收集终态。
该探针用于确保即使题目轨迹选择前台命令，也有实际后台路径的验证证据。

P0 执行中的直接证据：13453 首次后台构建出现
`ModuleNotFoundError: No module named 'setuptools.dep_util'`，但命令接了 tail，
工具报告 `status: exited (code 0)`；模型随后调整依赖重试。
这再次说明管道状态不能代表左侧构建状态，也正是 P1 指导针对的问题。
P0 固定二进制未包含 P1，不能拿这次重跑宣称新提示已改变模型行为。

## P0 最终结果

两题推理与官方评分均完成，无推理超时或评分基础设施故障：

| 题目 | 原耗时 → 本次 | 原循环警告 → 本次 | 原显式 shell sleep → 本次 | 原评分 → 本次 |
| --- | --- | --- | --- | --- |
| 13236 | 592.542s → 593.881s | 1 → 0 | 5/5s → 无 | 未通过 → 未通过 |
| 13453 | 592.294s → 179.255s | 1 → 0 | 90/120/150s → 无 | 通过 → 通过 |

13236 本次未调用后台工具；13453 两次 bash_start，两次 poll（0、60000ms）。
因此正常重复等待的直接覆盖来自上述真实探针与集成回归，而非两次题目运行都覆盖。
不宣称解题率提升或固定加速百分比。完整轨迹、补丁、评分和行为审计保存在独立产物目录，
`summary.zh-CN.md`、`behavior-audit.json` 与 `probes/background-poll/probe-result.json` 为入口。
