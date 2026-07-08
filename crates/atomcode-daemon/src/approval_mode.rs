/// Runtime approval mode shared by `/live`, `/chat`, WebUI, and IDE clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Ask before mutating operations when the client has an approval channel.
    #[default]
    Build,
    /// Read-only exploration and planning.
    Plan,
    /// Auto-approve all tool calls for this turn/session state.
    Bypass,
}

const READ_ONLY_TOOL_FILTER: &[&str] = &[
    "read_file",
    "grep",
    "glob",
    "list_directory",
    "web_search",
    "web_fetch",
    "trace_callees",
    "trace_callers",
    "trace_chain",
    "file_dependencies",
    "blast_radius",
];

pub(crate) fn approval_mode_tool_filter(mode: ApprovalMode) -> Option<&'static [&'static str]> {
    match mode {
        ApprovalMode::Plan => Some(READ_ONLY_TOOL_FILTER),
        ApprovalMode::Build | ApprovalMode::Bypass => None,
    }
}

pub(crate) fn approval_mode_wire(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Build => "build",
        ApprovalMode::Plan => "plan",
        ApprovalMode::Bypass => "bypass",
    }
}

/// Plan mode suffix appended to the per-turn system prompt. It is never
/// persisted and is shared by `/live` and `/chat` so all clients get identical
/// read-only guidance.
pub(crate) const PLAN_MODE_SYSTEM_SUFFIX: &str = "\n\n# Plan mode (read-only)\n\
You are in PLAN MODE. Explore the codebase read-only and PRESENT A PLAN for the \
user to approve. Do NOT edit files, do NOT run mutating shell commands — any such \
tool call will be denied. Use read/search tools freely, then summarize the concrete \
steps you would take. 你正处于「Plan 只读模式」：只读探索并给出方案，不要改文件、不要跑\
改动类命令（会被拒绝），最后给出你打算执行的具体步骤。";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_mode_wire_strings_are_lowercase() {
        for (mode, wire) in [
            (ApprovalMode::Build, "build"),
            (ApprovalMode::Plan, "plan"),
            (ApprovalMode::Bypass, "bypass"),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), format!("\"{wire}\""));
            let back: ApprovalMode =
                serde_json::from_str(&format!("\"{wire}\"")).expect("deserialize mode");
            assert_eq!(back, mode);
            assert_eq!(approval_mode_wire(mode), wire);
        }

        assert_eq!(ApprovalMode::default(), ApprovalMode::Build);
    }

    #[test]
    fn plan_mode_uses_the_read_only_tool_filter() {
        let filter =
            approval_mode_tool_filter(ApprovalMode::Plan).expect("plan mode should restrict tools");

        assert!(filter.contains(&"read_file"));
        assert!(filter.contains(&"grep"));
        assert!(filter.contains(&"list_directory"));
        assert!(!filter.contains(&"write_file"));
        assert!(!filter.contains(&"edit_file"));
        assert!(!filter.contains(&"bash"));

        assert!(approval_mode_tool_filter(ApprovalMode::Build).is_none());
        assert!(approval_mode_tool_filter(ApprovalMode::Bypass).is_none());
    }
}
