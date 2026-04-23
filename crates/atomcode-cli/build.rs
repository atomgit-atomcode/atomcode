fn main() {
    // Best-effort rebuild hints. These paths do NOT exist inside a git
    // worktree (where .git is a file, not a directory), so cargo will
    // silently treat them as "always re-run" — that's fine: the git
    // subprocess calls are cheap, and the env-var output is stable
    // between commits anyway. If you build inside a regular checkout
    // these become real cache hints.
    //
    // We watch .git/index (not .git/refs/heads/ like the tui crate does)
    // because dirty detection cares about staging-area changes, not
    // branch switches.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    // Inject git short hash as ATOMCODE_BUILD_ID at compile time.
    let hash = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // Detect dirty working tree. --untracked-files=no matches the
    // 'git describe --dirty' convention: only tracked-file changes count.
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=ATOMCODE_BUILD_ID={}", hash);
    println!(
        "cargo:rustc-env=ATOMCODE_BUILD_DIRTY={}",
        if dirty { "+dirty" } else { "" }
    );

    // Embed icon when targeting Windows. Gated on CARGO_CFG_TARGET_OS
    // (the *build target*, not the host) so cross-compile from macOS
    // fires this while native mac/linux builds skip it.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let icon = "assets/atomcode.ico";
        println!("cargo:rerun-if-changed={}", icon);
        let mut res = winresource::WindowsResource::new();
        res.set_icon(icon);
        if let Err(e) = res.compile() {
            println!("cargo:warning=winresource compile failed: {}", e);
        }
    }
}
