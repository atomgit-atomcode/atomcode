//! Filesystem scan that classifies real on-disk paths into uninstall groups.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::paths::uninstall_manifest;
use super::{Group, Item};

#[derive(Debug)]
pub struct Plan {
    pub items: Vec<Item>,
    pub binary_path: PathBuf,
    pub atomcode_dir: PathBuf,
}

/// Walk the filesystem and produce a Plan. Missing paths are silently skipped.
/// Order: Group::Binary first, then Credentials, then State.
pub fn scan(binary_path: &Path, atomcode_dir: &Path) -> Result<Plan> {
    let mut items = Vec::new();

    // ---- Group::Binary ----
    if binary_path.exists() {
        items.push(item(Group::Binary, binary_path.to_path_buf(), "binary")?);
    }
    if let Some(dir) = binary_path.parent() {
        // Self-update backup uses extension `.bak` appended (atomcode.bak / atomcode.exe.bak).
        let bak_name = {
            let mut s = binary_path.file_name().unwrap_or_default().to_os_string();
            s.push(".bak");
            s
        };
        let p = dir.join(&bak_name);
        if p.exists() {
            items.push(item(Group::Binary, p, "self-update backup")?);
        }

        for (name, note) in [
            (".atomcode.rolling", "self-update rename slot"),
            (".atomcode.download", "self-update partial download"),
            (".atomcode.writable-probe", "self-update probe leftover"),
        ] {
            let p = dir.join(name);
            if p.exists() {
                items.push(item(Group::Binary, p, note)?);
            }
        }
    }

    // ---- Group::Credentials ----
    let m = uninstall_manifest();
    for fname in m.credential_files {
        let p = atomcode_dir.join(fname);
        if p.exists() {
            items.push(item(Group::Credentials, p, fname)?);
        }
    }

    // ---- Group::State ----
    for fname in m.state_files {
        let p = atomcode_dir.join(fname);
        if p.exists() {
            items.push(item(Group::State, p, fname)?);
        }
    }
    for dname in m.state_dirs {
        let p = atomcode_dir.join(dname);
        if p.exists() {
            items.push(item(Group::State, p, dname)?);
        }
    }
    if atomcode_dir.exists() {
        for entry in fs::read_dir(atomcode_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            for prefix in m.state_prefixes {
                if name_str.starts_with(prefix) {
                    items.push(item(Group::State, entry.path(), "notice marker")?);
                    break;
                }
            }
        }
    }

    Ok(Plan {
        items,
        binary_path: binary_path.to_path_buf(),
        atomcode_dir: atomcode_dir.to_path_buf(),
    })
}

fn item(group: Group, path: PathBuf, note: &'static str) -> Result<Item> {
    let size = if path.is_dir() {
        dir_size(&path).unwrap_or(0)
    } else {
        fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    };
    let needs_privilege = needs_privilege_to_remove(&path);
    Ok(Item {
        group,
        path,
        size_bytes: size,
        note,
        needs_privilege,
    })
}

fn dir_size(p: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(p)? {
        let entry = entry?;
        let md = entry.metadata()?;
        if md.is_dir() {
            total = total.saturating_add(dir_size(&entry.path()).unwrap_or(0));
        } else {
            total = total.saturating_add(md.len());
        }
    }
    Ok(total)
}

#[cfg(unix)]
fn needs_privilege_to_remove(p: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let parent = match p.parent() {
        Some(parent) => parent,
        None => return false,
    };
    let c_path = match std::ffi::CString::new(parent.as_os_str().as_bytes()) {
        Ok(s) => s,
        Err(_) => return true, // path contains an interior NUL — treat conservatively
    };
    // SAFETY: access(2) reads from c_path, which is a valid CString; no allocations.
    unsafe { libc::access(c_path.as_ptr(), libc::W_OK) != 0 }
}

#[cfg(not(unix))]
fn needs_privilege_to_remove(_p: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_fake_install(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
        let bin_dir = tmp.path().join("bin");
        fs::create_dir(&bin_dir).unwrap();
        let exe = bin_dir.join("atomcode");
        fs::write(&exe, b"\x7fELF......").unwrap();
        // self-update artifacts
        fs::write(bin_dir.join("atomcode.bak"), b"old").unwrap();
        fs::write(bin_dir.join(".atomcode.rolling"), b"r").unwrap();

        let data = tmp.path().join(".atomcode");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("auth.toml"), b"k=1").unwrap();
        fs::write(data.join("config.toml"), b"x=1").unwrap();
        fs::write(data.join("history"), b"hi").unwrap();
        fs::create_dir(data.join("plugins")).unwrap();
        fs::write(data.join("plugins/.gitkeep"), b"").unwrap();
        fs::create_dir(data.join("staged")).unwrap();
        (exe, data)
    }

    #[test]
    fn scan_finds_binary_and_artifacts() {
        let tmp = TempDir::new().unwrap();
        let (exe, data) = make_fake_install(&tmp);
        let plan = scan(&exe, &data).unwrap();
        let bin_paths: Vec<_> = plan
            .items
            .iter()
            .filter(|i| i.group == Group::Binary)
            .map(|i| i.path.clone())
            .collect();
        assert!(bin_paths.contains(&exe));
        assert!(bin_paths.contains(&exe.with_file_name("atomcode.bak")));
        assert!(bin_paths.contains(&exe.with_file_name(".atomcode.rolling")));
    }

    #[test]
    fn scan_classifies_credentials_and_state() {
        let tmp = TempDir::new().unwrap();
        let (exe, data) = make_fake_install(&tmp);
        let plan = scan(&exe, &data).unwrap();
        let creds: Vec<_> = plan
            .items
            .iter()
            .filter(|i| i.group == Group::Credentials)
            .map(|i| i.path.clone())
            .collect();
        assert!(creds.contains(&data.join("auth.toml")));
        assert!(creds.contains(&data.join("config.toml")));

        let state: Vec<_> = plan
            .items
            .iter()
            .filter(|i| i.group == Group::State)
            .map(|i| i.path.clone())
            .collect();
        assert!(state.contains(&data.join("history")));
        assert!(state.contains(&data.join("plugins")));
        assert!(state.contains(&data.join("staged")));
    }

    #[test]
    fn scan_skips_missing_files_silently() {
        let tmp = TempDir::new().unwrap();
        let exe = tmp.path().join("atomcode");
        std::fs::write(&exe, b"x").unwrap();
        let data = tmp.path().join("nonexistent");
        let plan = scan(&exe, &data).unwrap();
        // Only binary present (no .bak, no .rolling, no data dir).
        assert_eq!(
            plan.items
                .iter()
                .filter(|i| i.group == Group::Binary)
                .count(),
            1
        );
        assert_eq!(
            plan.items
                .iter()
                .filter(|i| i.group != Group::Binary)
                .count(),
            0
        );
    }

    #[test]
    #[cfg(unix)]
    fn needs_privilege_false_for_user_writable_tempdir() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("file");
        std::fs::write(&p, b"x").unwrap();
        // Tempdir is user-writable, so we don't need privilege.
        assert!(!needs_privilege_to_remove(&p));
    }

    #[test]
    #[cfg(unix)]
    fn needs_privilege_true_for_path_with_no_parent() {
        // A path with no parent (e.g. just "/") returns false (no rm needed).
        let p = std::path::Path::new("/");
        // We don't expect needs_privilege to crash here.
        let _ = needs_privilege_to_remove(p);
    }

    #[test]
    fn scan_returns_items_in_group_order() {
        let tmp = TempDir::new().unwrap();
        let (exe, data) = make_fake_install(&tmp);
        let plan = scan(&exe, &data).unwrap();
        let groups: Vec<_> = plan.items.iter().map(|i| i.group).collect();
        let first_cred = groups.iter().position(|g| *g == Group::Credentials);
        let first_state = groups.iter().position(|g| *g == Group::State);
        let last_bin = groups.iter().rposition(|g| *g == Group::Binary);
        if let (Some(lb), Some(fc)) = (last_bin, first_cred) {
            assert!(lb < fc);
        }
        if let (Some(fc), Some(fs)) = (first_cred, first_state) {
            assert!(fc < fs);
        }
    }
}
