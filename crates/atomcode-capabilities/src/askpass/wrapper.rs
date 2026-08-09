use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub fn write_askpass_script(exe: &Path, dir: &Path) -> std::io::Result<PathBuf> {
    let p = dir.join("askpass.sh");
    std::fs::write(
        &p,
        format!("#!/bin/sh\nexec \"{}\" __askpass \"$@\"\n", exe.display()),
    )?;
    std::fs::set_permissions(&p, PermissionsExt::from_mode(0o700))?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn writes_executable_wrapper_invoking_helper() {
        let dir = tempfile::tempdir().unwrap();
        let p =
            write_askpass_script(std::path::Path::new("/usr/bin/atomcode"), dir.path()).unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(
            body.contains(r#"exec "/usr/bin/atomcode" __askpass "$@""#),
            "{body}"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "must be 0700");
    }
}
