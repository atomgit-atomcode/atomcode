# 本地定时任务 `atomcode schedule` —— 设计文档(阶段 1)

- 日期：2026-07-31
- 分支：feat/schedule-local-tasks（当前基于 release/v5.0.4 顶；实现前可 rebase 到 main）
- 类型：新功能。**纯本地**（无云端）。对标 Claude Code Desktop 的本地 scheduled tasks / ChatGPT「已安排的任务」，但调度与执行全在本机。
- 本 spec 只覆盖 **阶段 1**（任务 store + CLI + 执行器 + session 归属）；**阶段 2**（三平台 OS 调度器自动注册）另出 spec。

## 背景与目标

桌面端需要"定时/周期任务"能力，复用 atomcode 本地能力、**不走云端**。调研（opencode/codex/oh-my-pi）结论：三家都无原生本地调度，均用"外部 OS 调度器 + headless 单次运行"。atomcode 优势：已有常驻 daemon、headless `-p`、notify、持久 session。Claude Code 的 `/schedule`(routines) 是**云端**；其 **Desktop 本地 scheduled tasks** 才是本目标对标物。

**目标**：atomcode 把"外部 cron + headless"这套**收成一等公民**——用户用 `atomcode schedule` 管理任务，atomcode 存定义、到点 headless 执行、结果落成 session + 通知。桌面端/webui UI 后续对接同一份任务 store。

## 已收敛的决策（brainstorming）

| 决策点 | 结论 |
|---|---|
| 到点触发 | **注册 OS 调度器**（launchd/Task Scheduler/systemd-timer）——阶段 2；阶段 1 先提供 `schedule run <id>` 执行入口，可手动或用任意外部 cron 触发 |
| 结果投递 | **每次运行新建 session + notify** |
| 权限模式 | 无人值守默认 **Plan/只读**，可每任务提权到 accept_edits/auto |
| 会话归属 | session 打 `origin=scheduled` 标记；**普通列表(/resume、webui)默认过滤**；结果仍可 resume/打开 |
| 频率 | **简单频率为主**（daily/weekly/hourly/interval + 时间），外加 `cron` 逃生舱 |
| 交付形态 | **CLI 优先**；webui/桌面 UI 后续对接 |

## 阶段 1 架构

```
atomcode schedule add/list/remove/enable/disable   ← 管理任务定义(store CRUD)
atomcode schedule run <id>                          ← 执行入口(手动 / 外部cron / 阶段2的OS触发)
        │                                                    │
        ▼ 读写                                                ▼ 载入任务
  ~/.atomcode/schedules/<id>.json                     复用 headless 执行
                                                             │ cwd + permission_mode + prompt
                                                             ▼
                                        新建 session(origin=scheduled, schedule_id)
                                                → 跑 → notify → 回写 last_run_at/last_status
```

阶段 1 **不含** OS 调度器注册（阶段 2）；因此阶段 1 结束时，功能闭环但"自动到点"需用户暂用外部 cron 调 `atomcode schedule run <id>`（或等阶段 2）。

## 组件与文件结构

- **`ScheduleTask` 模型 + store**（新模块 `crates/atomcode-config/src/schedule.rs`——config-dir 属主、配置性数据，且 `Config::config_dir()` 就在此 crate）：serde 结构 + `~/.atomcode/schedules/<id>.json` 的 load/save/list/remove（一任务一文件，避免并发 clobber，与 sessions 目录同风格）。纯 I/O + serde，可单测。
- **下次运行时间计算**（纯函数）：`next_run(schedule, now) -> Option<DateTime>`，供 `schedule list` 显示"下次运行"。纯函数，单测。
- **CLI 子命令 `schedule`**（`crates/atomcode-cli`）：clap 子命令 add/list/remove/enable/disable/run。
- **执行器**（CLI 内，`schedule run <id>`）：复用现有 headless 路径（`run_headless` 及其 completion/notify 机制），注入任务的 `cwd` / `permission_mode` / `prompt`，运行前把新 session 标 `origin=scheduled` + `schedule_id`，运行后按 `notify` 级别发通知、回写 `last_run_at`/`last_status`。
- **SessionMeta 加 `origin` 字段**（`atomcode-capabilities` / session manager）：`enum SessionOrigin { Manual, Scheduled }`，`#[serde(default)]` 默认 Manual（向后兼容旧会话）。
- **会话列表默认过滤**：session catalog 的列举路径（/resume 选择器数据源 + webui 侧栏数据源）默认排除 `origin=Scheduled`（提供一个包含参数以便"定时任务视图"取用）。

## 任务数据模型

`~/.atomcode/schedules/<id>.json`：
```
id: String                 // 稳定 id（slug 化 title + 短随机后缀，或 uuid）
title: String
prompt: String             // "描述 atomcode 应该做什么"
cwd: String                // 运行目录（项目）
schedule: {
  kind: "daily" | "weekly" | "hourly" | "interval" | "cron",
  time: "HH:MM",           // daily/weekly
  weekday: 1..7,           // weekly（1=周一）
  every_minutes: u32,      // interval
  cron: String,            // cron kind（逃生舱）
}
permission_mode: "plan" | "accept_edits" | "auto"   // 默认 "plan"
notify: "off" | "important" | "all"                 // 默认 "important"
enabled: bool                                        // 默认 true
created_at: i64
last_run_at: Option<i64>
last_status: Option<"ok" | "error" | "cancelled">
```

## 数据流（`schedule run <id>`）

1. 载入 `<id>.json`；`enabled=false` → 跳过（记 skipped，退出 0）。
2. 用 `cwd` + `permission_mode` + `prompt` 走 headless：新建 session，元数据标 `origin=Scheduled` + `schedule_id=<id>`。
3. 运行到 turn 终结。按 `notify` 级别发通知（important=仅完成/失败；all=含中间；off=不发）。
4. 回写 `last_run_at=now`、`last_status`（依 completion）。退出码复用 headless 的 completion→exit-code 映射。

## 错误处理

- 任务文件损坏/缺字段：`schedule run` 报错退出非 0，不影响其它任务；`list` 跳过并标"损坏"。
- headless 运行失败：记 `last_status=error` + notify（若非 off），退出非 0。
- `cwd` 不存在：报错退出非 0，`last_status=error`。

## 测试计划（阶段 1）

- **纯逻辑单测**：`ScheduleTask` serde 往返；`next_run` 对 daily/weekly/hourly/interval 的下次时间计算（固定 `now` 注入，不依赖真实时钟）；id 生成稳定性/去重。
- **store 单测**：临时目录下 save→load→list→remove 往返；损坏文件被 list 跳过。
- **session 标记**：`schedule run` 后新 session 的 `origin=Scheduled` + `schedule_id` 正确；catalog 默认列举**不含**该 session、带包含参数时**含**。
- **执行器**：复用既有 headless 测试脚手架，断言 last_run/last_status 回写 + notify 触发（mock notify）。
- 回归：`cargo test -p atomcode-cli -p atomcode-config -p atomcode-capabilities` 全绿；既有 session 无 origin 字段仍反序列化为 Manual。

## 范围

**IN（阶段 1）**：ScheduleTask 模型 + `~/.atomcode/schedules` store CRUD + `next_run` 计算 + CLI add/list/remove/enable/disable/run + 执行器（复用 headless + session 标记 + notify + last_run 回写）+ SessionMeta `origin` 字段 + 普通列表默认过滤 + 简单频率与 cron 字段。

**DEFER**：
- **阶段 2**：三平台 OS 调度器自动注册/注销（launchd/schtasks/systemd-timer/crontab）。
- webui/桌面「定时任务」面板（图里那套）+ 独立"定时任务视图"（阶段 1 用 CLI `schedule list` 顶）。
- 「建议」模板（图里的每日简报/每周回顾等预设）。
- 追加到固定会话（延续上下文）；比 OS 更强的补跑语义。

## 非目标

- 不做云端/远程调度（明确排除）。
- 阶段 1 不碰 OS 调度器（无"自动到点"，靠 `schedule run` 手动/外部触发）。
