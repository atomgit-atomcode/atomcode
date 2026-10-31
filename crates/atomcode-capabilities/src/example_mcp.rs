//! A worked MCP server that ships inside the binary.
//!
//! `/mcp/status` used to be `{"servers":[],"trusted":false}` on a fresh install, and
//! the only explanation of MCP was prose in a skill — not the same as a server the
//! agent can actually call. This seeds the smallest real one: a stdlib-only Python
//! stdio server plus its `mcp.json` entry, so the wiring is visible end to end.
//!
//! Two deliberate limits:
//!
//! * The script is always written (it is our own file). The config entry is written
//!   ONLY when no `mcp.json` exists yet: adding a server to a file somebody already
//!   maintains — and whose comments a rewrite would delete — is not ours to do.
//! * The entry is only written when the runtime it names is present. A phone build
//!   without the bundled `python3` would otherwise get a server that can never start
//!   and a `Failed` row nobody can explain.

use std::path::{Path, PathBuf};

/// The server script, as it is written into the product tree.
pub const SERVER_SCRIPT: &str = include_str!("../assets/setup-seeds/mcp/example_server.py");

/// Server name in `mcp.json`; also the tool-name prefix (`mcp__example-tools__now`).
pub const SERVER_KEY: &str = "example-tools";

/// Where the script goes, relative to the product tree.
pub const SCRIPT_RELATIVE: &str = "mcp/example_server.py";

/// What a seeding pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SeededExampleMcp {
    /// The script path, when it was written this time.
    pub script: Option<PathBuf>,
    /// Whether an `mcp.json` entry was added.
    pub config_written: bool,
    /// Why the config entry was not written, when it was not.
    pub config_skipped: Option<String>,
}

/// Install the example server under `user_dir` (the product tree).
///
/// `runtime_available` says whether `python3` — the `command` the entry names — is
/// actually executable on this device. The host answers it from its own `PATH` setup
/// rather than guessing here.
pub fn seed_missing(user_dir: &Path, runtime_available: bool) -> SeededExampleMcp {
    let mut seeded = SeededExampleMcp::default();
    let script = user_dir.join(SCRIPT_RELATIVE);
    if !script.exists() {
        match script.parent().map(std::fs::create_dir_all) {
            Some(Err(error)) => {
                tracing::warn!(
                    dir = %script.parent().unwrap_or(user_dir).display(),
                    %error,
                    "cannot create the example MCP directory"
                );
                return seeded;
            }
            _ => {}
        }
        match std::fs::write(&script, SERVER_SCRIPT) {
            Ok(()) => seeded.script = Some(script.clone()),
            Err(error) => {
                tracing::warn!(path = %script.display(), %error, "cannot write the example MCP server");
                return seeded;
            }
        }
    }

    let config = user_dir.join("mcp.json");
    if config.exists() {
        // Never touch a config the person maintains: a rewrite would drop its
        // comments, and silently adding a server is a decision that is theirs.
        seeded.config_skipped = Some(format!("{} already exists", config.display()));
        return seeded;
    }
    if !runtime_available {
        seeded.config_skipped = Some("python3 is not available in this build".to_string());
        return seeded;
    }

    let args = vec![script.to_string_lossy().to_string()];
    match crate::mcp::config::merge_stdio_mcp_server_into_json_file(
        &config, SERVER_KEY, "python3", &args,
    ) {
        Ok(()) => seeded.config_written = true,
        Err(error) => {
            tracing::warn!(path = %config.display(), %error, "cannot write the example MCP config");
            seeded.config_skipped = Some(error.to_string());
        }
    }
    seeded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_is_a_standalone_stdio_server() {
        // Protocol messages on stdout are the one thing that cannot be wrong: a
        // stray `print` breaks the handshake, and an import outside the stdlib
        // fails on the bundled interpreter.
        assert!(
            SERVER_SCRIPT.contains("sys.stdout.write"),
            "must speak the protocol"
        );
        assert!(
            !SERVER_SCRIPT.contains("print("),
            "stdout carries protocol messages only; use stderr for diagnostics"
        );
        for forbidden in ["hashlib", "ctypes", "ssl", "requests", "socket"] {
            assert!(
                !SERVER_SCRIPT.contains(&format!("import {forbidden}")),
                "{forbidden} is not guaranteed in the bundled python3"
            );
        }
        assert!(SERVER_SCRIPT.contains("tools/list"));
        assert!(SERVER_SCRIPT.contains("tools/call"));
    }

    #[test]
    fn a_fresh_tree_gets_the_script_and_the_config_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let seeded = seed_missing(tmp.path(), true);
        assert_eq!(
            seeded.script.as_deref(),
            Some(tmp.path().join(SCRIPT_RELATIVE).as_path())
        );
        assert!(seeded.config_written, "{:?}", seeded.config_skipped);

        let config = std::fs::read_to_string(tmp.path().join("mcp.json")).unwrap();
        let json: serde_json::Value = serde_json::from_str(&config).unwrap();
        let entry = &json["mcpServers"][SERVER_KEY];
        assert_eq!(entry["command"], "python3");
        assert!(
            entry["args"][0]
                .as_str()
                .unwrap()
                .ends_with("mcp/example_server.py"),
            "{entry}"
        );

        // A second start changes nothing.
        let again = seed_missing(tmp.path(), true);
        assert!(again.script.is_none());
        assert!(!again.config_written);
    }

    #[test]
    fn an_existing_config_is_left_exactly_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("mcp.json");
        // JSONC, with a comment a rewrite would delete — the case that must not break.
        let original =
            "{\n  // my own servers\n  \"mcpServers\": {\"mine\": {\"command\": \"rg\"}}\n}\n";
        std::fs::write(&config, original).unwrap();

        let seeded = seed_missing(tmp.path(), true);
        assert!(!seeded.config_written);
        assert!(seeded.config_skipped.is_some());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
        // The script is still seeded: it is our file, next to theirs.
        assert!(tmp.path().join(SCRIPT_RELATIVE).exists());
    }

    #[test]
    fn without_the_runtime_only_the_script_is_seeded() {
        let tmp = tempfile::tempdir().unwrap();
        let seeded = seed_missing(tmp.path(), false);
        assert!(seeded.script.is_some());
        assert!(!seeded.config_written);
        assert!(
            !tmp.path().join("mcp.json").exists(),
            "an entry naming a command that cannot run is worse than none"
        );
        assert!(seeded.config_skipped.unwrap().contains("python3"));
    }
}
