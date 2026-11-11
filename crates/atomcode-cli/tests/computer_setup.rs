//! Independent-process checks include the binary's startup path, not only the
//! configuration helpers. Never capture or act on the real desktop.
use std::path::Path;
use std::process::{Command, Output};

fn invoke(root: &Path, args: &[&str]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_atomcode"))
        .args(["computer"])
        .args(args)
        .env("ATOMCODE_HOME", root)
        .output()
        .unwrap();
    output
}

#[test]
fn status_and_absent_disable_have_no_startup_writes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("not-created");
    for command in ["status", "disable"] {
        let output = invoke(&root, &[command]);
        assert!(output.status.success(), "{:?}", output);
        assert!(!root.exists(), "local query created a product tree");
    }
}

#[cfg(unix)]
#[test]
fn permissions_gate_prevents_even_startup_writes_and_enable_is_reversible() {
    use std::os::unix::fs::PermissionsExt;
    if !cfg!(target_os = "macos") {
        return; // The initial integration is deliberately macOS-only.
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("user");
    let backend = temp.path().join("backend with spaces");
    let script = |ready| {
        format!(
        "#!/bin/sh\n[ \"$#\" = 2 ] && [ \"$1\" = doctor ] && [ \"$2\" = --no-ui ] || exit 2\nprintf '%s' '{{\"schema_version\":1,\"kind\":\"doctor\",\"ui\":false,\"permissions\":{{\"accessibility\":{ready},\"screen_recording\":{ready}}}}}'\n"
    )
    };
    std::fs::write(&backend, script(false)).unwrap();
    std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    for command in ["doctor", "enable"] {
        let output = invoke(&root, &[command, "--executable", backend.to_str().unwrap()]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("permissions incomplete"));
        assert!(!root.exists(), "failed probe created a product tree");
    }
    std::fs::write(&backend, script(true)).unwrap();
    let output = invoke(
        &root,
        &["enable", "--executable", backend.to_str().unwrap()],
    );
    assert!(output.status.success(), "{:?}", output);
    let read = || -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(root.join("mcp.json")).unwrap()).unwrap()
    };
    assert_eq!(read()["mcpServers"]["computer-use"]["trust"], false);
    assert!(root.join("skills/computer-use/SKILL.md").is_file());
    let output = invoke(&root, &["disable"]);
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(read()["mcpServers"]["computer-use"]["disabled"], true);
    assert!(!root.join("skills/computer-use/SKILL.md").exists());
}
