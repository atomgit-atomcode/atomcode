fn main() {
    // Re-run on git HEAD changes so the hash stays fresh.
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

    // Detect dirty working tree.
    let dirty = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    println!("cargo:rustc-env=ATOMCODE_BUILD_ID={}", hash);
    println!(
        "cargo:rustc-env=ATOMCODE_BUILD_DIRTY={}",
        if dirty { "+dirty" } else { "" }
    );
}
