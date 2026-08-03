# 本地定时任务 `atomcode schedule` —— 阶段 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 atomcode 支持纯本地定时任务的存储、管理与执行:`atomcode schedule add/list/remove/enable/disable/run`,`run` 复用 headless 跑任务并把结果落成一个标记为 scheduled 的 session + 通知。

**Architecture:** 任务定义存 `~/.atomcode/schedules/<id>.json`(atomcode-config 新模块)。`schedule run <id>` 复用 CLI 现有 headless bootstrap(`runtime_config_from` → `spawn_native_cli_runtime(Fresh)` → `run_native_headless`),新建的 session 被打上 `origin=Scheduled` + `schedule_id`;普通会话列表默认过滤 scheduled。阶段 1 不含 OS 调度器(阶段 2)。

**Tech Stack:** Rust。crates: `atomcode-config`(store)、`atomcode-capabilities`(SessionMeta origin)、`atomcode-cli`(子命令+执行器)。无新第三方依赖(cron 表达式阶段 1 只存不算)。

## Global Constraints

- **纯本地,无云端。** 阶段 1 不碰 OS 调度器;`schedule run` 是执行入口(手动/外部 cron/阶段 2 OS 触发)。
- 任务 store:`~/.atomcode/schedules/<id>.json`,一任务一文件。config 根用 `atomcode_config::config::Config::config_dir()`(`$ATOMCODE_HOME` 或 `~/.atomcode`)。
- 权限模式默认 **Plan**;任务可配 `plan`/`accept_edits`/`auto`。映射到 `atomcode_coding::RuntimeMode`(`Plan`/`AcceptEdits`/`Auto`)。
- 结果:每次运行 **新建 session**(不复用),`origin=Scheduled`,`schedule_id=<task id>`;完成后按任务 `notify` 级别发通知(复用 `notify_turn_finished`)。
- SessionMeta 新增 `origin` 字段 **`#[serde(default)]`**,旧会话反序列化为 `Manual`(向后兼容)。普通列表(/resume、webui)默认排除 `Scheduled`。
- 分支 `feat/schedule-local-tasks`。提交用显式 pathspec(工作树有无关 foreign WIP,勿混入)。
- 频率:简单频率(daily/weekly/hourly/interval)`next_run` 可算;`cron` kind 阶段 1 只存储、`next_run` 返回 None(真正 cron 触发是阶段 2 OS 调度器的事)。
- 设计源:`docs/superpowers/specs/2026-07-31-local-scheduled-tasks-design.md`。

---

### Task 1: ScheduleTask 模型 + store + next_run(atomcode-config)

**Files:**
- Create: `crates/atomcode-config/src/schedule.rs`
- Modify: `crates/atomcode-config/src/lib.rs`(加 `pub mod schedule;`)

**Interfaces:**
- Consumes: `atomcode_config::config::Config::config_dir() -> PathBuf`(mod.rs L1529)。
- Produces:
  - `pub struct ScheduleTask { id, title, prompt, cwd, schedule: Schedule, permission_mode: String, notify: String, enabled: bool, created_at: i64, last_run_at: Option<i64>, last_status: Option<String> }`
  - `pub enum Schedule { Daily{time}, Weekly{weekday,time}, Hourly, Interval{every_minutes}, Cron{expr} }`(serde tag = "kind")
  - `pub fn schedules_root() -> PathBuf`、`pub fn save(&ScheduleTask)`, `pub fn load(id) -> Result<ScheduleTask>`, `pub fn list() -> Vec<ScheduleTask>`, `pub fn remove(id) -> Result<()>`
  - `pub fn next_run(&Schedule, now_epoch_secs: i64) -> Option<i64>`

- [ ] **Step 1: 写失败测试**（`schedule.rs` 内 `#[cfg(test)] mod tests`）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ScheduleTask {
        ScheduleTask {
            id: "t1".into(), title: "Daily brief".into(), prompt: "summarize".into(),
            cwd: "/tmp/proj".into(), schedule: Schedule::Daily { time: "09:00".into() },
            permission_mode: "plan".into(), notify: "important".into(), enabled: true,
            created_at: 0, last_run_at: None, last_status: None,
        }
    }

    #[test]
    fn task_json_roundtrips() {
        let t = sample();
        let json = serde_json::to_string(&t).unwrap();
        let back: ScheduleTask = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "t1");
        assert!(matches!(back.schedule, Schedule::Daily { .. }));
        assert_eq!(back.permission_mode, "plan");
    }

    #[test]
    fn store_save_load_list_remove_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", tmp.path());   // isolate config_dir
        let t = sample();
        save(&t).unwrap();
        assert_eq!(load("t1").unwrap().title, "Daily brief");
        assert_eq!(list().len(), 1);
        remove("t1").unwrap();
        assert!(list().is_empty());
        std::env::remove_var("ATOMCODE_HOME");
    }

    #[test]
    fn next_run_daily_is_today_or_tomorrow_at_time() {
        // 2026-07-31 08:00:00 UTC = 1785657600 ; daily 09:00 → same day 09:00
        let now = 1785657600;
        let nr = next_run(&Schedule::Daily { time: "09:00".into() }, now).unwrap();
        assert!(nr > now && nr - now <= 24 * 3600);
    }

    #[test]
    fn next_run_cron_is_none_in_phase1() {
        assert_eq!(next_run(&Schedule::Cron { expr: "0 9 * * *".into() }, 0), None);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-config --lib schedule::`
Expected: FAIL(模块/类型未定义,编译错误)。

- [ ] **Step 3: 实现 `schedule.rs`**

```rust
use std::path::PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Schedule {
    Daily { time: String },                        // "HH:MM"
    Weekly { weekday: u8, time: String },          // weekday 1..=7 (1=Mon)
    Hourly,
    Interval { every_minutes: u32 },
    Cron { expr: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScheduleTask {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub cwd: String,
    pub schedule: Schedule,
    #[serde(default = "default_mode")]
    pub permission_mode: String,   // "plan" | "accept_edits" | "auto"
    #[serde(default = "default_notify")]
    pub notify: String,            // "off" | "important" | "all"
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub last_run_at: Option<i64>,
    #[serde(default)]
    pub last_status: Option<String>,
}

fn default_mode() -> String { "plan".into() }
fn default_notify() -> String { "important".into() }
fn default_true() -> bool { true }

pub fn schedules_root() -> PathBuf {
    crate::config::Config::config_dir().join("schedules")
}

fn task_path(id: &str) -> PathBuf { schedules_root().join(format!("{id}.json")) }

pub fn save(task: &ScheduleTask) -> std::io::Result<()> {
    let root = schedules_root();
    std::fs::create_dir_all(&root)?;
    let bytes = serde_json::to_vec_pretty(task)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(task_path(&task.id), bytes)
}

pub fn load(id: &str) -> std::io::Result<ScheduleTask> {
    let bytes = std::fs::read(task_path(id))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

pub fn list() -> Vec<ScheduleTask> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(schedules_root()) else { return out };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
        if let Ok(bytes) = std::fs::read(&p) {
            if let Ok(t) = serde_json::from_slice::<ScheduleTask>(&bytes) {
                out.push(t);   // corrupt files are skipped
            }
        }
    }
    out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    out
}

pub fn remove(id: &str) -> std::io::Result<()> {
    match std::fs::remove_file(task_path(id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Next fire time (epoch secs) for simple frequencies. `Cron` returns None in
/// phase 1 (its real firing is the phase-2 OS scheduler). Uses naive local-less
/// UTC arithmetic; day/hour rollover only (no DST handling — acceptable for the
/// list display, exact firing is the OS scheduler's job in phase 2).
pub fn next_run(schedule: &Schedule, now_epoch_secs: i64) -> Option<i64> {
    fn hhmm(s: &str) -> Option<(i64, i64)> {
        let (h, m) = s.split_once(':')?;
        Some((h.parse().ok()?, m.parse().ok()?))
    }
    match schedule {
        Schedule::Interval { every_minutes } if *every_minutes > 0 =>
            Some(now_epoch_secs + (*every_minutes as i64) * 60),
        Schedule::Hourly => {
            let secs_into_hour = now_epoch_secs.rem_euclid(3600);
            Some(now_epoch_secs + (3600 - secs_into_hour))
        }
        Schedule::Daily { time } => {
            let (h, m) = hhmm(time)?;
            let day = now_epoch_secs.div_euclid(86400) * 86400;
            let target = day + h * 3600 + m * 60;
            Some(if target > now_epoch_secs { target } else { target + 86400 })
        }
        Schedule::Weekly { time, .. } => {
            // Phase 1 approximation: next day-boundary match of the time; exact
            // weekday alignment is delegated to the OS scheduler (phase 2).
            let (h, m) = hhmm(time)?;
            let day = now_epoch_secs.div_euclid(86400) * 86400;
            let target = day + h * 3600 + m * 60;
            Some(if target > now_epoch_secs { target } else { target + 86400 })
        }
        Schedule::Cron { .. } => None,
        Schedule::Interval { .. } => None,
    }
}
```

Add `pub mod schedule;` to `crates/atomcode-config/src/lib.rs`. Ensure `tempfile` is a dev-dependency of `atomcode-config` (it already is — used by config/memory.rs tests).

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p atomcode-config --lib schedule::`
Expected: PASS(4 tests)。

- [ ] **Step 5: 跑 crate 全量 + 提交**

Run: `cargo test -p atomcode-config`
```bash
git add crates/atomcode-config/src/schedule.rs crates/atomcode-config/src/lib.rs
git commit -m "feat(schedule): task store + model + next_run in atomcode-config" -- crates/atomcode-config/src/schedule.rs crates/atomcode-config/src/lib.rs
```

---

### Task 2: SessionMeta `origin` 字段 + 列表默认过滤(atomcode-capabilities)

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/manager.rs`(SessionMeta struct L333;`SessionMeta::new` L390;`list()` L2715)

**Interfaces:**
- Produces: `pub enum SessionOrigin { Manual, Scheduled }`(default Manual);`SessionMeta.origin`;`SessionManager::list_visible() -> Vec<SessionMeta>`(排除 Scheduled)。既有 `list()` 保持返回全部(供 scheduled 视图用)。

- [ ] **Step 1: 写失败测试**（manager.rs 的 `#[cfg(test)] mod tests`,或新增测试）

```rust
#[test]
fn session_origin_defaults_manual_and_roundtrips() {
    let mut m = SessionMeta::new("s1", "/tmp/p", 0);
    assert_eq!(m.origin, SessionOrigin::Manual);   // default
    m.origin = SessionOrigin::Scheduled;
    let json = serde_json::to_string(&m).unwrap();
    let back: SessionMeta = serde_json::from_str(&json).unwrap();
    assert_eq!(back.origin, SessionOrigin::Scheduled);
    // old meta without the field → Manual
    let old = r#"{"id":"x","name":"n","working_dir":"/w","created_at":0,"updated_at":0}"#;
    let parsed: SessionMeta = serde_json::from_str(old).unwrap();
    assert_eq!(parsed.origin, SessionOrigin::Manual);
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-capabilities --lib session_origin_defaults_manual_and_roundtrips`
Expected: FAIL(`origin`/`SessionOrigin` 未定义)。

- [ ] **Step 3: 实现**

在 manager.rs 加枚举 + 字段:
```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionOrigin {
    #[default]
    Manual,
    Scheduled,
}
```
在 `SessionMeta` struct 加:
```rust
    #[serde(default)]
    pub origin: SessionOrigin,
```
在 `SessionMeta::new(...)` 的构造体里加 `origin: SessionOrigin::Manual,`(default 也可,但显式更清晰;若 struct 用 `..Default::default()` 则免)。

加过滤访问器(在 `impl SessionManager`,紧邻 `list()`):
```rust
/// Sessions for normal pickers (/resume, webui sidebar): excludes scheduled-run
/// sessions so recurring tasks don't flood the user's manual history. Use `list()`
/// for the full set (e.g. a scheduled-tasks view).
pub fn list_visible(&self) -> Vec<SessionMeta> {
    self.list().into_iter().filter(|m| m.origin != SessionOrigin::Scheduled).collect()
}
```

- [ ] **Step 4: 跑测试确认通过 + 提交**

Run: `cargo test -p atomcode-capabilities --lib session_origin_defaults_manual_and_roundtrips`
Expected: PASS。
```bash
git add crates/atomcode-capabilities/src/session/manager.rs
git commit -m "feat(session): add SessionOrigin + list_visible filter" -- crates/atomcode-capabilities/src/session/manager.rs
```

> ⚠️ 消费者接入(/resume 选择器 + webui 侧栏改用 `list_visible()`)放到 Task 4 之后的 Task 3.5 里做,或本任务内一并改——见 Task 2b。

### Task 2b: 普通列表消费者改用 `list_visible()`

**Files:**
- Modify: /resume 选择器的会话枚举数据源 + webui 侧栏会话数据源(实现者 grep `\.list()` 在 `crates/atomcode-tuix`、`crates/atomcode-daemon` 中的调用点,判断哪些是"给用户看的普通列表",改成 `list_visible()`;scheduled 视图/全量统计仍用 `list()`)。

- [ ] **Step 1**: grep `SessionManager` + `.list()` / `scan_all` / `scan_catalog` 的消费者;列出哪些是面向用户的会话选择器/侧栏。
- [ ] **Step 2**: 把这些改为 `list_visible()`(catalog 层若走 `scan_catalog`,加等价的 origin 过滤;实现者按 catalog 的 CatalogSession 是否带 origin 决定——如不带,Task 2 需把 origin 也带进 CatalogScan 的投影)。
- [ ] **Step 3**: 跑 `cargo test -p atomcode-tuix -p atomcode-daemon`,确认无回归。
- [ ] **Step 4**: 提交(显式 pathspec 只提改到的文件)。

> 说明:CatalogScan 是否携带 origin 是本任务的关键判断点。若 catalog 投影不含 origin,最小改动 = 在 CatalogSession 投影里带上 origin 并在用户列表处过滤。此为实现期需依代码确定的集成点。

---

### Task 3: CLI `schedule` 管理子命令(add/list/remove/enable/disable)

**Files:**
- Create: `crates/atomcode-cli/src/schedule_cmd.rs`(ScheduleCli enum + 管理处理函数)
- Modify: `crates/atomcode-cli/src/main.rs`(`Commands` enum L689 加 `Schedule`;dispatch)

**Interfaces:**
- Consumes: `atomcode_config::schedule::{ScheduleTask, Schedule, save, load, list, remove, next_run}`(Task 1)。
- Produces: `pub enum ScheduleCli { Add{...}, List, Remove{id}, Enable{id}, Disable{id}, Run{id} }`(Run 的处理在 Task 4)。`pub async fn handle_schedule(cli: ScheduleCli) -> anyhow::Result<i32>`。

- [ ] **Step 1: 写失败测试**（schedule_cmd.rs 内;测纯逻辑:参数→ScheduleTask 构造 + add/list/remove 对 store 的效果,用 ATOMCODE_HOME 隔离)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn add_builds_daily_task_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("ATOMCODE_HOME", tmp.path());
        let t = build_task("Brief", "summarize", "/tmp/p", Schedule::Daily { time: "09:00".into() }, "plan", "important");
        atomcode_config::schedule::save(&t).unwrap();
        let all = atomcode_config::schedule::list();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Brief");
        std::env::remove_var("ATOMCODE_HOME");
    }
}
```
(`build_task` 是把 CLI 参数组装成 `ScheduleTask` 的纯函数——含 id 生成:slug(title)+短随机后缀;created_at=now。)

- [ ] **Step 2: 跑确认失败** — `cargo test -p atomcode-cli --lib schedule_cmd::` → FAIL。

- [ ] **Step 3: 实现**
  - `ScheduleCli` clap enum(Add 带 `--title --prompt --cwd` + 频率互斥组 `--daily HH:MM` / `--weekly N@HH:MM` / `--every Nm` / `--hourly` / `--cron "…"` + `--mode plan|accept_edits|auto`(default plan)+ `--notify off|important|all`(default important))。
  - `build_task(...)` 纯函数组装 ScheduleTask(id = `slug(title)-<6位随机>`;随机用 `uuid::Uuid::new_v4()` 取前6位十六进制,避免新依赖)。
  - `handle_schedule`:Add→save;List→`list()` 逐条打印 `id | title | 下次:next_run | 上次:last_status | enabled`;Remove→`remove(id)`;Enable/Disable→`load`+改 enabled+`save`;Run→调 Task 4 的执行器。
  - main.rs `Commands` 加 `#[command(subcommand)] Schedule(schedule_cmd::ScheduleCli)`,dispatch 到 `handle_schedule`。

- [ ] **Step 4: 跑通过 + 提交**
Run: `cargo test -p atomcode-cli --lib schedule_cmd::` → PASS。
```bash
git add crates/atomcode-cli/src/schedule_cmd.rs crates/atomcode-cli/src/main.rs
git commit -m "feat(schedule): CLI add/list/remove/enable/disable subcommands" -- crates/atomcode-cli/src/schedule_cmd.rs crates/atomcode-cli/src/main.rs
```

---

### Task 4: `schedule run <id>` 执行器(复用 headless + origin 标记 + notify + last_run 回写)

**Files:**
- Modify: `crates/atomcode-cli/src/schedule_cmd.rs`(Run 分支 → 执行器);可能 `crates/atomcode-cli/src/main.rs`(把 headless bootstrap 的复用点暴露成 crate-内可调,或直接在 schedule_cmd 里复刻)。

**Interfaces:**
- Consumes(main.rs 现有,实现者复用/提取为 crate 内可见):
  - `runtime_config_from(config, working_dir, provider_override, telemetry, dangerously_skip_permissions, interactive) -> CodingRuntimeConfig`(main.rs L2213)
  - `spawn_native_cli_runtime(cfg, resume_session_id: Option<String>, bootstrap, fork_on_session_in_use, round_cap_checkpoint) -> (CodingRuntime, CodingAgentConfig, Option<ContinuedCliSession>)`(L2252)
  - `interactive_provider_bootstrap(&runtime_cfg) -> ProviderBootstrap`(L2240)
  - `run_native_headless(notifications_cfg, runtime, prompt, provider_name, verbose, capture, working_dir, skip_permissions, is_admin) -> (i32, Option<String>)`(L2362)
  - `CodingRuntime.session: Option<RuntimeSessionInfo>`(runtime.rs L647)——取新建 session id
  - `SessionManager::for_project(cwd).update_meta(id, |m| ...)`(manager.rs L1412)+ `SessionOrigin`(Task 2)
  - `RuntimeMode::{Plan, AcceptEdits, Auto}`;`runtime.handle.set_mode(mode)`

- [ ] **Step 1: 写失败测试**（执行器的纯逻辑可测部分:completion→last_status 映射、permission_mode 字符串→RuntimeMode 映射)

```rust
#[test]
fn permission_mode_str_maps_to_runtime_mode() {
    use atomcode_coding::RuntimeMode;
    assert_eq!(mode_from_str("plan"), RuntimeMode::Plan);
    assert_eq!(mode_from_str("accept_edits"), RuntimeMode::AcceptEdits);
    assert_eq!(mode_from_str("auto"), RuntimeMode::Auto);
    assert_eq!(mode_from_str("bogus"), RuntimeMode::Plan);  // safe default
}

#[test]
fn exit_code_maps_to_last_status() {
    assert_eq!(last_status_for(0), "ok");
    assert_eq!(last_status_for(130), "cancelled");
    assert_eq!(last_status_for(1), "error");
}
```

- [ ] **Step 2: 跑确认失败** — `cargo test -p atomcode-cli --lib schedule_cmd::` → FAIL(`mode_from_str`/`last_status_for` 未定义)。

- [ ] **Step 3: 实现执行器**
  纯函数先落地(供上面测试):
```rust
fn mode_from_str(s: &str) -> atomcode_coding::RuntimeMode {
    match s {
        "accept_edits" => atomcode_coding::RuntimeMode::AcceptEdits,
        "auto" => atomcode_coding::RuntimeMode::Auto,
        _ => atomcode_coding::RuntimeMode::Plan,   // plan + unknown → safe default
    }
}
fn last_status_for(exit_code: i32) -> &'static str {
    match exit_code { 0 => "ok", 130 => "cancelled", _ => "error" }
}
```
  执行器 `async fn run_task(id: &str) -> anyhow::Result<i32>`:
  1. `let mut task = atomcode_config::schedule::load(id)?;` 若 `!task.enabled` → 打印 skipped,返回 0。
  2. 载入 `Config`(复刻 main.rs 顶层 headless 载 config 的方式);`cwd = PathBuf::from(&task.cwd)`;`cwd` 不存在 → `last_status=error` + save + 返回非 0。
  3. `let runtime_cfg = runtime_config_from(&config, &cwd, None, telemetry, mode_from_str(&task.permission_mode).is_auto(), false);`(auto 走 skip_permissions=true;plan/accept_edits 走 false 再 set_mode)
  4. `let bootstrap = interactive_provider_bootstrap(&runtime_cfg);`
  5. `let (runtime, _agent, _cont) = spawn_native_cli_runtime(&runtime_cfg, None, bootstrap, false, false).await?;`(None → Fresh session)
  6. **打 origin 标记**:`if let Some(sid) = runtime.session.as_ref().and_then(|s| s.id-accessor) { SessionManager::for_project(&cwd).update_meta(&sid, |m| { m.origin = SessionOrigin::Scheduled; })?; }`(实现者确认 `RuntimeSessionInfo` 的 id 字段名——runtime.rs L143/647 附近)
  7. 非 auto 模式:`runtime.handle.set_mode(mode_from_str(&task.permission_mode)).await?;`(auto 已在 spawn 内 set)
  8. `let notifications_cfg = config.notifications(...)`(按任务 `notify`:off→构造 disabled 的 NotificationConfig;important/all→用 config 的 + 复用现有 `run_native_headless` 内的 `notify_turn_finished`)。
  9. `let (exit, _out) = run_native_headless(notifications_cfg, runtime, task.prompt.clone(), None, false, false, cwd.clone(), mode_from_str(&task.permission_mode).is_auto(), false).await?;`
  10. `task.last_run_at = Some(now_secs); task.last_status = Some(last_status_for(exit).into()); atomcode_config::schedule::save(&task)?;`
  11. 返回 `exit`。
  `handle_schedule` 的 `Run{id}` 分支 → `run_task(&id).await`。

  > 集成注意(实现者读 main.rs 顶层 `--prompt` headless 分支 L1342+ 与 L2362 的完整调用点作为参照,逐字复用同样的 config 载入 / telemetry / notifications_cfg 构造):本任务是**复刻现有 headless bootstrap**,不发明新流程。

- [ ] **Step 4: 跑通过** — `cargo test -p atomcode-cli --lib schedule_cmd::` → PASS(纯函数测试)。执行器端到端需真机(需 provider),阶段 1 不做自动化 e2e,靠纯函数单测 + 手动 `atomcode schedule run <id>` 验证。

- [ ] **Step 5: 全量 + 提交**
Run: `cargo test -p atomcode-cli`
```bash
git add crates/atomcode-cli/src/schedule_cmd.rs crates/atomcode-cli/src/main.rs
git commit -m "feat(schedule): schedule run executor (headless + scheduled-origin session + notify)" -- crates/atomcode-cli/src/schedule_cmd.rs crates/atomcode-cli/src/main.rs
```

---

## Self-Review

**1. Spec coverage:**
- 任务 store CRUD + `~/.atomcode/schedules` → Task 1. ✅
- next_run(简单频率算/cron None) → Task 1. ✅
- CLI add/list/remove/enable/disable → Task 3. ✅
- `schedule run` 执行器(复用 headless + 新 session + notify + last_run) → Task 4. ✅
- SessionMeta origin + 普通列表默认过滤 → Task 2 + Task 2b. ✅
- 权限默认 plan、可提权 → Task 4 mode_from_str(default plan). ✅
- 简单频率 + cron 字段 → Task 1 Schedule enum. ✅
- 不碰云端 / 不碰 OS 调度器 → 全程无,阶段 2 defer. ✅

**2. Placeholder scan:** Task 2b 和 Task 4 有"实现者读参照/确认字段名/判断 catalog 是否带 origin"——这些是**真实的集成判断点**(依现有代码结构定),已给出确切 grep 目标 + 参照行号 + 决策规则,非 TBD。其余步骤含真实代码。

**3. Type consistency:** `ScheduleTask`/`Schedule` 字段贯穿 Task 1/3/4 一致;`SessionOrigin`/`origin`/`list_visible` 贯穿 Task 2/2b/4;`mode_from_str`/`last_status_for`/`run_task`/`handle_schedule`/`build_task` 命名一致;复用的 main.rs 函数签名逐字取自现有代码。✅

## 阶段 2(另出 spec,不在本计划)
三平台 OS 调度器自动注册/注销(launchd/schtasks/systemd-timer/crontab)+ 到点调 `atomcode schedule run <id>`。
