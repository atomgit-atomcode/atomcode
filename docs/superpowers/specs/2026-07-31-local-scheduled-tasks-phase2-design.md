# 本地定时任务 `atomcode schedule` —— 阶段 2 设计文档

- 日期：2026-07-31
- 分支：release/v5.0.4（承接阶段 1）
- 类型：新功能。阶段 2 = OS 调度器自动注册（"自动到点"）+ 无人值守安全加强（I1）。纯本地，无云端。
- 前置：阶段 1（store + CLI + `run_task` 执行器 + session origin 标记）已完成。设计源：`docs/superpowers/specs/2026-07-31-local-scheduled-tasks-design.md`。

## 背景

阶段 1 让 `atomcode schedule` 能存/管/执行任务，但"到点触发"需手动或外部 cron 调 `atomcode schedule run <id>`。阶段 2 把触发外包给 **OS 调度器**（launchd/Task Scheduler/systemd-timer），实现跨重启/关机补跑、无需常驻进程的自动到点。同时加强无人值守执行的安全（final-review 的 I1）。

## 已收敛的决策（brainstorming）

| 决策点 | 结论 |
|---|---|
| 注册时机 | **`schedule add` 自动注册** OS 条目；`remove`/`disable` 注销；`enable` 重注册。对齐"加了就跑"。 |
| I1 安全 | **纳入阶段 2**：scheduled 执行走**更严 approver**（破坏性/越界 bash 即使 accept_edits/auto 仍拒）。 |
| 平台机制 | macOS launchd / Windows Task Scheduler / Linux systemd-timer（无 systemd 退 crontab）。 |
| install 失败 | **警告不回滚**（保留任务定义，提示 `schedule sync` 重注册），不因系统注册失败丢任务定义。 |
| sync | 提供 `schedule sync`（按 store 重建所有 OS 条目，供重装/迁移/手删条目恢复）。 |

## 架构

```
schedule add ──save──► ~/.atomcode/schedules/<id>.json
     │ 自动
     ▼ OsScheduler::install(task)  ──► launchd plist / systemd unit+timer / schtasks
                                          │ 到点(跨重启/关机补跑)唤醒
                                          ▼  <abs-exe> schedule run <id>  (headless,无终端)
                                          └─► 阶段1 run_task + 更严 approver(I1)
schedule remove/disable ──► OsScheduler::uninstall(id)
schedule enable          ──► install ;  schedule sync ──► 按 store 重建全部
```

## 组件与文件结构

- **`OsScheduler` trait + 3 平台实现**（新文件 `crates/atomcode-cli/src/schedule_os.rs`；消费 `atomcode_config::schedule`）：
  ```rust
  pub enum InstallState { Installed, Missing }
  pub trait OsScheduler {
      fn install(&self, task: &ScheduleTask) -> anyhow::Result<()>;
      fn uninstall(&self, id: &str) -> anyhow::Result<()>;
      fn status(&self, id: &str) -> InstallState;
  }
  pub fn current() -> Box<dyn OsScheduler>;   // cfg(target_os) 选平台实现
  ```
  平台实现用 **`#[cfg(target_os = "…")]`** 分区（Launchd / TaskScheduler / SystemdTimer + crontab fallback）。为可测：spawn 命令走一个注入的 `CommandRunner`（trait，默认真跑 `std::process::Command`，测试注入 fake 断言参数），文件根可注入（测试用 tempdir，不写真实 `~/Library/...`）。
- **翻译纯函数**（同文件，无副作用、可单测）：`Schedule` → 各平台条目内容/参数。
- **接线**（改 `crates/atomcode-cli/src/schedule_cmd.rs`）：`add`/`remove`/`enable`/`disable` 调 `OsScheduler`；新增 `Sync` 子命令；`list` 显示注册状态。
- **I1 approver**（改 `run_task` 及其审批路径）：scheduled 执行不复用 `-p` 的 bash-blanket-approve，改用更严 decider。

## OS 条目细节

条目命令统一 = **当前 atomcode 可执行文件的绝对路径**（`std::env::current_exe()`）+ `schedule run <id>`，headless、`windowsHide`、无终端依赖。

- **macOS（launchd）**：`~/Library/LaunchAgents/com.atomcode.schedule.<id>.plist`，`ProgramArguments`=[exe, "schedule", "run", id]；daily/weekly/hourly → `StartCalendarInterval`（含 Hour/Minute/Weekday）；interval → `StartInterval`=秒。`launchctl bootstrap gui/<uid>` 装、`bootout` 卸。launchd 在唤醒后合并补跑错过的 calendar 触发。
- **Linux（systemd user timer）**：`~/.config/systemd/user/atomcode-schedule-<id>.service`（`ExecStart`=exe schedule run id, `Type=oneshot`）+ `.timer`（`OnCalendar=`/`OnUnitActiveSec=` + **`Persistent=true`** 补跑）；`systemctl --user daemon-reload && enable --now <timer>` 装、`disable --now` + 删文件卸。**无 systemd** → 退 crontab（`crontab -l` 读 + 注入/删除带 `# atomcode-schedule:<id>` 标记的行）。
- **Windows（Task Scheduler）**：`schtasks /Create /TN "atomcode\schedule\<id>" /TR "<exe> schedule run <id>" /SC …`（daily/weekly/hourly/minute + `/MO N` + `/ST HH:MM` + `/D <day>`）/`/RU` 当前用户 + 允许错过后尽快补跑；`/Delete /F` 卸。

## Schedule → OS 翻译（纯函数）

| Schedule | launchd | systemd OnCalendar | schtasks |
|---|---|---|---|
| Daily{HH:MM} | StartCalendarInterval{Hour,Minute} | `*-*-* HH:MM:00` | `/SC DAILY /ST HH:MM` |
| Weekly{wd,HH:MM} | +Weekday | `<Dow> *-*-* HH:MM` | `/SC WEEKLY /D <DOW> /ST HH:MM` |
| Hourly | StartCalendarInterval{Minute:0} | `hourly` | `/SC HOURLY` |
| Interval{N min} | StartInterval=N*60 | `OnUnitActiveSec=Nmin` | `/SC MINUTE /MO N` |
| Cron{expr} | 拒绝并提示（launchd 无 cron 表达式）| 尽量转 `OnCalendar`，转不了退 crontab | 尽量，复杂拒绝提示 |

Cron kind 阶段 2 的支持：Linux 直接进 crontab / OnCalendar 最自然；mac/win 若无法表达则 `install` 返回明确错误（不静默失败）。

## I1 无人值守安全加强

阶段 1 的 `run_task` 复用 `run_native_headless`，后者对 `bash` 无条件自动批准（`skip_permissions || tool=="bash"`）。无人值守 + 递归/重复触发放大了破坏性/越界 bash 的风险。

阶段 2：scheduled 执行改用**更严 approval decider**：
- **Plan**：本就只读，不变。
- **accept_edits / auto**：自动批准该模式安全放行的操作，但**破坏性 / 工作区外 bash（由 `BashWorkspaceGate` 判定）一律拒绝**（而非 `-p` 的无条件放行）。被拒即让该工具调用失败（无人可审），任务继续或按失败收尾。
- 实现方向：run_task 不走 `run_native_headless` 的 bash-blanket 分支，而是提供一个 scheduled 专用 decider（复用 `BashWorkspaceGate`/`ApprovalMiddleware` 的判定，对"该 gate 判为危险"的一律 deny）。具体接入点 plan 期定（需读 `run_native_headless` 的审批循环）。

## 错误处理

- `install` 失败：警告 + 保留任务定义，提示 `schedule sync`；不回滚 save，不删任务。
- `current_exe()` 失败：`install` 报错（无法可靠定位 exe）。
- Cron 无法在该平台表达：`install` 返回明确错误（任务仍存，未注册）。
- `uninstall` 幂等（条目已不存在视为成功）。

## 测试计划

- **翻译纯函数**：每平台 × 每频率 → 期望条目内容/参数字符串（单测）。
- **install/uninstall**：注入 `CommandRunner`（记录被调命令与参数）+ 文件根（tempdir），断言：生成的 plist/unit/timer/schtasks 参数正确、写到对的相对路径、uninstall 幂等。不真动系统。
- **I1 approver**：破坏性/越界 bash 审批请求 → scheduled decider 返回 deny；安全操作 → allow；Plan 模式只读不触发。纯逻辑单测。
- 回归：`cargo test -p atomcode-cli`（+ 相关 crate）全绿。
- **端到端**（真装 OS 条目 + 到点真触发）需真机，不自动化。

## 范围

**IN（阶段 2）**：`OsScheduler` trait + 3 平台 install/uninstall/status + Schedule→OS 翻译 + add/remove/enable/disable 接线 + `schedule sync` + `list` 注册状态 + I1 更严 approver。

**DEFER / 非目标**：webui/桌面「定时任务」面板（后续对接）；Task 2b catalog 层过滤 scheduled 会话（仍 follow-up）；比 OS 更强的自定义补跑；云端（明确排除）。

## 建议任务切分（plan 细化）

1. `Schedule`→OS 翻译纯函数 + `OsScheduler` trait + `CommandRunner` 抽象。
2. 三平台 impl（cfg 分区，注入 CommandRunner/文件根可测）+ status。
3. 接线 `add/remove/enable/disable` + `sync` 子命令 + `list` 显示注册状态。
4. I1：scheduled 专用更严 approver 接入 run_task。
