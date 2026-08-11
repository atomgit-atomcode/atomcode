use std::collections::BTreeMap;

use atomcode_capabilities::team::{TeamEvent, TeamEventPayload};

use crate::render::{SubtaskItem, SubtaskProgress, SubtaskStatus};

#[derive(Debug, Clone, Default)]
pub struct TeamProjection {
    generation: Option<u64>,
    runs: BTreeMap<String, RunProjection>,
    visible: bool,
}

#[derive(Debug, Clone, Default)]
struct RunProjection {
    last_seq: u64,
    total: usize,
    members: BTreeMap<String, MemberProjection>,
}

#[derive(Debug, Clone)]
struct MemberProjection {
    label: String,
    description: String,
    model: String,
    activity: String,
    status: SubtaskStatus,
    started_at: Option<std::time::Instant>,
    output_tokens: u64,
}

impl Default for MemberProjection {
    fn default() -> Self {
        Self {
            label: String::new(),
            description: String::new(),
            model: String::new(),
            activity: String::new(),
            status: SubtaskStatus::Pending,
            started_at: None,
            output_tokens: 0,
        }
    }
}

impl TeamProjection {
    pub fn apply(&mut self, generation: u64, event: TeamEvent) {
        if self.generation != Some(generation) {
            self.runs.clear();
            self.generation = Some(generation);
        }
        let run = self.runs.entry(event.run_id.to_string()).or_default();
        if event.seq <= run.last_seq {
            return;
        }
        run.last_seq = event.seq;
        match event.payload {
            TeamEventPayload::RunStarted { total } => {
                run.total = total;
                self.visible = true;
            }
            TeamEventPayload::MemberStarted {
                member_id,
                role,
                model,
                description,
            } => {
                let member = run.members.entry(member_id.to_string()).or_default();
                member.label = role.to_string();
                member.description = description;
                member.model = model;
                member.activity = "running".into();
                member.status = SubtaskStatus::Running;
                member
                    .started_at
                    .get_or_insert_with(std::time::Instant::now);
                run.total = run.total.max(run.members.len());
                self.visible = true;
            }
            TeamEventPayload::MemberActivity {
                member_id,
                activity,
                output_tokens,
            } => {
                let member = run.members.entry(member_id.to_string()).or_default();
                if member.label.is_empty() {
                    member.label = member_id.to_string();
                }
                member.activity = activity;
                // Estimates are monotonic; a reordered/late event can't lower the count.
                member.output_tokens = member.output_tokens.max(output_tokens);
                if member.status == SubtaskStatus::Pending {
                    member.status = SubtaskStatus::Running;
                    member.started_at = Some(std::time::Instant::now());
                }
                run.total = run.total.max(run.members.len());
            }
            TeamEventPayload::MemberFinished {
                member_id,
                success,
                stop,
                summary,
                output_tokens,
            } => {
                let member = run.members.entry(member_id.to_string()).or_default();
                if member.label.is_empty() {
                    member.label = member_id.to_string();
                }
                member.output_tokens = member.output_tokens.max(output_tokens);
                let was_stopped = stop == "stopped";
                member.activity = if summary.is_empty() { stop } else { summary };
                member.status = if success {
                    SubtaskStatus::Completed
                } else if was_stopped {
                    SubtaskStatus::Stopped
                } else {
                    SubtaskStatus::Failed
                };
                run.total = run.total.max(run.members.len());
            }
            TeamEventPayload::RunFinished { total, .. } => {
                run.total = total.max(run.members.len());
            }
        }
    }

    pub fn show(&mut self) {
        self.visible = true;
    }

    pub fn hide(&mut self) {
        self.visible = false;
    }

    pub fn clear(&mut self) {
        self.runs.clear();
        self.visible = false;
    }

    pub fn reset_generation(&mut self, generation: u64) {
        if self.generation != Some(generation) {
            self.runs.clear();
            self.generation = Some(generation);
        }
    }

    pub fn summary(&self) -> String {
        if self.runs.is_empty() {
            return "No Team runs.".into();
        }
        let members = self.runs.values().flat_map(|run| run.members.values());
        let (mut completed, mut running, mut failed, mut stopped) = (0, 0, 0, 0);
        for member in members {
            match member.status {
                SubtaskStatus::Completed => completed += 1,
                SubtaskStatus::Running => running += 1,
                SubtaskStatus::Stopped => stopped += 1,
                SubtaskStatus::Failed => failed += 1,
                SubtaskStatus::Pending => {}
            }
        }
        format!(
            "Team: {} run(s) · {completed} completed · {running} running · {failed} failed · {stopped} stopped",
            self.runs.len()
        )
    }

    pub fn panel(&self) -> Option<SubtaskProgress> {
        if !self.visible || self.runs.is_empty() {
            return None;
        }
        let mut items = Vec::new();
        let mut total = 0;
        for (run_id, run) in &self.runs {
            total += run.total.max(run.members.len());
            for member in run.members.values() {
                items.push(SubtaskItem {
                    label: if member.label.is_empty() {
                        run_id.clone()
                    } else {
                        member.label.clone()
                    },
                    description: member.description.clone(),
                    model: member.model.clone(),
                    activity: member.activity.clone(),
                    started_at: member.started_at,
                    output_tokens: member.output_tokens,
                    status: member.status,
                });
            }
        }
        let completed = items
            .iter()
            .filter(|item| item.status == SubtaskStatus::Completed)
            .count();
        Some(SubtaskProgress {
            call_id: "team:runtime".into(),
            completed,
            total: total.max(items.len()),
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::team::{TeamMemberId, TeamRoleId, TeamRunId};

    fn event(run: &str, seq: u64, payload: TeamEventPayload) -> TeamEvent {
        TeamEvent::new(TeamRunId::new(run), seq, payload)
    }

    #[test]
    fn reducer_handles_multiple_runs_out_of_order_and_generation_reset() {
        let mut state = TeamProjection::default();
        state.apply(
            1,
            event(
                "a",
                2,
                TeamEventPayload::MemberStarted {
                    member_id: TeamMemberId::new("a#1"),
                    role: TeamRoleId::Explorer,
                    model: "fast".into(),
                    description: "inspect".into(),
                },
            ),
        );
        state.apply(1, event("a", 1, TeamEventPayload::RunStarted { total: 1 }));
        state.apply(1, event("b", 1, TeamEventPayload::RunStarted { total: 2 }));
        let panel = state.panel().unwrap();
        assert_eq!(panel.items.len(), 1);
        assert_eq!(panel.total, 3);
        state.apply(
            2,
            event("new", 1, TeamEventPayload::RunStarted { total: 1 }),
        );
        assert_eq!(state.panel().unwrap().total, 1);
    }

    #[test]
    fn reducer_tracks_terminal_counts_and_visibility_controls() {
        let mut state = TeamProjection::default();
        state.apply(1, event("a", 1, TeamEventPayload::RunStarted { total: 1 }));
        state.apply(
            1,
            event(
                "a",
                2,
                TeamEventPayload::MemberFinished {
                    member_id: TeamMemberId::new("a#1"),
                    success: false,
                    stop: "failed".into(),
                    summary: "boom".into(),
                    output_tokens: 900,
                },
            ),
        );
        assert!(state.summary().contains("1 failed"));
        state.apply(1, event("b", 1, TeamEventPayload::RunStarted { total: 1 }));
        state.apply(
            1,
            event(
                "b",
                2,
                TeamEventPayload::MemberFinished {
                    member_id: TeamMemberId::new("b#1"),
                    success: false,
                    stop: "stopped".into(),
                    summary: "cancelled".into(),
                    output_tokens: 0,
                },
            ),
        );
        assert!(state.summary().contains("1 stopped"));
        assert_eq!(
            state.panel().unwrap().items.iter()
                .find(|item| item.label == "b#1").unwrap().status,
            SubtaskStatus::Stopped
        );
        // Real per-member token estimate flows through to the panel (was hardcoded 0).
        assert_eq!(
            state.panel().unwrap().items.iter()
                .find(|item| item.label == "a#1").unwrap().output_tokens,
            900
        );
        state.hide();
        assert!(state.panel().is_none());
        state.show();
        assert!(state.panel().is_some());
        state.clear();
        assert_eq!(state.summary(), "No Team runs.");
    }

    #[test]
    fn activity_promotes_pending_to_running_and_tracks_tokens() {
        let mut state = TeamProjection::default();
        // RunStarted 使面板可见；之后只有 MemberActivity、没有 MemberStarted：
        // Pending 应提升为 Running，token 估计应单调取 max（乱序/迟到事件不能把计数拉低）。
        state.apply(1, event("a", 1, TeamEventPayload::RunStarted { total: 1 }));
        state.apply(
            1,
            event(
                "a",
                2,
                TeamEventPayload::MemberActivity {
                    member_id: TeamMemberId::new("a#1"),
                    activity: "using read_file".into(),
                    output_tokens: 300,
                },
            ),
        );
        state.apply(
            1,
            event(
                "a",
                3,
                TeamEventPayload::MemberActivity {
                    member_id: TeamMemberId::new("a#1"),
                    activity: "using grep".into(),
                    output_tokens: 500,
                },
            ),
        );
        // 迟到的低 token 事件（seq 更大但 token 更小）不能拉低显示计数。
        state.apply(
            1,
            event(
                "a",
                4,
                TeamEventPayload::MemberActivity {
                    member_id: TeamMemberId::new("a#1"),
                    activity: "done".into(),
                    output_tokens: 100,
                },
            ),
        );
        let panel = state.panel().unwrap();
        let item = panel
            .items
            .iter()
            .find(|item| item.label == "a#1")
            .unwrap();
        assert_eq!(item.status, SubtaskStatus::Running);
        assert_eq!(item.activity, "done");
        assert_eq!(item.output_tokens, 500, "token 计数必须单调取 max");
        assert!(state.summary().contains("1 running"));
    }

    #[test]
    fn out_of_order_seq_events_are_dropped() {
        let mut state = TeamProjection::default();
        // RunStarted 使面板可见并建立 seq=1 基线。
        state.apply(1, event("a", 1, TeamEventPayload::RunStarted { total: 1 }));
        // 先到达高 seq 事件。
        state.apply(
            1,
            event(
                "a",
                5,
                TeamEventPayload::MemberFinished {
                    member_id: TeamMemberId::new("a#1"),
                    success: true,
                    stop: "completed".into(),
                    summary: "ok".into(),
                    output_tokens: 100,
                },
            ),
        );
        // 迟到的低 seq 事件（seq <= last_seq）必须被丢弃，不得覆盖终态。
        state.apply(
            1,
            event(
                "a",
                4,
                TeamEventPayload::MemberActivity {
                    member_id: TeamMemberId::new("a#1"),
                    activity: "late".into(),
                    output_tokens: 999,
                },
            ),
        );
        let panel = state.panel().unwrap();
        let item = panel
            .items
            .iter()
            .find(|item| item.label == "a#1")
            .unwrap();
        assert_eq!(item.status, SubtaskStatus::Completed);
        assert_eq!(item.activity, "ok");
        assert_eq!(item.output_tokens, 100);
        assert!(state.summary().contains("1 completed"));
    }

    #[test]
    fn reset_generation_clears_previous_runs() {
        let mut state = TeamProjection::default();
        state.apply(1, event("a", 1, TeamEventPayload::RunStarted { total: 1 }));
        state.apply(
            1,
            event(
                "a",
                2,
                TeamEventPayload::MemberFinished {
                    member_id: TeamMemberId::new("a#1"),
                    success: false,
                    stop: "failed".into(),
                    summary: "boom".into(),
                    output_tokens: 0,
                },
            ),
        );
        assert!(state.summary().contains("1 failed"));
        // 显式 reset 到新 generation：旧 run 应被清空。
        state.reset_generation(2);
        assert_eq!(state.summary(), "No Team runs.");
        // 新 generation 的 RunStarted 生效，旧 run 的失败计数不得残留。
        state.apply(2, event("b", 1, TeamEventPayload::RunStarted { total: 2 }));
        assert_eq!(state.panel().unwrap().total, 2);
        assert!(!state.summary().contains("1 failed"));
    }
}
