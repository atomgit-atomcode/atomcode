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

#[cfg(unix)]
#[test]
fn scoped_setup_cli_is_explicit_preserves_policy_and_rejects_unsupported_reports_without_writes() {
    use serde_json::{json, Value};
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("user");
    let backend = temp.path().join("origin backend.cjs");
    std::fs::write(&backend, "fixture").unwrap();
    let package = temp.path().join("package");
    std::fs::create_dir(&package).unwrap();
    std::fs::write(package.join("cli.js"), "fixture").unwrap();
    let node = temp.path().join("node fixture");
    let put_node = |count: usize, ready: bool| {
        let report = json!({"version":1,"kind":"doctor","browser_launched":!ready,"capability":"atomcode/browser-origin-scope","mcp":"0.0.85","playwright":"1.66.0-alpha-2026-11-09","allowed_origin_count":count,"downloads":"deny-artifacts","uploads":"deny","full_cdp_access":"deny"});
        std::fs::write(&node,format!("#!/bin/sh\nif [ \"$#\" = 1 ] && [ \"$1\" = --version ]; then echo v22.0.0; exit 0; fi\nfor arg do if [ \"$arg\" = --doctor ]; then printf '%s' '{report}'; exit 0; fi; done\necho 'Version 0.0.85'\n")).unwrap();
        std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    let first = vec![
        "enable",
        "--origin-backend",
        backend.to_str().unwrap(),
        "--backend-root",
        package.to_str().unwrap(),
        "--node",
        node.to_str().unwrap(),
        "--allow-origin",
        "https://EXAMPLE.test:443/",
    ];
    put_node(1, false);
    assert!(!invoke(&root, &first).status.success());
    assert!(!root.exists());
    put_node(1, true);
    let output = invoke(&root, &first);
    assert!(output.status.success(), "{output:?}");
    let read = || -> Value {
        serde_json::from_str(&std::fs::read_to_string(root.join("mcp.json")).unwrap()).unwrap()
    };
    let initial = read();
    let entry = &initial["mcpServers"]["browser-use"];
    assert_eq!(entry["originScope"], 1);
    assert_eq!(
        entry["requiredExperimentalCapabilities"]["atomcode/browser-origin-scope"],
        1
    );
    assert!(entry["env"]["ATOMCODE_BROWSER_ORIGIN_POLICY"]
        .as_str()
        .unwrap()
        .contains("https://example.test"));
    let output = invoke(&root, &["enable"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(read(), initial);
    assert!(invoke(&root, &["doctor"]).status.success());
    assert_eq!(read(), initial);
    let stock = package.join("cli.js");
    for args in [
        vec!["enable", "--allow-origin", "file:///tmp"],
        vec!["enable", "--deny-all-origins", "--unrestricted-origins"],
        vec!["enable", "--server", stock.to_str().unwrap()],
    ] {
        assert!(!invoke(&root, &args).status.success());
        assert_eq!(read(), initial);
    }
    put_node(0, true);
    let output = invoke(&root, &["enable", "--deny-all-origins"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&invoke(&root, &["status"]).stdout).contains("deny all origins")
    );
    let output = invoke(&root, &["enable", "--unrestricted-origins"]);
    assert!(output.status.success(), "{output:?}");
    assert!(read()["mcpServers"]["browser-use"]
        .get("originScope")
        .is_none());
    assert!(read()["mcpServers"]["browser-use"]["env"]
        .get("ATOMCODE_BROWSER_ORIGIN_POLICY")
        .is_none());
}
