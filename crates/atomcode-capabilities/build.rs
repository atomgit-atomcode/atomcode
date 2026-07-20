//! Build script for atomcode-capabilities.
//!
//! Pack `assets/setup-seeds/` into `OUT_DIR/setup-seeds.tar.zst` so the
//! `setup` capability's `seeds.rs` can `include_bytes!` it. The packed
//! archive is extracted at first run to `~/.atomcode/seeds-cache/<binary-sha>/`.
//!
//! (Moved here from `atomcode-core/build.rs` alongside the `setup` module.)

use std::fs::File;
use std::path::PathBuf;

fn main() {
    // Only pack when the `setup` feature is on — its `seeds.rs` is the sole `include_bytes!`
    // consumer of the archive, compiled only under that feature. Cargo sets CARGO_FEATURE_SETUP
    // when the feature is active (build scripts CAN read features). The zstd/tar build-deps
    // still compile regardless — Cargo can't feature-gate [build-dependencies] — but a lean
    // (no-setup) build then skips the packing work and the empty-archive I/O.
    if std::env::var_os("CARGO_FEATURE_SETUP").is_some() {
        pack_setup_seeds();
    }
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
