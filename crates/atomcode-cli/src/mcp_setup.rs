//! Host-local MCP setup file guards and owned diagnostic-process cleanup.
use anyhow::{bail, Result};
use atomcode_capabilities::{
    setup::{fs_atomic::atomic_write, lock::SetupLock},
    ProductDirs,
};
use std::path::Path;
pub(crate) fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if !meta.file_type().is_file() => {
            bail!("refusing non-regular file: {}", path.display())
        }
        Ok(_) => Ok(Some(std::fs::read_to_string(path)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn write_checked(path: &Path, before: &Option<String>, text: &str) -> Result<()> {
    if &read_optional(path)? != before {
        bail!("{} changed; retry without overwriting it", path.display());
    }
    atomic_write(path, text.as_bytes(), 0o600)
}

/// Managed MCP setup edits one document. Serialize setup writers; optimistic text checks still detect non-cooperating edits.
pub(crate) fn lock_mcp_setup(dirs: &ProductDirs) -> Result<SetupLock> {
    Ok(SetupLock::acquire(
        &dirs.user().join("integrations/mcp-setup"),
        false,
    )?)
}

#[cfg(unix)]
pub(crate) struct ProbeGroup(pub(crate) Option<i32>);
#[cfg(unix)]
impl Drop for ProbeGroup {
    fn drop(&mut self) {
        if let Some(group) = self.0.take() {
            // The probe starts its own process group; kill wrappers and their
            // children on deadline/cancellation, never another runtime's group.
            unsafe {
                libc::kill(-group, libc::SIGKILL);
            }
        }
    }
}
