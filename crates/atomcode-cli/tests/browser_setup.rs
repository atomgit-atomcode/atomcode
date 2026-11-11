//! Test actual early CLI dispatch with an isolated user tree and inert backend.
use std::path::Path;
use std::process::{Command, Output};

fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_atomcode"))
        .arg("browser")
        .args(args)
        .env("ATOMCODE_HOME", root)
        .output()
        .unwrap()
}

#[test]
fn read_only_and_missing_backend_paths_do_not_create_product_tree() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("not-created");
    for cmd in ["status", "disable"] {
        assert!(invoke(&root, &[cmd]).status.success());
    }
    for cmd in ["doctor", "enable"] {
        assert!(!invoke(&root, &[cmd]).status.success());
    }
    assert!(!root.exists());
}

#[cfg(unix)]
#[test]
fn version_gate_prevents_writes_and_enable_disable_are_reversible() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("user");
    let node = t.path().join("fake node");
    let server = t.path().join("cli.js");
    std::fs::write(&server, "inert fixture, never executed").unwrap();
    let write_node = |version| {
        std::fs::write(&node, format!("#!/bin/sh\nif [ \"$#\" = 1 ] && [ \"$1\" = --version ]; then echo v22.0.0; elif [ \"$#\" = 2 ] && [ \"$2\" = --version ]; then echo 'Version {version}'; else exit 7; fi\n")).unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    write_node("0.0.1");
    let args = |cmd| {
        vec![
            cmd,
            "--server",
            server.to_str().unwrap(),
            "--node",
            node.to_str().unwrap(),
        ]
    };
    for cmd in ["doctor", "enable"] {
        let out = invoke(&root, &args(cmd));
        assert!(!out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("unsupported browser backend version")
        );
        assert!(!root.exists());
    }
    write_node("0.0.85");
    assert!(invoke(&root, &args("doctor")).status.success());
    assert!(!root.exists());
    let out = invoke(&root, &args("enable"));
    assert!(out.status.success(), "{out:?}");
    assert!(root.join("skills/browser-use/SKILL.md").is_file());
    assert!(invoke(&root, &["doctor"]).status.success());
    assert!(invoke(&root, &["disable"]).status.success());
    let config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("mcp.json")).unwrap()).unwrap();
    assert_eq!(config["mcpServers"]["browser-use"]["disabled"], true);
    assert!(!root.join("skills/browser-use/SKILL.md").exists());
}
