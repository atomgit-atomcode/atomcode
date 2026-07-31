# 本地定时任务 `atomcode schedule` —— 阶段 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `schedule add` 自动注册 OS 调度条目(launchd/Task Scheduler/systemd-timer)到点唤醒 `atomcode schedule run <id>`,并让 scheduled 执行走更严 approver(无人值守下拒绝危险/越界 bash)。

**Architecture:** 新 `OsScheduler` trait + 3 平台 cfg-gated 实现(命令走注入的 `CommandRunner`、文件根可注入 → 纯逻辑可测,不真动系统)。`Schedule`→OS 规格是纯函数。接线进阶段 1 的 schedule_cmd.rs（add/remove/enable/disable/sync）。I1:`run_native_headless` 加 `strict_unattended` 参数不再 blanket-approve bash;`run_task` 从不全 bypass gates、auto 封顶为 accept-edits-级 gating。

**Tech Stack:** Rust。crate `atomcode-cli`(schedule_os.rs 新建 + schedule_cmd.rs + main.rs)。无新第三方依赖(plist/unit/schtasks 都是字符串生成 + 进程调用)。

## Global Constraints

- 纯本地,无云端。
- 条目命令 = `std::env::current_exe()` 的**绝对路径** + `schedule run <id>`,headless、无终端。
- 平台:macOS launchd / Windows Task Scheduler / Linux systemd-timer(无 systemd→crontab),用 `#[cfg(target_os=...)]` 分区。
- `install` 失败:**警告不回滚**(保留任务定义,提示 `schedule sync`)。`uninstall` 幂等。
- Cron kind 在某平台无法表达 → `install` 返回明确错误(不静默失败)。
- **I1 安全**:scheduled 执行**从不设 skip_permissions / 从不全 bypass gates**;危险/越界 bash(经 BashWorkspaceGate 升级到审批的)一律拒;安全操作按 mode 自动放行。Plan 只读不变。
- 可测:命令走 `CommandRunner`(测试注入 fake 断言参数)、文件写到注入的根(tempdir);真装 OS 条目 + 到点触发需真机、不自动化。
- 分支 release/v5.0.4。提交显式 pathspec(工作树可能有无关 foreign WIP)。
- 设计源:`docs/superpowers/specs/2026-07-31-local-scheduled-tasks-phase2-design.md`。上游阶段 1:`atomcode_config::schedule::{ScheduleTask, Schedule}`。

---

### Task 1: Schedule→OS 翻译纯函数 + OsScheduler trait + CommandRunner

**Files:**
- Create: `crates/atomcode-cli/src/schedule_os.rs`
- Modify: `crates/atomcode-cli/src/main.rs`(加 `mod schedule_os;`)

**Interfaces:**
- Consumes: `atomcode_config::schedule::{ScheduleTask, Schedule}`。
- Produces:
  - `pub trait CommandRunner { fn run(&self, program: &str, args: &[String]) -> std::io::Result<std::process::Output>; }` + `pub struct RealCommandRunner`(用 `std::process::Command`)。
  - `pub trait OsScheduler { fn install(&self, task: &ScheduleTask) -> anyhow::Result<()>; fn uninstall(&self, id: &str) -> anyhow::Result<()>; fn status(&self, id: &str) -> InstallState; }`
  - `pub enum InstallState { Installed, Missing }`
  - 纯翻译函数(下列),供各平台 impl 用,单独可测。

- [ ] **Step 1: 写失败测试**(schedule_os.rs 内 `#[cfg(test)] mod tests`;测翻译纯函数)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_config::schedule::Schedule;

    #[test]
    fn systemd_oncalendar_translation() {
        assert_eq!(systemd_calendar(&Schedule::Daily { time: "09:30".into() }).unwrap(),
                   OnCalendar::Calendar("*-*-* 09:30:00".into()));
        assert_eq!(systemd_calendar(&Schedule::Hourly).unwrap(),
                   OnCalendar::Calendar("hourly".into()));
        assert_eq!(systemd_calendar(&Schedule::Interval { every_minutes: 30 }).unwrap(),
                   OnCalendar::Interval("30min".into()));
        assert_eq!(systemd_calendar(&Schedule::Weekly { weekday: 1, time: "16:00".into() }).unwrap(),
                   OnCalendar::Calendar("Mon *-*-* 16:00:00".into()));
    }

    #[test]
    fn launchd_calendar_translation() {
        // Daily 09:30 → {Hour:9, Minute:30}
        let d = launchd_calendar(&Schedule::Daily { time: "09:30".into() }).unwrap();
        assert_eq!(d, LaunchdTrigger::Calendar { hour: Some(9), minute: Some(30), weekday: None });
        // Interval 30min → StartInterval 1800
        assert_eq!(launchd_calendar(&Schedule::Interval { every_minutes: 30 }).unwrap(),
                   LaunchdTrigger::Interval(1800));
    }

    #[test]
    fn schtasks_args_translation() {
        let a = schtasks_args(&Schedule::Daily { time: "09:30".into() }).unwrap();
        assert!(a.contains(&"/SC".to_string()) && a.contains(&"DAILY".to_string())
                && a.windows(2).any(|w| w[0] == "/ST" && w[1] == "09:30"));
    }

    #[test]
    fn cron_kind_rejected_on_launchd() {
        assert!(launchd_calendar(&Schedule::Cron { expr: "0 9 * * *".into() }).is_err());
    }
}
```

- [ ] **Step 2: 跑确认失败** — `cargo test -p atomcode-cli --lib schedule_os::` → FAIL(未定义)。

- [ ] **Step 3: 实现翻译 + trait + CommandRunner**

```rust
use atomcode_config::schedule::{Schedule, ScheduleTask};

#[derive(Debug, PartialEq, Eq)]
pub enum InstallState { Installed, Missing }

pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<std::process::Output>;
}
pub struct RealCommandRunner;
impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[String]) -> std::io::Result<std::process::Output> {
        std::process::Command::new(program).args(args).output()
    }
}

pub trait OsScheduler {
    fn install(&self, task: &ScheduleTask) -> anyhow::Result<()>;
    fn uninstall(&self, id: &str) -> anyhow::Result<()>;
    fn status(&self, id: &str) -> InstallState;
}

fn hhmm(s: &str) -> anyhow::Result<(u8, u8)> {
    let (h, m) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("bad time {s}"))?;
    Ok((h.parse()?, m.parse()?))
}

// ---- systemd ----
#[derive(Debug, PartialEq, Eq)]
pub enum OnCalendar { Calendar(String), Interval(String) }
pub fn systemd_calendar(s: &Schedule) -> anyhow::Result<OnCalendar> {
    Ok(match s {
        Schedule::Daily { time } => { let (h,m)=hhmm(time)?; OnCalendar::Calendar(format!("*-*-* {h:02}:{m:02}:00")) }
        Schedule::Weekly { weekday, time } => { let (h,m)=hhmm(time)?; OnCalendar::Calendar(format!("{} *-*-* {h:02}:{m:02}:00", dow_abbr(*weekday)?)) }
        Schedule::Hourly => OnCalendar::Calendar("hourly".into()),
        Schedule::Interval { every_minutes } => OnCalendar::Interval(format!("{every_minutes}min")),
        Schedule::Cron { expr } => OnCalendar::Calendar(expr.clone()), // systemd 用 OnCalendar 表达,复杂 cron 退 crontab 由 impl 决定
    })
}
fn dow_abbr(wd: u8) -> anyhow::Result<&'static str> {
    Ok(["Mon","Tue","Wed","Thu","Fri","Sat","Sun"].get((wd.max(1)-1) as usize).copied()
        .ok_or_else(|| anyhow::anyhow!("bad weekday {wd}"))?)
}

// ---- launchd ----
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchdTrigger { Calendar { hour: Option<u8>, minute: Option<u8>, weekday: Option<u8> }, Interval(u64) }
pub fn launchd_calendar(s: &Schedule) -> anyhow::Result<LaunchdTrigger> {
    Ok(match s {
        Schedule::Daily { time } => { let (h,m)=hhmm(time)?; LaunchdTrigger::Calendar{hour:Some(h),minute:Some(m),weekday:None} }
        Schedule::Weekly { weekday, time } => { let (h,m)=hhmm(time)?; LaunchdTrigger::Calendar{hour:Some(h),minute:Some(m),weekday:Some(*weekday % 7)} } // launchd Sunday=0
        Schedule::Hourly => LaunchdTrigger::Calendar{hour:None,minute:Some(0),weekday:None},
        Schedule::Interval { every_minutes } => LaunchdTrigger::Interval(*every_minutes as u64 * 60),
        Schedule::Cron { .. } => anyhow::bail!("cron schedules are not supported on macOS launchd; use a simple frequency"),
    })
}

// ---- schtasks ----
pub fn schtasks_args(s: &Schedule) -> anyhow::Result<Vec<String>> {
    let v = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    Ok(match s {
        Schedule::Daily { time } => { let (h,m)=hhmm(time)?; v(&["/SC","DAILY","/ST"]).into_iter().chain([format!("{h:02}:{m:02}")]).collect() }
        Schedule::Weekly { weekday, time } => { let (h,m)=hhmm(time)?; v(&["/SC","WEEKLY","/D"]).into_iter().chain([schtasks_dow(*weekday)?.into(), "/ST".into(), format!("{h:02}:{m:02}")]).collect() }
        Schedule::Hourly => v(&["/SC","HOURLY"]),
        Schedule::Interval { every_minutes } => v(&["/SC","MINUTE","/MO"]).into_iter().chain([every_minutes.to_string()]).collect(),
        Schedule::Cron { .. } => anyhow::bail!("cron schedules are not supported on Windows Task Scheduler; use a simple frequency"),
    })
}
fn schtasks_dow(wd: u8) -> anyhow::Result<&'static str> {
    Ok(["MON","TUE","WED","THU","FRI","SAT","SUN"].get((wd.max(1)-1) as usize).copied()
        .ok_or_else(|| anyhow::anyhow!("bad weekday {wd}"))?)
}
```

Add `mod schedule_os;` to main.rs.

- [ ] **Step 4: 跑通过 + 提交**
Run: `cargo test -p atomcode-cli --lib schedule_os::` → PASS。`cargo build -p atomcode-cli`。
```bash
git add crates/atomcode-cli/src/schedule_os.rs crates/atomcode-cli/src/main.rs
git commit -m "feat(schedule): OsScheduler trait + Schedule→OS translation" -- crates/atomcode-cli/src/schedule_os.rs crates/atomcode-cli/src/main.rs
```

---

### Task 2: 三平台 OsScheduler 实现(cfg-gated,CommandRunner + 文件根可注入)

**Files:**
- Modify: `crates/atomcode-cli/src/schedule_os.rs`

**Interfaces:**
- Consumes: Task 1 的 trait + 翻译函数 + CommandRunner。
- Produces: `Launchd`/`SystemdTimer`/`TaskSched` 结构(各含 `runner: Box<dyn CommandRunner>` + `root: PathBuf`);`pub fn current() -> Box<dyn OsScheduler>`(cfg 选平台,root=真实系统路径、runner=RealCommandRunner)。

- [ ] **Step 1: 写失败测试**(以 systemd 为例,注入 fake runner + tempdir root,断言写了 .service/.timer + 调了 systemctl)

```rust
struct FakeRunner { calls: std::sync::Mutex<Vec<(String, Vec<String>)>> }
impl CommandRunner for FakeRunner {
    fn run(&self, p: &str, a: &[String]) -> std::io::Result<std::process::Output> {
        self.calls.lock().unwrap().push((p.into(), a.to_vec()));
        Ok(std::process::Output { status: Default::default(), stdout: vec![], stderr: vec![] })
    }
}

#[test]
fn systemd_install_writes_units_and_enables() {
    let tmp = tempfile::tempdir().unwrap();
    let runner = std::sync::Arc::new(FakeRunner { calls: Default::default() });
    let sched = SystemdTimer { root: tmp.path().to_path_buf(), runner: runner.clone() };
    let task = /* ScheduleTask daily 09:30, id "t1" */;
    sched.install(&task).unwrap();
    // 写了 unit 文件
    assert!(tmp.path().join("atomcode-schedule-t1.service").exists());
    let timer = std::fs::read_to_string(tmp.path().join("atomcode-schedule-t1.timer")).unwrap();
    assert!(timer.contains("OnCalendar=*-*-* 09:30:00") && timer.contains("Persistent=true"));
    // 调了 systemctl --user enable --now
    let calls = runner.calls.lock().unwrap();
    assert!(calls.iter().any(|(p,a)| p=="systemctl" && a.iter().any(|x| x=="enable")));
    assert_eq!(sched.status("t1"), InstallState::Installed);
    sched.uninstall("t1").unwrap();
    assert!(!tmp.path().join("atomcode-schedule-t1.timer").exists());
}
```
(注:测试用的具体平台结构需 `#[cfg(...)]` 或让结构体非 cfg-gated、只有 `current()` cfg-gated——**推荐后者**:三个结构体都编译(不依赖平台特有 API,只生成字符串 + 调命令),这样每个平台的 impl 都能在任意开发机上单测;只有 `current()` 按 target_os 选。)

- [ ] **Step 2: 跑确认失败** → FAIL。

- [ ] **Step 3: 实现三平台**(结构体都可编译;install=生成条目内容 via Task1 翻译 + 写到 `root` 下 + 调 runner 激活;uninstall=删文件 + 调 runner 注销,幂等;status=文件存在性 + 可选查询)。exe 路径用 `std::env::current_exe()?`。命令:
  - Launchd:写 `<root>/com.atomcode.schedule.<id>.plist`(plist XML,ProgramArguments + StartCalendarInterval/StartInterval),`runner.run("launchctl", ["bootstrap","gui/<uid>",path])` / `["bootout",...]`。
  - Systemd:写 `.service`+`.timer` 到 `<root>`,`runner.run("systemctl",["--user","daemon-reload"])` + `["--user","enable","--now",unit]` / `["--user","disable","--now",unit]`;**crontab fallback** 另判(`which systemctl` 失败时,用 `crontab` 读改写带 `# atomcode-schedule:<id>` 标记的行)——本任务可先只做 systemd,crontab fallback 作为 Task 2 内的次条目或紧跟的小步骤。
  - TaskSched:`runner.run("schtasks",["/Create","/F","/TN",format!("atomcode\\schedule\\{id}"),"/TR",format!("\"{exe}\" schedule run {id}"), ...schtasks_args])` / `["/Delete","/F","/TN",...]`。

- [ ] **Step 4: 跑通过 + 提交** — `cargo test -p atomcode-cli --lib schedule_os::`(至少 systemd 那套 fake-runner 测试)+ `cargo build`。
```bash
git commit -m "feat(schedule): launchd/systemd/schtasks OsScheduler impls" -- crates/atomcode-cli/src/schedule_os.rs
```

---

### Task 3: 接线 add/remove/enable/disable + sync + list 状态

**Files:**
- Modify: `crates/atomcode-cli/src/schedule_cmd.rs`

**Interfaces:**
- Consumes: `schedule_os::{current, OsScheduler, InstallState}`;阶段 1 的 store。
- Produces: `ScheduleCli` 加 `Sync` 变体;各分支调 OsScheduler。

- [ ] **Step 1: 写失败测试**(纯逻辑:注入 fake OsScheduler,断言 add→install 被调、remove→uninstall、disable→uninstall、enable→install、sync→对每个 task install。用一个可注入的 scheduler 参数或把 handler 拆成接收 `&dyn OsScheduler` 的内部函数便于测试)

```rust
#[test]
fn add_registers_via_os_scheduler() {
    let fake = FakeScheduler::default();      // 记录 install/uninstall 调用
    // build_task + save + fake.install(&task)（走接收 &dyn OsScheduler 的内部 handler）
    handle_add_with(&fake, /* args… */).unwrap();
    assert_eq!(fake.installed(), vec!["<expected id>"]);
}
```

- [ ] **Step 2: 跑确认失败** → FAIL。

- [ ] **Step 3: 实现**
  - 把 add/remove/enable/disable/sync 的核心逻辑抽成接收 `&dyn OsScheduler` 的内部函数(便于注入 fake);`handle_schedule` 用 `schedule_os::current()` 注入真实实现。
  - add:save 成功后 `os.install(&task)`;失败 → `eprintln!` 警告 + 提示 `atomcode schedule sync`,**不删任务**。
  - remove:`os.uninstall(id)` + 删任务文件。disable:load→enabled=false→save + `os.uninstall(id)`。enable:load→enabled=true→save + `os.install(&task)`。
  - `Sync`:遍历 `schedule::list()`,enabled 的 `install`、disabled 的 `uninstall`。
  - `list`:每条追加 `os.status(&id)`(installed/missing)。

- [ ] **Step 4: 跑通过 + 提交** — `cargo test -p atomcode-cli`;`cargo build`。
```bash
git commit -m "feat(schedule): auto-register OS entries on add/enable + sync + list status" -- crates/atomcode-cli/src/schedule_cmd.rs
```

---

### Task 4: I1 —— scheduled 执行走更严 approver(拒危险/越界 bash)

**Files:**
- Modify: `crates/atomcode-cli/src/main.rs`(`run_native_headless` 审批循环 L2525-2548)
- Modify: `crates/atomcode-cli/src/schedule_cmd.rs`(`run_task`)

**Interfaces:**
- Consumes: `run_native_headless`(阶段 1 已 pub(crate))。
- Produces: `run_native_headless` 增参 `strict_unattended: bool`(所有现有调用点传 `false`,保持 `-p` 行为不变;`run_task` 传 `true`)。

- [ ] **Step 1: 写失败测试**(把审批决策抽成纯函数便于测)

```rust
// 在 main.rs(或 schedule_cmd.rs)加纯函数:给定 (strict_unattended, skip_permissions, tool) → allow?
#[test]
fn strict_unattended_denies_escalated_bash() {
    // 正常 -p:bash 升级到审批 → 放行(现状)
    assert!(headless_auto_approve(false, false, "bash"));
    // scheduled(strict):bash 升级到审批(=危险/越界)→ 拒
    assert!(!headless_auto_approve(true, false, "bash"));
    // scheduled(strict):非 bash 需审批 → 拒(无人可审)
    assert!(!headless_auto_approve(true, false, "edit_file"));
    // strict 下即使 skip_permissions 也不放行危险项(scheduled 从不 skip,但防御)
    assert!(!headless_auto_approve(true, true, "bash"));
}
```

- [ ] **Step 2: 跑确认失败** → FAIL(`headless_auto_approve` 未定义)。

- [ ] **Step 3: 实现**
  纯函数:
```rust
/// Auto-approve decision for headless approval requests. A request only reaches
/// this point when a gate already escalated the tool call to approval (i.e. it
/// is NOT trivially safe). `-p` blanket-approves bash; scheduled (strict_unattended)
/// refuses — no human is present to vet a destructive/out-of-workspace bash.
fn headless_auto_approve(strict_unattended: bool, skip_permissions: bool, tool: &str) -> bool {
    if strict_unattended { return false; }          // scheduled: deny everything escalated
    skip_permissions || tool == "bash"              // -p: current behavior
}
```
  改审批循环 L2529-2540:把 `if skip_permissions || approval.tool == "bash"` 换成 `if headless_auto_approve(strict_unattended, skip_permissions, &approval.tool)`。给 `run_native_headless` 加参 `strict_unattended: bool`;现有调用点(顶层 `-p` 分支)传 `false`。
  `run_task`(schedule_cmd.rs):
  - **从不设 skip_permissions / 从不全 bypass**:`runtime_config_from(..., dangerously_skip_permissions=false, ...)`(不再用 `mode.is_auto()` 开 skip);spawn 后按 mode set_mode,但 **auto 封顶为 accept-edits 级 gating**(避免 Auto/bypass 在中间件层放行危险 bash):`let effective = if task_mode==Auto { AcceptEdits } else { task_mode }; runtime.handle.set_mode(effective)`(Plan/AcceptEdits 照常)。
  - 调 `run_native_headless(..., strict_unattended=true)`,`skip_permissions` 参也传 `false`。
  - 在代码/文档注明:scheduled 任务**不做完整 bypass**(无人值守安全),auto 等价 accept-edits + 严格 bash。

- [ ] **Step 4: 跑通过 + 提交** — `cargo test -p atomcode-cli`(纯函数测试 + 全量);`cargo build -p atomcode`。
```bash
git commit -m "feat(schedule): strict unattended approver — deny risky bash for scheduled runs" -- crates/atomcode-cli/src/main.rs crates/atomcode-cli/src/schedule_cmd.rs
```

---

## Self-Review

**1. Spec coverage:**
- OsScheduler trait + 翻译 + CommandRunner → Task 1。
- 三平台 install/uninstall/status(+crontab fallback)→ Task 2。
- add/remove/enable/disable 接线 + sync + list 状态 → Task 3。
- I1 更严 approver(拒危险 bash、从不 bypass、auto 封顶)→ Task 4。
- install 失败警告不回滚 → Task 3 add 分支。cron 无法表达→报错 → Task 1 翻译 bail。✅
- DEFER(webui UI / Task 2b / 云端)→ 全程无。✅

**2. Placeholder scan:** Task 2 的平台命令细节(launchctl/systemctl/schtasks 参数)给了具体命令+参数形态;crontab fallback 标为 Task 2 内次条目(真实集成点,非 TBD)。Task 4 给了纯函数 + 精确改点 L2529。无 TBD。

**3. Type consistency:** `OsScheduler`/`CommandRunner`/`InstallState`/翻译函数签名贯穿 Task 1→2→3;`headless_auto_approve(strict_unattended,skip_permissions,tool)` + `run_native_headless` 增参 `strict_unattended: bool` 一致;`Schedule`/`ScheduleTask` 来自阶段 1。✅

## 非目标(阶段 2 不做)
webui/桌面「定时任务」面板;Task 2b catalog 过滤 scheduled 会话;云端;比 OS 更强的自定义补跑。
