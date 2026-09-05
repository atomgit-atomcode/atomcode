//! The capability map, generated from the code that implements it.
//!
//! DeepSeek Harness keeps a curated `SERVICE_ROLES` table and cross-checks it
//! against the source, because TypeScript cannot carry the classification on the
//! interface itself. Rust can: [`SeamMode`] and the title live on the
//! [`ServiceKey`](atomcode_plexus::ServiceKey) impl, and the provider/consumer
//! columns come from what plugins declare. Nothing here is hand-maintained, so
//! nothing here can go stale.

use std::collections::BTreeMap;

use atomcode_plexus::{PluginRegistry, SeamMode, ServiceKey};

/// One row of the map: a slot, what it is, who fills it, who reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeamRow {
    pub name: &'static str,
    pub mode: SeamMode,
    pub title: &'static str,
    /// Plugins that declare they provide this slot.
    pub providers: Vec<&'static str>,
    /// Plugins that declare they inject it (hard dependency).
    pub consumers: Vec<&'static str>,
    /// Plugins that read it opportunistically.
    pub optional_consumers: Vec<&'static str>,
    /// True when the host itself reads this slot — the launcher, or a library
    /// caller like `run_turn`. Without this the map reports "consumed by
    /// nothing" for the one service the whole binary exists to call.
    pub host_consumer: bool,
}

impl SeamRow {
    /// Every reader of this slot, whatever the strength of the dependency.
    pub fn has_consumers(&self) -> bool {
        self.host_consumer || !self.consumers.is_empty() || !self.optional_consumers.is_empty()
    }

    /// A seam with one provider is a seam in name only — either a second
    /// implementation is missing, or the slot is really `Core`. Worth saying out
    /// loud in the map rather than discovering when someone tries to swap it.
    pub fn is_nominal_seam(&self) -> bool {
        self.mode == SeamMode::Seam && self.providers.len() < 2
    }
}

/// Every seam this build defines, with its classification.
///
/// The one hand-written list in the file, and it is a list of *types*: adding a
/// seam without adding it here fails the `every_seam_is_on_the_map` test,
/// because the audit sees a provided service the map does not know.
macro_rules! seam_catalog {
    ($($key:ty),+ $(,)?) => {
        pub fn seam_definitions() -> Vec<(&'static str, SeamMode, &'static str)> {
            vec![$((<$key as ServiceKey>::NAME, <$key as ServiceKey>::MODE, <$key as ServiceKey>::TITLE)),+]
        }
    };
}

use crate::seams::{
    AgentLoopSvc, AgentsSvc, ApprovalSvc, CodeIndexSvc, CompactionSvc, ControlSvc, FindingsSvc,
    FsSvc, LlmSvc, McpSvc, SessionPersistenceSvc, SessionProjectionsSvc, SessionSvc,
    SessionTitleSvc, ShellSvc, SkillsSvc, SubagentsSvc, SubprocessSvc, SystemPromptSvc, ToolsSvc,
    UiSvc, UserQuestionsSvc,
};

seam_catalog!(
    AgentsSvc,
    LlmSvc,
    ToolsSvc,
    SystemPromptSvc,
    SessionSvc,
    SessionProjectionsSvc,
    SessionPersistenceSvc,
    SessionTitleSvc,
    SkillsSvc,
    CodeIndexSvc,
    McpSvc,
    FsSvc,
    SubprocessSvc,
    ShellSvc,
    CompactionSvc,
    UserQuestionsSvc,
    FindingsSvc,
    SubagentsSvc,
    AgentLoopSvc,
    ApprovalSvc,
    UiSvc,
    ControlSvc,
);

/// Slots the host reads directly rather than through a plugin. `run_turn` calls
/// `agent-loop`; the launcher prints titles and audits the tree.
///
/// Public because the audit needs the same list: a slot the binary itself calls
/// is consumed, and reporting it as dead weight would bury the slots that
/// really are.
/// Slots the host fills itself. Only the launcher owns the `App`, so only it
/// can offer reconfiguration of the running tree.
pub const HOST_PROVIDED: &[&str] = &["control"];

pub const HOST_CONSUMED: &[&str] = &[
    // The launcher resolves `ui` and hands over; `run_turn` and friends reach
    // for the rest.
    "ui",
    // A review's product is its findings, and the thing that reads them is
    // whoever launched the run — a CLI, CI, an eval.
    "findings",
    "agent-loop",
    "agents",
    "sessions",
    "session-title",
];

/// Build the map by joining the seam definitions with what the registry's
/// plugins declare about them.
pub fn seam_map(registry: &PluginRegistry) -> Vec<SeamRow> {
    let mut providers: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    let mut consumers: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    let mut optional: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for name in registry.names() {
        let Some(plugin) = registry.get(name) else {
            continue;
        };
        for service in plugin.provides() {
            providers.entry(service).or_default().push(name);
        }
        for service in plugin.inject() {
            consumers.entry(service).or_default().push(name);
        }
        for service in plugin.uses() {
            optional.entry(service).or_default().push(name);
        }
    }

    seam_definitions()
        .into_iter()
        .map(|(name, mode, title)| SeamRow {
            name,
            mode,
            title,
            providers: providers.get(name).cloned().unwrap_or_default(),
            consumers: consumers.get(name).cloned().unwrap_or_default(),
            optional_consumers: optional.get(name).cloned().unwrap_or_default(),
            host_consumer: HOST_CONSUMED.contains(&name),
        })
        .collect()
}

/// Service names some plugin declares but no seam definition covers.
///
/// These are the ones that would be missing from the map — the failure the
/// generated-doc approach exists to prevent.
pub fn undeclared_services(registry: &PluginRegistry) -> Vec<&'static str> {
    let known: Vec<&'static str> = seam_definitions().into_iter().map(|(n, _, _)| n).collect();
    let mut missing = Vec::new();
    for name in registry.names() {
        let Some(plugin) = registry.get(name) else {
            continue;
        };
        for service in plugin.provides().iter().chain(plugin.inject()) {
            if !known.contains(service) && !missing.contains(service) {
                missing.push(*service);
            }
        }
    }
    missing.sort_unstable();
    missing
}

/// Render the map as a table, grouped by classification.
pub fn render_table(rows: &[SeamRow]) -> String {
    let mut out = String::new();
    for (mode, heading) in [
        (SeamMode::Seam, "Seams — replaceable capabilities"),
        (SeamMode::Core, "Core — the spine"),
        (SeamMode::Bundle, "Bundles — composition points"),
    ] {
        let group: Vec<&SeamRow> = rows.iter().filter(|r| r.mode == mode).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("\n{heading}\n"));
        for row in group {
            out.push_str(&format!("  {}  — {}\n", row.name, row.title));
            out.push_str(&format!(
                "    provided by: {}\n",
                if row.providers.is_empty() {
                    "(nothing)".to_string()
                } else {
                    row.providers.join(", ")
                }
            ));
            let mut readers: Vec<String> = row.consumers.iter().map(|c| c.to_string()).collect();
            readers.extend(
                row.optional_consumers
                    .iter()
                    .map(|c| format!("{c} (optional)")),
            );
            if row.host_consumer {
                readers.push("the host".to_string());
            }
            out.push_str(&format!(
                "    consumed by: {}\n",
                if readers.is_empty() {
                    "(nothing)".to_string()
                } else {
                    readers.join(", ")
                }
            ));
            if row.is_nominal_seam() {
                out.push_str("    note: declared a seam but has fewer than two providers\n");
            }
        }
    }
    out
}

/// Render the map as a mermaid graph, the same shape DeepSeek Harness generates.
pub fn render_mermaid(rows: &[SeamRow]) -> String {
    fn id(prefix: &str, name: &str) -> String {
        format!("{prefix}_{}", name.replace(['-', '.'], "_"))
    }

    let mut out = String::from("flowchart LR\n");
    for row in rows {
        out.push_str(&format!(
            "  {}[\"{}<br/>{}\"]\n",
            id("svc", row.name),
            row.name,
            row.title
        ));
        for provider in &row.providers {
            out.push_str(&format!("  {}[\"{provider}\"]\n", id("pkg", provider)));
            out.push_str(&format!(
                "  {} -->|provides| {}\n",
                id("pkg", provider),
                id("svc", row.name)
            ));
        }
        for consumer in &row.consumers {
            out.push_str(&format!("  {}[\"{consumer}\"]\n", id("pkg", consumer)));
            out.push_str(&format!(
                "  {} -->|injects| {}\n",
                id("svc", row.name),
                id("pkg", consumer)
            ));
        }
        for consumer in &row.optional_consumers {
            out.push_str(&format!("  {}[\"{consumer}\"]\n", id("pkg", consumer)));
            out.push_str(&format!(
                "  {} -.->|uses| {}\n",
                id("svc", row.name),
                id("pkg", consumer)
            ));
        }
        if row.host_consumer {
            out.push_str(&format!(
                "  {} -->|host| host[\"the host\"]\n",
                id("svc", row.name)
            ));
        }
    }
    out
}
