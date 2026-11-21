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
    // Windows `canonicalize` yields a verbatim `\\?\C:\…` path. Hand back the
    // ordinary spelling: fixtures embed these paths raw into JSON tool
    // arguments, where a leading `\\` is a legal escape decoding to ONE
    // backslash, so the verbatim prefix cannot round-trip the args-repair
    // chain. The temp check above stays on canonical forms so it still matches.
    let root = plain_path(root);
    tempfile::Builder::new()
        .prefix("approval-")
        .tempdir_in(root)
        .unwrap()
}

/// Strip the Windows verbatim drive prefix (`\\?\C:\…` → `C:\…`). The path is
/// already canonical, so the ordinary form addresses the same location (the
/// approval gates canonicalize again internally). `\\?\UNC\…` is left alone —
/// it has no safe ordinary spelling. No-op on other platforms.
fn plain_path(path: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        if let Some(text) = path.to_str() {
            if let Some(rest) = text.strip_prefix(r"\\?\") {
                let bytes = rest.as_bytes();
                if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                    return std::path::PathBuf::from(rest);
                }
            }
        }
    }
    path
}
