//! `team-progress`: delegated agents' work, as the `Team` events a front end
//! built for the product's own delegation reads.
//!
//! The product delegates through the harness's `task` and `team` rows now
//! (`docs/adr/0023` §2): a child is an agent in the tree with its own log. The
//! terminal front end that ships today draws a team panel from
//! `CodingRuntimeEvent::Team`, which the product's own delegation used to emit.
//! This row folds each delegated agent's committed facts into those events so
//! the panel keeps working while that front end does. Transitional: it goes
//! when the front end does (plan M6).
//!
//! One run per delegated turn: the turn's first prompt queues and starts the
//! member, each tool it starts is activity, its usage counts tokens, and the
//! turn's end finishes the member and the run.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_capabilities::team::{
    TeamEvent, TeamEventPayload, TeamMemberId, TeamRoleId, TeamRunId,
};
use atomcode_harness::events::SessionEventCommitted;
use atomcode_harness::seams::AgentsSvc;
use atomcode_harness::session::{Committed, InjectionOrigin, SessionEvent};
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

/// Where the events go.
pub(crate) type TeamSink = Arc<dyn Fn(TeamEvent) + Send + Sync>;

struct Run {
    id: TeamRunId,
    seq: u64,
    member: TeamMemberId,
    started: bool,
    tool_uses: u64,
    output_tokens: u64,
    last_said: String,
}

impl Run {
    fn next(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

pub(crate) struct TeamProgressPlugin(pub(crate) TeamSink);

#[async_trait]
impl Plugin for TeamProgressPlugin {
    fn name(&self) -> &'static str {
        "team-progress"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["agents"]
    }
    fn description(&self) -> &'static str {
        "each delegated agent's turn as the Team events the shipped team panel draws"
    }

    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let sink = self.0.clone();
        let runs: Arc<Mutex<HashMap<String, Run>>> = Arc::default();
        let agents_ctx = ctx.clone();
        let _ = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            let Some(agent) = agents_ctx
                .service::<AgentsSvc>()
                .and_then(|agents| agents.by_session(&committed.session))
            else {
                return;
            };
            if agent.parent().is_none() {
                return;
            }
            let mut runs = runs.lock().expect("team runs poisoned");
            let emit = |run: &mut Run, payload| {
                let seq = run.next();
                sink(TeamEvent::new(run.id.clone(), seq, payload));
            };
            match &committed.event {
                SessionEvent::TurnStart { turn } => {
                    let description = agent.describe();
                    let member = description
                        .member
                        .as_ref()
                        .map(|m| m.name.clone())
                        .unwrap_or_else(|| committed.session.clone());
                    runs.insert(
                        committed.session.clone(),
                        Run {
                            id: TeamRunId::new(format!("delegated:{}:{turn}", committed.session)),
                            seq: 0,
                            member: TeamMemberId::new(member),
                            started: false,
                            tool_uses: 0,
                            output_tokens: 0,
                            last_said: String::new(),
                        },
                    );
                }
                SessionEvent::UserMessage { text, .. }
                | SessionEvent::Injected {
                    text,
                    origin: InjectionOrigin::Peer { .. },
                    ..
                } => {
                    let Some(run) = runs.get_mut(&committed.session) else {
                        return;
                    };
                    if run.started {
                        return;
                    }
                    run.started = true;
                    let description = agent.describe();
                    let role = description
                        .member
                        .as_ref()
                        .and_then(|m| TeamRoleId::from_str(&m.role).ok())
                        .unwrap_or(TeamRoleId::Explorer);
                    let model = description.model.clone().unwrap_or_default();
                    let task = text.lines().next().unwrap_or_default().to_string();
                    emit(run, TeamEventPayload::RunStarted { total: 1 });
                    emit(
                        run,
                        TeamEventPayload::MemberQueued {
                            member_id: run.member.clone(),
                            role,
                            model: model.clone(),
                            description: task.clone(),
                        },
                    );
                    emit(
                        run,
                        TeamEventPayload::MemberStarted {
                            member_id: run.member.clone(),
                            role,
                            model,
                            description: task,
                        },
                    );
                }
                SessionEvent::ToolStarted { call, .. } => {
                    let Some(run) = runs.get_mut(&committed.session).filter(|r| r.started) else {
                        return;
                    };
                    run.tool_uses += 1;
                    let payload = TeamEventPayload::MemberActivity {
                        member_id: run.member.clone(),
                        activity: call.name.clone(),
                        output_tokens: run.output_tokens,
                        tool_uses: run.tool_uses,
                    };
                    emit(run, payload);
                }
                SessionEvent::Usage { usage, .. } => {
                    if let Some(run) = runs.get_mut(&committed.session) {
                        run.output_tokens += u64::from(usage.completion);
                    }
                }
                SessionEvent::AssistantMessage { text, .. } if !text.trim().is_empty() => {
                    if let Some(run) = runs.get_mut(&committed.session) {
                        run.last_said = text.clone();
                    }
                }
                SessionEvent::TurnEnd { stop, error, .. } => {
                    let Some(mut run) = runs.remove(&committed.session).filter(|r| r.started)
                    else {
                        return;
                    };
                    let success =
                        *stop == atomcode_kernel::event::StopReason::Stopped && error.is_none();
                    let summary = error.clone().unwrap_or_else(|| {
                        run.last_said.lines().next().unwrap_or_default().to_string()
                    });
                    let payload = TeamEventPayload::MemberFinished {
                        member_id: run.member.clone(),
                        success,
                        stop: format!("{stop:?}").to_lowercase(),
                        summary,
                        output_tokens: run.output_tokens,
                    };
                    emit(&mut run, payload);
                    emit(
                        &mut run,
                        TeamEventPayload::RunFinished {
                            total: 1,
                            completed: usize::from(success),
                            failed: usize::from(!success),
                        },
                    );
                }
                _ => {}
            }
        });
        Ok(())
    }
}
