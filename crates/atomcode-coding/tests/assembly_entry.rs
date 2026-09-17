//! The product agent is assembled in one place (`docs/tui-replaces-tuix-plan.md`
//! M2): the row list and its defaults are this crate's, and every host mounts
//! them through `runtime::mount`. A second crate that stacked them itself would
//! be a second product, and the two drift the day either changes.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn the_assembly_constants_are_used_only_inside_the_coding_crate() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .to_path_buf();
    let mut offenders = Vec::new();
    for krate in std::fs::read_dir(&crates).expect("crates/").flatten() {
        if krate.file_name() == "atomcode-coding" {
            continue;
        }
        let mut files = Vec::new();
        rust_files(&krate.path().join("src"), &mut files);
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for (n, line) in text.lines().enumerate() {
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if ["CODING_DEFAULTS", "coding_overlay", "CODING_ROWS"]
                    .iter()
                    .any(|name| line.contains(name))
                {
                    offenders.push(format!("{}:{}: {}", file.display(), n + 1, line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the product assembly is mounted through `runtime::mount`, not restated: {offenders:#?}"
    );
}
