//! CI gate: scripts/uninstall.{sh,ps1} hardcode a path manifest that MUST
//! match crates/atomcode-cli/src/uninstall/paths.rs::uninstall_manifest().
//! Any drift breaks symmetry between the subcommand and fallback scripts.

use atomcode::uninstall::paths::uninstall_manifest;
use std::collections::{HashMap, HashSet};
use std::process::Command;

// Redirect ATOMCODE_HOME to a throwaway temp dir before any test in this binary
// runs, so tests never persist into the developer's real home. isolate_home is a
// inherited shell values are replaced before tests run.
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn parse_kv(output: &str) -> HashMap<String, HashSet<String>> {
    let mut out = HashMap::new();
    for line in output.lines() {
        if let Some((k, v)) = line.split_once('=') {
            let set: HashSet<String> = v.split_whitespace().map(String::from).collect();
            out.insert(k.to_string(), set);
        }
    }
    out
}

fn rust_manifest_as_kv() -> HashMap<String, HashSet<String>> {
    let m = uninstall_manifest();
    let mut out = HashMap::new();
    out.insert(
        "group2_files".into(),
        m.credential_files.iter().map(|s| (*s).into()).collect(),
    );
    out.insert(
        "group3_files".into(),
        m.state_files.iter().map(|s| (*s).into()).collect(),
    );
    out.insert(
        "group3_dirs".into(),
        m.state_dirs.iter().map(|s| (*s).into()).collect(),
    );
    out.insert(
        "group3_prefixes".into(),
        m.state_prefixes.iter().map(|s| (*s).into()).collect(),
    );
    out
}

fn workspace_root() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/atomcode-cli → crates
    p.pop(); // crates → workspace root
    p
}

#[test]
fn shell_script_matches_rust() {
    let script = workspace_root().join("scripts/uninstall.sh");
    let out = Command::new("sh")
        .arg(&script)
        .arg("--print-manifest")
        .output()
        .expect("run uninstall.sh --print-manifest");
    assert!(
        out.status.success(),
        "uninstall.sh failed: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        parse_kv(&stdout),
        rust_manifest_as_kv(),
        "scripts/uninstall.sh manifest drift detected"
    );
}

#[test]
fn powershell_script_matches_rust() {
    let script = workspace_root().join("scripts/uninstall.ps1");
    let pwsh = match which::which("pwsh")
        .ok()
        .or_else(|| which::which("powershell").ok())
    {
        Some(p) => p,
        None => {
            eprintln!("skipping: no pwsh or powershell on PATH");
            return;
        }
    };
    let out = Command::new(pwsh)
        .args(["-File"])
        .arg(&script)
        .arg("-PrintManifest")
        .output()
        .expect("run uninstall.ps1 -PrintManifest");
    assert!(
        out.status.success(),
        "uninstall.ps1 failed: stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        parse_kv(&stdout),
        rust_manifest_as_kv(),
        "scripts/uninstall.ps1 manifest drift detected"
    );
}
