use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// Separates structured Team progress from ordinary tool progress text.
pub const TEAM_EVENT_MARKER: char = '\u{1f}';

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamRoleId {
    Planner,
    Architect,
    Explorer,
    Implementer,
    Rust,
    TuiUx,
    Reviewer,
    Tester,
    Debugger,
    Security,
    Performance,
    DocsWriter,
    ReleaseManager,
    MigrationCompat,
}

impl TeamRoleId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Architect => "architect",
            Self::Explorer => "explorer",
            Self::Implementer => "implementer",
            Self::Rust => "rust",
            Self::TuiUx => "tui_ux",
            Self::Reviewer => "reviewer",
            Self::Tester => "tester",
            Self::Debugger => "debugger",
            Self::Security => "security",
            Self::Performance => "performance",
            Self::DocsWriter => "docs_writer",
            Self::ReleaseManager => "release_manager",
            Self::MigrationCompat => "migration_compat",
        }
    }
}

impl fmt::Display for TeamRoleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TeamRoleId {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "planner" => Ok(Self::Planner),
            "architect" => Ok(Self::Architect),
            "explorer" => Ok(Self::Explorer),
            "implementer" => Ok(Self::Implementer),
            "rust" => Ok(Self::Rust),
            "tui_ux" => Ok(Self::TuiUx),
            "reviewer" => Ok(Self::Reviewer),
            "tester" => Ok(Self::Tester),
            "debugger" => Ok(Self::Debugger),
            "security" => Ok(Self::Security),
            "performance" => Ok(Self::Performance),
            "docs_writer" => Ok(Self::DocsWriter),
            "release_manager" => Ok(Self::ReleaseManager),
            "migration_compat" => Ok(Self::MigrationCompat),
            other => Err(format!("unknown team role: {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamPermission {
    Explore,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamDifficulty {
    Simple,
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeamRoleProfile {
    pub id: TeamRoleId,
    pub display_name: &'static str,
    pub permission: TeamPermission,
    pub difficulty: TeamDifficulty,
    pub persona: &'static str,
    pub when_to_use: &'static str,
}

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = std::convert::Infallible;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Ok(Self::new(value))
            }
        }
    };
}

string_id!(TeamRunId);
string_id!(TeamMemberId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTaskSpec {
    pub description: String,
    pub prompt: String,
    pub role: TeamRoleId,
    pub permission: TeamPermission,
    pub difficulty: TeamDifficulty,
    #[serde(default)]
    pub scope: Vec<String>,
}

/// Reject worker scope conflicts that can be proven without attempting general glob
/// intersection: identical patterns, workspace-wide `**`, or a recursive `<dir>/**`
/// lane containing another worker's literal/recursive lane. Complex glob intersections
/// are intentionally left to commit-time file locking rather than guessed here.
pub fn validate_non_overlapping_worker_scopes(tasks: &[TeamTaskSpec]) -> Result<(), String> {
    let workers: Vec<(usize, Vec<String>)> = tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| task.permission == TeamPermission::Worker)
        .map(|(index, task)| {
            (
                index,
                task.scope
                    .iter()
                    .map(|scope| normalize_scope(scope))
                    .filter(|scope| !scope.is_empty())
                    .collect(),
            )
        })
        .collect();

    for left in 0..workers.len() {
        for right in left + 1..workers.len() {
            for a in &workers[left].1 {
                for b in &workers[right].1 {
                    if scopes_provably_overlap(a, b) {
                        return Err(format!(
                            "team worker scopes overlap: task {} scope {:?} conflicts with task {} \
                             scope {:?}. Assign non-overlapping files/directories before dispatch.",
                            workers[left].0 + 1,
                            a,
                            workers[right].0 + 1,
                            b
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn normalize_scope(scope: &str) -> String {
    scope
        .trim()
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn recursive_scope_root(scope: &str) -> Option<&str> {
    if scope == "**" {
        Some("")
    } else {
        scope.strip_suffix("/**").map(|root| root.trim_end_matches('/'))
    }
}

fn has_glob_meta(scope: &str) -> bool {
    scope.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']' | '{' | '}'))
}

fn path_is_under(path: &str, root: &str) -> bool {
    root.is_empty()
        || path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn scopes_provably_overlap(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    if let Some(root) = recursive_scope_root(a) {
        if let Some(other_root) = recursive_scope_root(b) {
            return path_is_under(other_root, root) || path_is_under(root, other_root);
        }
        return !has_glob_meta(b) && path_is_under(b, root);
    }
    if let Some(root) = recursive_scope_root(b) {
        return !has_glob_meta(a) && path_is_under(a, root);
    }
    false
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TeamEventPayload {
    RunStarted {
        total: usize,
    },
    MemberQueued {
        member_id: TeamMemberId,
        role: TeamRoleId,
        model: String,
        description: String,
    },
    MemberStarted {
        member_id: TeamMemberId,
        role: TeamRoleId,
        model: String,
        description: String,
    },
    MemberActivity {
        member_id: TeamMemberId,
        activity: String,
        /// Estimated output tokens so far (chars/4), matching the legacy `task`
        /// subagent panel so both render a live token count consistently.
        #[serde(default)]
        output_tokens: u64,
    },
    MemberFinished {
        member_id: TeamMemberId,
        success: bool,
        stop: String,
        summary: String,
        #[serde(default)]
        output_tokens: u64,
    },
    RunFinished {
        total: usize,
        completed: usize,
        failed: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamEvent {
    pub run_id: TeamRunId,
    pub seq: u64,
    pub payload: TeamEventPayload,
}

impl TeamEvent {
    pub fn new(run_id: TeamRunId, seq: u64, payload: TeamEventPayload) -> Self {
        Self {
            run_id,
            seq,
            payload,
        }
    }
}

const fn role(
    id: TeamRoleId,
    display_name: &'static str,
    permission: TeamPermission,
    difficulty: TeamDifficulty,
    persona: &'static str,
    when_to_use: &'static str,
) -> TeamRoleProfile {
    TeamRoleProfile {
        id,
        display_name,
        permission,
        difficulty,
        persona,
        when_to_use,
    }
}

const BUILT_IN_ROLES: [TeamRoleProfile; 14] = [
    role(
        TeamRoleId::Planner,
        "Planner",
        TeamPermission::Explore,
        TeamDifficulty::Hard,
        "Decomposes work and plans delegation.",
        "Use for task decomposition and delegation plans.",
    ),
    role(
        TeamRoleId::Architect,
        "Architect",
        TeamPermission::Explore,
        TeamDifficulty::Hard,
        "Maps runtime ownership and crate boundaries.",
        "Use for runtime ownership, boundaries, protocol, and persistence impact.",
    ),
    role(
        TeamRoleId::Explorer,
        "Explorer",
        TeamPermission::Explore,
        TeamDifficulty::Simple,
        "Discovers code paths and call chains.",
        "Use for code search and call-chain discovery.",
    ),
    role(
        TeamRoleId::Implementer,
        "Implementer",
        TeamPermission::Worker,
        TeamDifficulty::Hard,
        "Makes scoped implementation edits.",
        "Use for focused file edits.",
    ),
    role(
        TeamRoleId::Rust,
        "Rust Expert",
        TeamPermission::Worker,
        TeamDifficulty::Hard,
        "Handles Rust async, traits, errors, and tests.",
        "Use for Rust async, trait, error-handling, and test work.",
    ),
    role(
        TeamRoleId::TuiUx,
        "TUI/UX",
        TeamPermission::Worker,
        TeamDifficulty::Hard,
        "Builds terminal UI state, layout, and interaction.",
        "Use for terminal UI state, layout, width, and interaction.",
    ),
    role(
        TeamRoleId::Reviewer,
        "Reviewer",
        TeamPermission::Explore,
        TeamDifficulty::Hard,
        "Reviews changes with a bug-focused lens.",
        "Use for bug-focused code review.",
    ),
    role(
        TeamRoleId::Tester,
        "Tester",
        TeamPermission::Worker,
        TeamDifficulty::Hard,
        "Creates tests and verifies commands.",
        "Use for tests, fixtures, and command verification.",
    ),
    role(
        TeamRoleId::Debugger,
        "Debugger",
        TeamPermission::Explore,
        TeamDifficulty::Hard,
        "Reproduces failures and isolates root causes.",
        "Use for failure reproduction and root-cause isolation.",
    ),
    role(
        TeamRoleId::Security,
        "Security",
        TeamPermission::Explore,
        TeamDifficulty::Hard,
        "Assesses approvals, secrets, scope, and execution risk.",
        "Use for approval, secrets, path scope, and auto-execution risk.",
    ),
    role(
        TeamRoleId::Performance,
        "Performance",
        TeamPermission::Explore,
        TeamDifficulty::Hard,
        "Analyzes concurrency, tokens, rendering, latency, and memory.",
        "Use for concurrency, token, rendering, latency, and memory concerns.",
    ),
    role(
        TeamRoleId::DocsWriter,
        "Docs Writer",
        TeamPermission::Worker,
        TeamDifficulty::Simple,
        "Writes user-facing documentation and evaluation instructions.",
        "Use for user-facing docs and eval instructions.",
    ),
    role(
        TeamRoleId::ReleaseManager,
        "Release Manager",
        TeamPermission::Explore,
        TeamDifficulty::Simple,
        "Checks final validation and branch hygiene.",
        "Use for the final validation matrix and branch hygiene.",
    ),
    role(
        TeamRoleId::MigrationCompat,
        "Migration Compatibility",
        TeamPermission::Explore,
        TeamDifficulty::Hard,
        "Reviews legacy, importer, and wire compatibility.",
        "Use for legacy, importer, and wire compatibility review.",
    ),
];

pub fn built_in_roles() -> &'static [TeamRoleProfile] {
    &BUILT_IN_ROLES
}

pub fn role_by_id(id: &str) -> Option<&'static TeamRoleProfile> {
    built_in_roles().iter().find(|role| role.id.as_str() == id)
}

pub fn encode_team_event(event: &TeamEvent) -> Result<String, serde_json::Error> {
    serde_json::to_string(event).map(|json| format!("{TEAM_EVENT_MARKER}{json}"))
}

pub fn decode_team_event(value: &str) -> Option<TeamEvent> {
    value
        .strip_prefix(TEAM_EVENT_MARKER)
        .and_then(|json| serde_json::from_str(json).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_parse_and_expose_stable_policy() {
        assert_eq!("tui_ux".parse(), Ok(TeamRoleId::TuiUx));
        assert!("unknown".parse::<TeamRoleId>().is_err());
        assert_eq!(
            role_by_id("implementer").unwrap().permission,
            TeamPermission::Worker
        );
        assert_eq!(
            role_by_id("explorer").unwrap().difficulty,
            TeamDifficulty::Simple
        );
        assert_eq!(built_in_roles().len(), 14);
    }

    #[test]
    fn identifiers_have_stable_transparent_json() {
        let run = TeamRunId::new("run-17");
        let member = TeamMemberId::new("explorer#2");
        assert_eq!(serde_json::to_string(&run).unwrap(), "\"run-17\"");
        assert_eq!(
            serde_json::from_str::<TeamMemberId>("\"explorer#2\"").unwrap(),
            member
        );
        assert_eq!(run.as_str(), "run-17");
    }

    #[test]
    fn event_marker_json_round_trips_and_rejects_malformed_input() {
        let event = TeamEvent::new(
            TeamRunId::new("run-1"),
            7,
            TeamEventPayload::MemberActivity {
                member_id: TeamMemberId::new("architect#1"),
                activity: "inspect runtime ownership".into(),
                output_tokens: 128,
            },
        );
        let encoded = encode_team_event(&event).unwrap();
        assert_eq!(decode_team_event(&encoded), Some(event));
        assert_eq!(decode_team_event("ordinary tool progress"), None);
        assert_eq!(decode_team_event("\u{1f}{not-json}"), None);
        assert_eq!(decode_team_event("\u{1f}{\"run_id\":\"x\"}"), None);
    }
}
