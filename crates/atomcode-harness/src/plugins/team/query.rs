//! A role selects a published MCP query. The connected server's user/driver
//! configuration authorizes it, including after a reconnect or session restore.
use super::{fail, Agent, Arc, Tool, ToolBox, ToolContext, ToolResult, Value};
use async_trait::async_trait;
use atomcode_capabilities::mcp::{McpRegistry, McpToolAdapter};
use atomcode_capabilities::tools::ArtifactStore;
use serde_json::json;

pub(super) fn is_query_name(name: &str) -> bool {
    name.strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
        .is_some_and(|(server, tool)| !server.is_empty() && !tool.is_empty())
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// A busy lead can resume before claiming a queued note. Its member still
/// logged the authorized tool's result. Never accept a pointer printed by
/// another tool or by the member's own prose.
async fn member_logged_reference(lead: &Agent, member: &str, id: &str) -> bool {
    use crate::session::SessionEvent;
    let Some(store) = lead.ctx().service::<crate::seams::SessionPersistenceSvc>() else {
        return false;
    };
    let Ok(children) = store.children(lead.session_id()).await else {
        return false;
    };
    let Some(child) = children
        .iter()
        .find(|h| h.member.as_ref().is_some_and(|m| m.name == member))
    else {
        return false;
    };
    let Ok(events) = store.load(&child.id).await else {
        return false;
    };
    let marker = format!("Team evidence {id}\n");
    events.iter().enumerate().rev().any(|(index, entry)| {
        let SessionEvent::ToolResultLogged { turn, round, call_id, content, .. } = &entry.event else { return false };
        content.starts_with(&marker) && events[..index].iter().rev().any(|e| matches!(&e.event,
            SessionEvent::ToolStarted {turn: t, round: r, call} if t == turn && r == round && &call.id == call_id && is_query_name(&call.name)))
    })
}

pub(super) async fn read_evidence(
    lead: &Agent,
    member: Option<&str>,
    id: Option<&str>,
) -> Result<String, String> {
    use crate::session::{InjectionOrigin, SessionEvent};
    let member = member
        .filter(|m| !m.is_empty())
        .ok_or("evidence requires the member name")?;
    let id = id.ok_or("evidence requires the query reference")?;
    // The artifact directory is project-local. A file placed there is not
    // proof of a query: the runtime must have issued this reference to this lead.
    let prefix = format!("Team evidence {id} from {member} (");
    if !lead.session().contains_event(|event| {
        matches!(event, SessionEvent::Injected {
        origin: InjectionOrigin::TeamNote { member: sender }, text, ..
    } if sender == member && text.starts_with(&prefix))
    }) && !member_logged_reference(lead, member, id).await
    {
        return Err("The team has no recorded query with that member and reference".into());
    }
    let store = lead
        .ctx()
        .require::<crate::seams::OutputArtifactsSvc>()
        .map_err(|e| e.to_string())?
        .team_evidence(lead.session_id(), member);
    let id = id.to_owned();
    let bytes = tokio::task::spawn_blocking(move || store.get_input(&id))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("Evidence unavailable for this member: {e}"))?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

pub(super) struct MemberQuery {
    pub lead: std::sync::Weak<Agent>,
    pub name: String,
    pub description: String,
    pub role: String,
    pub allowed_servers: Option<Vec<String>>,
    pub member: String,
    pub registry: Arc<McpRegistry>,
    pub catalog: std::sync::Weak<ToolBox>,
    pub evidence: Arc<ArtifactStore>,
}

impl MemberQuery {
    fn adapter(&self) -> Option<McpToolAdapter> {
        self.catalog.upgrade()?.get(&self.name)?;
        if let Some(servers) = &self.allowed_servers {
            let info = self.registry.team_query_info(&self.name, &self.role)?;
            if !servers.contains(&info.server_name) {
                return None;
            }
        }
        McpToolAdapter::for_team(self.registry.clone(), &self.name, &self.role)
    }
}

#[async_trait]
impl Tool for MemberQuery {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> Value {
        self.adapter()
            .map(|tool| tool.parameters_schema())
            .unwrap_or_else(|| json!({"type":"object"}))
    }
    fn read_only_hint(&self) -> bool {
        true
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let Some(adapter) = self.adapter() else {
            return fail(format!(
                "{} is unavailable or no longer authorized for role {}",
                self.name, self.role
            ));
        };
        let mut result = adapter.execute(args, ctx).await;
        if ctx.cancel.is_cancelled() {
            return result;
        }
        if !result.images.is_empty() || result.content.len() + args.len() > 4 * 1024 * 1024 {
            return fail("Team query returned an image or exceeded the 4 MiB evidence limit. Narrow the text query.");
        }
        let record = json!({
            "member": self.member, "role": self.role, "tool": self.name,
            "call_id": ctx.progress.source_id(), "arguments": serde_json::from_str::<Value>(args).unwrap_or(Value::Null),
            "is_error": result.is_error, "content": result.content
        }).to_string();
        let store = self.evidence.clone();
        let saved = tokio::task::spawn_blocking(move || store.put_input(record.as_bytes())).await;
        match saved {
            Ok(Ok(id)) => {
                if let Some(lead) = self.lead.upgrade() {
                    lead.note(
                        format!("Team evidence {id} from {} ({})", self.member, self.name),
                        crate::session::InjectionOrigin::TeamNote {
                            member: self.member.clone(),
                        },
                    );
                }
                result.content = format!("Team evidence {id}\n{}", result.content);
            }
            Ok(Err(e)) => {
                return fail(format!(
                    "Team query finished but its evidence could not be saved: {e}"
                ))
            }
            Err(e) => return fail(format!("Team evidence writer failed: {e}")),
        }
        result
    }
}
