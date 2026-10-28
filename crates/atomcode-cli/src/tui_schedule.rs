//! `/schedule`:排好的定时任务,以及它们下一次什么时候跑。
//!
//! Task management uses a shared host service; actual jobs remain OS-triggered.
//! The keyboard picker and editor are in `tui_schedule_ui`.

#[cfg(test)]
use atomcode_i18n::screen::{t as tr, Msg as SMsg};
use std::sync::Arc;

use async_trait::async_trait;
#[cfg(test)]
use atomcode_config::schedule::{self, ScheduleTask};
use atomcode_plexus::{Context, Plugin};
use atomcode_tui::plugin::CommandsSvc;
use serde_json::Value;

/// 行的名字。
pub const ROW: &str = "tui-schedule";

/// 命令名。
pub const COMMAND: &str = "schedule";

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// 挂上 `/schedule`。
pub struct ScheduleRow;

#[async_trait]
impl Plugin for ScheduleRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "create, edit, pause and inspect OS-scheduled tasks and their execution sessions"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let commands = ctx.require::<CommandsSvc>().map_err(|e| e.to_string())?;
        commands.add(Arc::new(super::tui_schedule_ui::ScheduleCommands::default()))?;
        Ok(())
    }
}

/// 排了什么,一行一条。纯函数,所以判据不必在磁盘上摆任务。
#[cfg(test)]
pub(crate) fn listing(tasks: &[ScheduleTask], now: i64) -> String {
    if tasks.is_empty() {
        return tr(SMsg::ScheduleNone).into_owned();
    }
    let mut out = String::new();
    for task in tasks {
        let next = schedule::next_run(&task.schedule, now)
            .map(when)
            .unwrap_or_else(|| "-".to_string());
        let last = task.last_status.clone().unwrap_or_else(|| "-".to_string());
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&tr(SMsg::ScheduleTaskLine {
            id: &task.id,
            title: &task.title,
            next: &next,
            last: &last,
            state: &tr(if task.enabled {
                SMsg::ScheduleOn
            } else {
                SMsg::ScheduleOff
            }),
        }));
    }
    out.push('\n');
    out.push_str(&tr(SMsg::ScheduleEditInTheCli));
    out
}

/// 一个时间点,写成人看的样子。与 CLI 的 `schedule list` 同一种写法(UTC),
/// 因为下一次触发时间本来就是按 UTC 算出来的。
pub(crate) fn when(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let rest = epoch_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}Z",
        rest / 3600,
        (rest % 3600) / 60
    )
}

/// Howard Hinnant 的 `civil_from_days`,和 CLI 那侧同一份算法。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_config::schedule::Schedule;

    fn task(id: &str, enabled: bool) -> ScheduleTask {
        ScheduleTask {
            id: id.into(),
            title: format!("{id} 的标题"),
            prompt: "do it".into(),
            cwd: "/tmp".into(),
            schedule: Schedule::Daily {
                time: "09:00".into(),
            },
            permission_mode: "plan".into(),
            notify: "important".into(),
            enabled,
            created_at: 0,
            last_run_at: None,
            last_status: None,
        }
    }

    #[test]
    fn nothing_scheduled_says_so_and_says_where_to_add_one() {
        let said = listing(&[], 0);
        assert!(said.contains("atomcode schedule add"), "{said}");
    }

    #[test]
    fn each_task_says_when_it_runs_next_and_whether_it_is_on() {
        // 2026-09-23T00:00:00Z,下一次日 9 点就是同一天。
        let now = 1_774_224_000;
        let said = listing(&[task("nightly", true), task("weekly", false)], now);
        assert!(said.contains("nightly"), "{said}");
        assert!(said.contains("09:00Z"), "下一次什么时候跑:{said}");
        assert!(
            said.lines().count() >= 3,
            "一条任务一行,外加末尾那句:{said}"
        );
    }
}
