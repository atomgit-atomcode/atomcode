//! Approval fixtures must remain outside the temp-path exception even when the
//! repository itself is a temporary worktree. Each guard owns its test data.

pub fn tempdir() -> tempfile::TempDir {
    let root = std::env::var_os("ATOMCODE_TEST_NON_TEMP_ROOT")
        .map(std::path::PathBuf::from)
        .or_else(dirs::cache_dir)
        .expect("approval scenarios need a writable non-temporary cache directory")
        .join("atomcode-approval-tests");
    std::fs::create_dir_all(&root).expect("non-temporary approval scratch root");
    let root = std::fs::canonicalize(root).unwrap();
    for temp in [std::env::temp_dir(), std::path::PathBuf::from("/tmp")] {
        if let Ok(temp) = std::fs::canonicalize(temp) {
            assert!(
                !root.starts_with(temp),
                "approval scratch is temporary; set ATOMCODE_TEST_NON_TEMP_ROOT to a non-temporary writable directory"
            );
        }
    }
    tempfile::Builder::new()
        .prefix("approval-")
        .tempdir_in(root)
        .unwrap()
}
