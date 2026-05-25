//! First-run notice.

use anyhow::Result;
use std::fs;
use std::path::Path;

pub const MARKER: &str = ".telemetry_notice_shown";

pub const NOTICE_TEXT: &str = "\
ℹ AtomCode collects anonymous usage telemetry to improve the product.
  Opt out: `atomcode telemetry disable`  |  Details: https://atomgit.com/atomgit_atomcode/atomcode/blob/main/docs/telemetry.md
";

pub fn should_show_and_mark(atomcode_dir: &Path) -> Result<bool> {
    let m = atomcode_dir.join(MARKER);
    if m.exists() {
        return Ok(false);
    }
    fs::create_dir_all(atomcode_dir)?;
    fs::write(&m, "")?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn first_call_true_subsequent_false() {
        let d = TempDir::new().unwrap();
        assert!(should_show_and_mark(d.path()).unwrap());
        assert!(!should_show_and_mark(d.path()).unwrap());
        assert!(d.path().join(MARKER).exists());
    }
}
