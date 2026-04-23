fn main() {
    // Always re-run: git HEAD changes on every commit.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads/");

    // Inject git short hash as ATOMCODE_BUILD_ID at compile time.
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output();
    let hash = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    };
    println!("cargo:rustc-env=ATOMCODE_BUILD_ID={}", hash);
}
