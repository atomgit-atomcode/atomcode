# 后台任务诊断计时

实现起点：`release/v5.2.5`，`52d370e6c4106a9243a37b0d5ff44473bcee30fa`。
此检出保留上一轮后台等待修复的工作区补丁；该修复已另行提交到 release/v5.2.4。
本次仅增加诊断日志，不改变工具输出、命令语义、退出码、等待上限、审批、UI 或持久化协议。
工作区其他配置/终端鼠标相关改动不属于本任务。

## 所有权与字段

后台进程仍只由 capabilities 的 reader_task 持有。计时使用 tokio 单调时钟，
只存在于当前进程，不持久化为会话事实。用 pid + job_id 关联不同进程的任务。

| 日志事件 | 级别 | 字段与含义 |
| --- | --- | --- |
| background_job_started | INFO | spawn_ms：world.spawn 获取进程句柄所用时间 |
| background_job_start_failed | INFO | spawn_ms + failed/unsupported；不记录原始错误正文 |
| background_job_exited | INFO | observed_runtime_ms：取得句柄到观察到 shell 退出；exit_code 保留既有规范化值 |
| background_job_finished | INFO | job_elapsed_ms：取得句柄到 reader 完成；post_exit_ms：退出后的输出排空、后代持管道与清理阶段 |
| background_poll_wait_finished | DEBUG | requested_wait_ms、effective_wait_ms、waited_ms；outcome 为 output/finished/status_changed/deadline/cancelled/abandoned |

观测耗时不是编译器 CPU 时间。poll 等待与进程运行可以重叠，不能直接相加。
这些日志不会自动区分“安装依赖”和“编译”，需结合已有任务记录关联。
取消或丢弃等待时的计时观察器只记录日志，不消费输出、不终止进程。
无效 job_id 不进入新增诊断日志；命令、工作目录、环境、输出和原始错误正文均不记录。

CLI 原有文件日志支持 RUST_LOG。启动时开启单模块 DEBUG 示例：

```sh
RUST_LOG='info,atomcode_capabilities::tools::bash::background=debug' atomcode
```

计时会沿用宿主的 tracing subscriber，不自行创建日志文件或日志线程。
tools feature 显式启用已有可选 tracing 依赖，使不启用 provider 的宿主也能编译和记录。

## 验证

用虚拟时钟和捕获 subscriber 检查实际日志：启动 100ms、输出等待 200ms、退出等待 300ms，
运行共 500ms；等待超时、取消、丢弃，以及启动失败均保留原有行为。
使用占位内容验证命令、路径、输出、原始错误和无效 job_id 不泄露进计时字段。
验证 tools-only feature，运行 capabilities 回归与全仓编译闸门；不以这次诊断改动宣称构建加速。

验证结果（2026-10-22，macOS）：后台任务相关 24/24、capabilities 全量 1119/1119
通过；tools-only lib 编译、完整 gates/compile.sh、fmt 与 diff 检查通过，Cargo.lock 无改动。
未重跑模型性能基准，未做 Windows 实机测试；本轮未改变失败重试提示或重试策略。

提交前复核：在 `c7d9d3859` 基线上再次通过 harness 268/268、coding loop policy
24/24，以及完整编译闸门和格式检查。后台等待修复作为独立提交移植到
release/v5.2.5，计时诊断随后单独提交；无关未跟踪文件保留。
