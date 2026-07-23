/// Runtime approval mode shared by `/live`, `/chat`, WebUI, and IDE clients.
pub use atomcode_coding::RuntimeMode as ApprovalMode;

// Wire strings come from the native runtime mode.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_mode_wire_strings_are_lowercase() {
        for (mode, wire) in [
            (ApprovalMode::Build, "build"),
            (ApprovalMode::AcceptEdits, "accept_edits"),
            (ApprovalMode::Plan, "plan"),
            (ApprovalMode::Auto, "bypass"),
        ] {
            assert_eq!(serde_json::to_string(&mode).unwrap(), format!("\"{wire}\""));
            let back: ApprovalMode =
                serde_json::from_str(&format!("\"{wire}\"")).expect("deserialize mode");
            assert_eq!(back, mode);
            assert_eq!(mode.wire(), wire);
        }

        assert_eq!(ApprovalMode::default(), ApprovalMode::Build);
    }
}
