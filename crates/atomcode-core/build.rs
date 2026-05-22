//! Build script for atomcode-core.
//!
//! 1. Inject git short hash as `ATOMCODE_BUILD_ID` so DatalogWriter can tag
//!    every run with the commit that produced the binary.
//! 2. Pack `assets/setup-seeds/` into `OUT_DIR/setup-seeds.tar.zst` so
//!    `seeds.rs` can `include_bytes!` it. The packed archive is extracted at
//!    first run to `~/.atomcode/seeds-cache/<binary-sha>/`.

use std::fs::File;
use std::path::PathBuf;

fn main() {
    inject_build_id();
    pack_setup_seeds();
}

fn inject_build_id() {
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

fn pack_setup_seeds() {
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let seeds_dir = crate_dir.join("assets/setup-seeds");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let out_path = out_dir.join("setup-seeds.tar.zst");

    println!("cargo:rerun-if-changed=assets/setup-seeds");

    if !seeds_dir.exists() {
        // No seeds yet → write empty archive so include_bytes! works.
        let f = File::create(&out_path).expect("create empty tar.zst");
        let zstd_enc = zstd::Encoder::new(f, 3).expect("zstd encoder");
        let mut tar = tar::Builder::new(zstd_enc);
        tar.finish().expect("empty tar finish");
        tar.into_inner()
            .expect("zstd into_inner")
            .finish()
            .expect("zstd finish");
        return;
    }

    let f = File::create(&out_path).expect("create tar.zst");
    let zstd_enc = zstd::Encoder::new(f, 19)
        .expect("zstd encoder")
        .auto_finish();
    let mut tar = tar::Builder::new(zstd_enc);
    tar.append_dir_all(".", &seeds_dir).expect("tar append");
    tar.finish().expect("tar finish");
}
