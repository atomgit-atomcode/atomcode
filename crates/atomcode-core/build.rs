fn main() {
    // Always re-run: git HEAD changes on every commit.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads/");

    // Inject git short hash as ATOMCODE_BUILD_ID at compile time.
    // The DatalogWriter reads this via option_env! and falls back to "dev".
    // Without this file, every datalog would show [build:dev] even for release
    // builds, making post-hoc analysis of which commit produced a run impossible.
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    let hash = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    };
    println!("cargo:rustc-env=ATOMCODE_BUILD_ID={}", hash);
}
