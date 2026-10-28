# 后台构建等待：根因、四家对照与修复

## 基线与证据

检查起点：`release/v5.2.5`，`d8fed29d090ea9ce416decc244ee22e9692cbb84`。
工作区已有 provider 配置/UI 改动，实施期间另有 recap 等并行改动，均不属于本任务。

三方同模型对比产物位于 SWE-bench 的
`logs/comparison/20261022-taotoken-first5/`。第 4 题 AtomCode 正确调用
`bash_start` 启动构建，但三次空 `bash_poll` 后收到 exact guard 的禁止继续查询提示，
随后发出 `sleep 90`、`sleep 120`、`sleep 150`。原工具立即返回，没有带等待的查询。
长等待期间还使用 `find`/`ps` 猜测进度，最后才收集到编译器报错。
构建命令的 `| tail` 同时隐藏了实时输出并令 shell 最终退出码取决于 tail；本次不重写用户命令。
不能将 360 秒 sleep 全部视为可节省时间，编译本身也确实在运行。

## 本地源码对照

| 项目 | 实现位置 | 等待策略 |
| --- | --- | --- |
| opencode | `packages/core/src/tool/plugin/shell.ts` | background 返回任务；完成自动通知，明确禁止 sleep/重复读日志轮询。`Shell.wait` 以 Deferred 等待完成。 |
| codex | `codex-rs/core/src/unified_exec/process_manager.rs`、`unified_exec/mod.rs` | exec 先限时等待，返回 session；空 write_stdin 继续等待并可提前返回，等待上限与命令寿命分开。 |
| oh-my-pi | `packages/coding-agent/src/tools/bash.ts`、`exec/settings.ts`、`tools/wait.ts` | 默认开启自动后台化，阈值 60 秒；后台作业交由现有 manager，wait 等待所属任务/服务，结果可交付。 |
| deepseek-harness | `packages/shell/tool-bash/src/index.ts`、`packages/jobs/tool-jobs/src/index.ts` | 显式后台或前台等待超时提升为 job；job_output 可带 wait，默认等待 30 秒，有完成通知。 |

上述为本地 checkout 的当前源码；DeepSeek Harness 本次对比实际使用的是此前记录的 npm 包。

本次参考的 checkout：opencode `e5a557ef`，codex `806d9732`，oh-my-pi `4585e9da`，
deepseek-harness `687f282e`。

## 选择与边界

选择增强现有 poll，而非这轮引入自动后台化或自动唤醒。后两者涉及命令超时、
idle turn 恢复和消息交付，范围更大。

进程仍由 capabilities `background::reader_task` 和现有 STORE 持有。
`bash_poll` 新增 `wait_ms`，默认 10000，最大 60000，0 为立即查询。
未读输出、状态变化或退出可提前返回；超时返回当前状态，不终止构建。
Notify 只做唤醒提示，以现有共享状态为准；注册通知后检查状态，避免丢失唤醒。
等待不持有任务表锁；取消不消费输出、不杀后台任务，仍通过 bash_kill 停止。
不改变审批、跨平台 Shell、进程回收、持久化或 runtime 生命周期。

exact guard 和 repeat fuse 仅豁免成功的活跃 bash_poll：未传等待参数使用默认值，
显式等待参数须至少 1000ms。未知 job、错误、终态重复和不足 1 秒的快速查询继续适用原有保护。
不能只改 exact guard：否则 repeat fuse 仍会把正常等待当循环。

本地与 world shell 的提示统一引导长构建使用 start/poll，不用 sleep、不重复启动。

## 验证

- 虚拟时钟：输出提前返回、安静任务默认等待/上限/0 查询、退出与回收。
- 取消正在等待或已取消的查询，不丢未读输出，任务仍运行。
- 不存在的任务立即失败。
- 两个 guard 分别验证：连续活跃等待可继续；快速查询仍熔断。
- 现有 capability/harness/coding 回归、fmt、编译闸门。

这次改善等待行为，不承诺加速编译器；通用构建缓存、并行度、依赖版本管理另需实测。

## 初次实现验证结果（原开发检出）

2026-10-22，本机 macOS：

- `cargo nextest run -p atomcode-capabilities`：1115/1115 通过。
- `cargo nextest run -p atomcode-harness`：268/268 通过。
- `cargo nextest run -p atomcode-coding --test loop_policy`：24/24 通过。
- `bash gates/compile.sh`：全仓、feature 门与闸门阴性对照均通过。
- `cargo fmt --all -- --check`、`git diff --check`：通过；Cargo.lock 无改动。

虚拟时钟验证：指定最多等 60 秒时，200ms 的新输出立即返回，500ms 的退出立即返回；
安静任务默认等 10 秒，超大参数钳制到 60 秒，0 立即查询，取消在 100ms 返回且保留任务。
两道 guard 分别验证带等待查询可超过原熔断阈值，0/1ms 等待的重复查询仍停止。
尚未重新运行同模型 5 题耗时对比，未进行 Windows 实机验证。

## 审计与 release/v5.2.4 验证

目标分支基线：`11fae6ee67fb4f0aa6a3cb5a2e0bdb5e5b3cfdb6`。
使用独立 worktree 应用本轮补丁，未合入整个 release/v5.2.5。

审计收紧了极短等待的豁免条件：显式等待不足 1000ms 不豁免两道重复调用检查，
防止 `wait_ms=1` 变成快速空转的绕过方式。默认/空值继续采用 10 秒等待。
新增 1/999/1000ms 分类边界及 1ms 两道 guard 的集成回归。

目标分支复验：capabilities 1102/1102，harness 268/268，loop policy 24/24，
共 1394 项全部通过；格式检查与完整编译闸门通过。Cargo.lock 无改动，
未引入 hkdf、hmac、zeroize_derive。仍未重跑同模型基准或 Windows 实机测试。
