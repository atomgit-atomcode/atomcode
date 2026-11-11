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

#[cfg(target_os = "macos")]
#[test]
fn doctor_and_refused_enable_identify_the_reported_app_without_startup_writes() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("not-created");
    let backend = temp.path().join("backend");
    let app = "/Applications/Open Computer Use (AtomCode Lease).app";
    let mut report = serde_json::json!({
        "schema_version":1, "kind":"doctor", "ui":false,
        "permissions":{"accessibility":false,"screen_recording":false},
        "runtime":{"permission_app":app,"bundle_identifier":"com.ifuryst.opencomputeruse.dev", "unrelated_secret":"not-for-output"},
    });
    let put = |report: &serde_json::Value| {
        std::fs::write(&backend, format!("#!/bin/sh\nprintf '%s' '{}'\n", report)).unwrap();
        std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    put(&report);
    for command in ["doctor", "enable"] {
        let output = invoke(&root, &[command, "--executable", backend.to_str().unwrap()]);
        assert!(!output.status.success());
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        assert!(
            diagnostic.contains(app) && diagnostic.contains("permissions incomplete"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("not-for-output"));
        assert!(!root.exists());
    }
    report["permissions"] = serde_json::json!({"accessibility":true,"screen_recording":true});
    put(&report);
    let output = invoke(
        &root,
        &["doctor", "--executable", backend.to_str().unwrap()],
    );
    assert!(output.status.success(), "{:?}", output);
    assert!(String::from_utf8_lossy(&output.stdout).contains(app));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("not-for-output"));
    assert!(!root.exists());
    report["runtime"]["permission_app"] = serde_json::json!("relative.app");
    put(&report);
    let output = invoke(
        &root,
        &["enable", "--executable", backend.to_str().unwrap()],
    );
    assert!(!output.status.success());
    assert!(!root.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn application_scope_cli_requires_backend_support_and_preserves_scope_on_plain_enable() {
    use serde_json::{json, Value};
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("user");
    let backend = temp.path().join("backend");
    let put_backend = |scope| {
        let mut report = json!({"schema_version":1,"kind":"doctor","ui":false,"permissions":{"accessibility":true,"screen_recording":true}});
        if scope {
            report["application_scope"] = json!({"version":1});
        }
        std::fs::write(&backend, format!("#!/bin/sh\n[ \"$1\" = doctor ] && [ \"$2\" = --no-ui ] || exit 2\nprintf '%s' '{report}'\n")).unwrap();
        std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
    };
    let path = backend.to_str().unwrap();
    put_backend(false);
    let output = invoke(
        &root,
        &[
            "enable",
            "--executable",
            path,
            "--allow-app",
            "com.apple.TextEdit",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("application_scope version 1"));
    assert!(!root.exists());
    put_backend(true);
    let output = invoke(
        &root,
        &[
            "enable",
            "--executable",
            path,
            "--allow-app",
            "com.apple.TextEdit",
            "--allow-app",
            "COM.APPLE.TEXTEDIT",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let read = || -> Value {
        serde_json::from_str(&std::fs::read_to_string(root.join("mcp.json")).unwrap()).unwrap()
    };
    let expected = read();
    assert_eq!(
        expected["mcpServers"]["computer-use"]["env"]["OPEN_COMPUTER_USE_ALLOWED_BUNDLE_IDS"],
        "[\"com.apple.textedit\"]"
    );
    assert_eq!(
        expected["mcpServers"]["computer-use"]["requiredExperimentalCapabilities"]
            ["atomcode/application-scope"],
        1
    );
    let output = invoke(&root, &["enable"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(read(), expected);
    let status = invoke(&root, &["status"]);
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("Application scope: com.apple.textedit")
    );
    for options in [
        vec!["--allow-app", "com.*"],
        vec!["--deny-all-apps", "--unrestricted-apps"],
        vec!["--allow-app", "com.app", "--deny-all-apps"],
    ] {
        let mut args = vec!["enable"];
        args.extend(options);
        assert!(!invoke(&root, &args).status.success());
        assert_eq!(read(), expected);
    }
    let output = invoke(&root, &["enable", "--deny-all-apps"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        read()["mcpServers"]["computer-use"]["env"]["OPEN_COMPUTER_USE_ALLOWED_BUNDLE_IDS"],
        "[]"
    );
    put_backend(false);
    let deny_all = read();
    let output = invoke(&root, &["enable"]);
    assert!(!output.status.success());
    assert_eq!(read(), deny_all);
    let output = invoke(&root, &["enable", "--unrestricted-apps"]);
    assert!(output.status.success(), "{output:?}");
    assert!(read()["mcpServers"]["computer-use"]["env"]
        .get("OPEN_COMPUTER_USE_ALLOWED_BUNDLE_IDS")
        .is_none());
}
