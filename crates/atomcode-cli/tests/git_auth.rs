//! Run the real CLI with fake local probes: never touch the developer's agent or credentials.
#![cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn probe_script(directory: &std::path::Path, name: &str, body: &str) {
    let path = directory.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn status_uses_only_local_probes_and_never_prints_probe_output_or_creates_a_profile() {
    let root = tempfile::tempdir().unwrap();
    let bins = root.path().join("bin");
    std::fs::create_dir(&bins).unwrap();
    probe_script(&bins, "ssh-add", "[ \"$#\" = 1 ] && [ \"$1\" = -l ] || exit 99\necho SECRET_KEY_IDENTITY\necho SECRET_AGENT_ERROR >&2\nexit 0");
    probe_script(&bins, "git", "[ \"$#\" = 4 ] && [ \"$1\" = config ] && [ \"$2\" = --name-only ] && [ \"$3\" = --get-regexp ] || exit 99\necho SECRET_HELPER_OUTPUT\necho SECRET_HELPER_ERROR >&2\nexit 0");
    let profile = root.path().join("profile");
    let output = assert_cmd::Command::cargo_bin("atomcode")
        .unwrap()
        .args(["git-auth", "status", "--json"])
        .current_dir(root.path())
        .env("PATH", &bins)
        .env("ATOMCODE_HOME", &profile)
        .env("SSH_AUTH_SOCK", root.path().join("not-a-real-agent"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty(), "{:?}", output);
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains("SECRET"));
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["ssh_agent"], "identities_loaded");
    assert_eq!(value["credential_helper"], "setting_present");
    assert_eq!(value["remote_authentication_verified"], false);
    assert!(
        !profile.exists(),
        "a diagnostic must bypass startup side effects"
    );
}

#[test]
fn status_distinguishes_empty_and_unreachable_agents_and_missing_helpers() {
    let root = tempfile::tempdir().unwrap();
    probe_script(root.path(), "git", "exit 1");
    for (exit, expected) in [(1, "no_identities"), (2, "unavailable")] {
        probe_script(root.path(), "ssh-add", &format!("exit {exit}"));
        let output = assert_cmd::Command::cargo_bin("atomcode")
            .unwrap()
            .args(["git-auth", "status", "--json"])
            .current_dir(root.path())
            .env("PATH", root.path())
            .env("ATOMCODE_HOME", root.path().join("profile"))
            .output()
            .unwrap();
        assert!(output.status.success());
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["ssh_agent"], expected);
        assert_eq!(value["credential_helper"], "no_setting");
    }
}

#[test]
fn remember_refuses_piped_secrets_and_unlock_without_registrations_is_side_effect_free() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let output = assert_cmd::Command::cargo_bin("atomcode")
        .unwrap()
        .args(["git-auth", "remember", "--key", "nonexistent-key"])
        .env("ATOMCODE_HOME", &profile)
        .write_stdin("test-only-secret\n")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("test-only-secret"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("external terminal"));
    assert!(!profile.exists());
    let output = assert_cmd::Command::cargo_bin("atomcode")
        .unwrap()
        .args(["git-auth", "unlock"])
        .env("ATOMCODE_HOME", &profile)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!profile.exists());
}

#[test]
fn an_invalid_worker_request_never_creates_a_profile_or_prints_credentials() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let output = assert_cmd::Command::cargo_bin("atomcode")
        .unwrap()
        .arg("__ssh-key-worker")
        .arg(&profile)
        .arg("0")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert!(!profile.exists());
}
