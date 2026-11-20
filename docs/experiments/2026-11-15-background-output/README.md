# 后台输出与 poll 实测（2026-11-15）

检验 Windows WebUI 失联报告中的两个猜测，不把资源风险直接等同于事故根因。
本机为 macOS；未修改生产输出队列行为。使用真实 LocalShell 子进程及真实
bash_start/poll 路径，有限输出，不调用模型、不操作用户会话。

每个子进程输出 stdout 8 MiB、stderr 2 MiB。每个实验独立测试进程，最多 9 个
子进程、90 MiB 输出。RSS 仅包含测试进程，不包含 Python 子进程。暂停消费组
在子进程退出前不调用 next_chunk，之后核实全部字节仍可读取。正常后台组持续
运行生产 pump，核实每个结果环形缓冲不超过 256 KiB。长等待组每 0.4 秒输出
64 KiB，共约 65 秒，使用真实墙钟。HTTP 探测是同一四线程 Tokio 运行时上的
最小 loopback 服务，不是完整 daemon/WebUI。

| 场景 | 总输出 | 基线 RSS | 积压时/峰值 RSS | HTTP 失败 |
|---|---:|---:|---:|---:|
| 1 个 LocalShell，暂停消费 | 10 MiB | 8.0 MiB | 22.4 MiB | 0 |
| 4 个 LocalShell，暂停消费 | 40 MiB | 8.0 MiB | 52.9 MiB | 0 |
| 9 个 LocalShell，暂停消费 | 90 MiB | 8.2 MiB | 103.5 MiB | 0 |
| 9 个后台任务，正常消费 | 90 MiB | 7.9 MiB | 峰值 27.7 MiB | 0 |
| 9 个后台任务，真实长 poll | 90 MiB | 8.3 MiB | 峰值 26.7 MiB | 0 |

正常后台组耗时 833 ms，最终环形缓冲总量 2.25 MiB，HTTP 最慢 74 ms。
长等待组耗时 64,766 ms，18 次 poll，最长单次 60,000 ms；HTTP 最慢 13 ms，
2,737 次探测全部成功。原始数据见 results.json；RSS 和延迟会随机器负载变化。

确认慢/停止消费者会使前置输出队列随积压增长，结果缓冲上限不覆盖该队列。
但正常消费及真实超过 60 秒的九任务并发均未复现失联或退出，不能将队列认定为
原事故根因，也不能排除更长时间或更大负载下的资源问题。未验证 Windows Job
Object、Git Bash、完整 WebUI 状态/锁或原始视频任务。后续需 Windows 同路径压测
和进程退出码/转储，不能以本机阴性结果替代 Windows 复现。

实验为 Unix-only ignored 测试，依赖 /usr/bin/python3 和 /bin/ps；不进入普通 CI。
真实进程与墙钟用于测量 RSS 和调度，不适用虚拟时钟。

```sh
cargo nextest run -p atomcode-capabilities --lib --run-ignored only -E 'test(local_output_experiment) & !test(long_poll)' --no-capture
cargo nextest run -p atomcode-capabilities --lib --run-ignored only -E 'test(local_output_experiment_long_poll_9)' --no-capture
```

验证：5 组手动实验通过；capabilities 常规测试 1,370 通过、6 跳过（含 5 组手动实验）；完整编译门、格式及 diff 检查通过。生产队列实现尚未修改，故障根因仍未确认。
