/// Runtime approval mode shared by `/live`, `/chat`, WebUI, and IDE clients.
/// Unified onto the shared `atomcode_core::agent::Mode` (wire: build/plan/bypass,
/// where the old `Bypass` is `Mode::Auto`).
pub use atomcode_core::agent::Mode as ApprovalMode;

// Wire strings come from `ApprovalMode::wire()` (core), which the core
// `mode_wire_matches_serde` test locks against the serde rename.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_mode_wire_strings_are_lowercase() {
        for (mode, wire) in [
            (ApprovalMode::Build, "build"),
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
