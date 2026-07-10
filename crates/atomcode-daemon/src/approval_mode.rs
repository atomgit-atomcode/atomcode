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

pub(crate) fn approval_mode_wire(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Build => "build",
        ApprovalMode::Plan => "plan",
        ApprovalMode::Bypass => "bypass",
    }
}

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
}
