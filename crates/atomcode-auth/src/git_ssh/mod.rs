//! Explicit, OS-backed SSH registrations. No model-facing API returns a passphrase.
mod agent;
mod loader;
mod vault;

use anyhow::{bail, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use vault::{SystemVault, Vault};
use zeroize::Zeroizing;

pub const WORKER_COMMAND: &str = "__ssh-key-worker";
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    path: PathBuf,
    digest: String,
    namespace: String,
    lifetime_secs: Option<u32>,
    active: bool,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    version: u32,
    keys: Vec<Binding>,
}
fn registry_path(root: &Path) -> PathBuf {
    root.join("auth").join("ssh-keys.json")
}

fn read_registry(root: &Path) -> Result<Registry> {
    let path = registry_path(root);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Registry {
                version: 1,
                keys: Vec::new(),
            })
        }
        Err(_) => bail!("Cannot inspect SSH registrations"),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 65536 {
        bail!("Invalid SSH registration file");
    }
    let registry: Registry = serde_json::from_slice(&fs::read(path)?)
        .map_err(|_| anyhow::anyhow!("Invalid SSH registration metadata"))?;
    if registry.version != 1 || registry.keys.len() > 8 {
        bail!("Unsupported SSH registration metadata");
    }
    for key in &registry.keys {
        validate_lifetime(key.lifetime_secs)?;
    }
    Ok(registry)
}
fn lock(root: &Path) -> Result<File> {
    let directory = root.join("auth");
    fs::create_dir_all(&directory)?;
    if fs::symlink_metadata(&directory)?.file_type().is_symlink() {
        bail!("SSH registration directory cannot be a symlink");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    let path = directory.join("ssh-keys.lock");
    if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        bail!("Invalid SSH registration lock");
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock_exclusive()
        .map_err(|_| anyhow::anyhow!("SSH registrations are busy"))?;
    Ok(file)
}
fn write_registry(root: &Path, registry: &Registry) -> Result<()> {
    let path = registry_path(root);
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(&serde_json::to_vec_pretty(registry)?)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|_| anyhow::anyhow!("Cannot commit SSH registrations"))?;
    Ok(())
}
fn validate_lifetime(seconds: Option<u32>) -> Result<()> {
    if seconds.is_some_and(|seconds| !(60..=86400).contains(&seconds)) {
        bail!("SSH key lifetime must be 60..86400 seconds");
    }
    Ok(())
}
fn digest(path: &Path) -> Result<String> {
    let meta = fs::metadata(path)?;
    if !meta.is_file() || meta.len() > 256 * 1024 {
        bail!("SSH key must be a regular file of at most 256 KiB");
    }
    let mut bytes = Zeroizing::new(Vec::new());
    File::open(path)?
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 256 * 1024 {
        bail!("SSH key is too large");
    }
    Ok(format!("{:x}", Sha256::digest(&*bytes)))
}
fn namespace(root: &Path) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&fs::canonicalize(root)?)?)
    ))
}
fn account(binding: &Binding) -> Result<String> {
    let bytes = serde_json::to_vec(&(&binding.namespace, &binding.path, &binding.digest))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn verified(root: &Path, binding: &Binding) -> Result<()> {
    if binding.namespace != namespace(root)?
        || !binding.path.is_absolute()
        || fs::canonicalize(&binding.path)? != binding.path
        || digest(&binding.path)? != binding.digest
    {
        bail!("Registered SSH key changed; forget it and register it again");
    }
    Ok(())
}

/// Registration is an explicit CLI action, with a passphrase supplied by hidden
/// terminal input. Validate by loading before persisting anything.
pub fn remember(
    root: &Path,
    key: &Path,
    lifetime: Option<u32>,
    secret: Zeroizing<String>,
) -> Result<()> {
    remember_with(root, key, lifetime, secret, &SystemVault, loader::load)
}

fn remember_with(
    root: &Path,
    key: &Path,
    lifetime: Option<u32>,
    secret: Zeroizing<String>,
    vault: &dyn Vault,
    load: impl FnOnce(&Path, Option<u32>, &str) -> Result<()>,
) -> Result<()> {
    remember_with_cancellation(root, key, lifetime, secret, vault, load, &|| false)
}

/// A host-owned cancellation flag is checked around irreversible native calls.
/// Cancellation never enables a saved registration; agent keys may remain loaded.
pub fn remember_cancellable(
    root: &Path,
    key: &Path,
    lifetime: Option<u32>,
    secret: Zeroizing<String>,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    remember_with_cancellation(
        root,
        key,
        lifetime,
        secret,
        &SystemVault,
        loader::load,
        &cancelled,
    )
}

fn remember_with_cancellation(
    root: &Path,
    key: &Path,
    lifetime: Option<u32>,
    secret: Zeroizing<String>,
    vault: &dyn Vault,
    load: impl FnOnce(&Path, Option<u32>, &str) -> Result<()>,
    cancelled: &dyn Fn() -> bool,
) -> Result<()> {
    if cancelled() {
        bail!("SSH registration cancelled");
    }
    validate_lifetime(lifetime)?;
    loader::validate_passphrase(&secret)?;
    let _lock = lock(root)?;
    let mut registry = read_registry(root)?;
    let path = fs::canonicalize(key)?;
    if registry.keys.iter().any(|k| k.path == path) {
        bail!("SSH key is already registered; forget it before replacing its registration");
    }
    if registry.keys.len() >= 8 {
        bail!("At most eight SSH keys can be registered");
    }
    let mut binding = Binding {
        digest: digest(&path)?,
        namespace: namespace(root)?,
        path,
        lifetime_secs: lifetime,
        active: true,
    };
    if cancelled() {
        bail!("SSH registration cancelled");
    }
    load(&binding.path, lifetime, &secret)?;
    if cancelled() {
        bail!("SSH registration cancelled; the agent key may remain loaded until expiry");
    }
    verified(root, &binding)?;
    let account = account(&binding)?;
    // Persist a disabled registration first, so every uncertain keyring write
    // has a durable cleanup identity and can be retried with forget.
    binding.active = false;
    registry.keys.push(binding);
    write_registry(root, &registry)?;
    if cancelled() {
        bail!("SSH registration cancelled; automatic loading is disabled. Check pending cleanup records");
    }
    vault.set(&account, &secret).map_err(|_| anyhow::anyhow!("OS keyring save failed; automatic loading is disabled. Retry forget to clean up; the agent key may remain loaded until removed or expired"))?;
    if cancelled() {
        if vault.delete(&account).is_ok() {
            registry.keys.pop();
            write_registry(root, &registry)?;
        }
        bail!("SSH registration cancelled; automatic loading is disabled. Check pending cleanup records and loaded agent keys");
    }
    registry.keys.last_mut().unwrap().active = true;
    if write_registry(root, &registry).is_err() {
        // The on-disk registration remains disabled. Keep it for cleanup even
        // if deleting the keyring item fails; never lose the account binding.
        let cleared = vault.delete(&account).is_ok();
        bail!(if cleared {
            "SSH registration commit failed; saved passphrase removed (agent key may remain loaded until removed or expired)"
        } else {
            "SSH registration commit failed; automatic loading remains disabled. Retry forget for keyring cleanup"
        });
    }
    Ok(())
}

/// Stop future automatic loading first. Other agents and already-loaded keys
/// remain governed by their own lifetimes; never clear unrelated agent keys.
pub fn forget(root: &Path, key: &Path) -> Result<()> {
    forget_with(root, key, &SystemVault)
}
fn forget_with(root: &Path, key: &Path, vault: &dyn Vault) -> Result<()> {
    if read_registry(root)?.keys.is_empty() {
        bail!("SSH key is not registered");
    }
    let _lock = lock(root)?;
    let mut registry = read_registry(root)?;
    let path = if registry.keys.iter().any(|binding| binding.path == key) {
        key.to_path_buf()
    } else {
        fs::canonicalize(key).unwrap_or_else(|_| key.to_path_buf())
    };
    let index = registry
        .keys
        .iter()
        .position(|k| k.path == path)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "SSH key is not registered (use its absolute registered path if it was deleted)"
            )
        })?;
    registry.keys[index].active = false;
    let account = account(&registry.keys[index])?;
    write_registry(root, &registry)?;
    vault.delete(&account).map_err(|_| anyhow::anyhow!("Automatic loading disabled, but keyring deletion failed; retry forget to remove the saved item"))?;
    registry.keys.remove(index);
    write_registry(root, &registry)
}

/// Safe UI metadata: no key contents, namespace IDs, hashes or passphrases.
#[derive(Clone)]
pub struct RegisteredKey {
    pub path: PathBuf,
    pub lifetime_secs: Option<u32>,
    pub active: bool,
}
pub fn registered_keys(root: &Path) -> Result<Vec<RegisteredKey>> {
    Ok(read_registry(root)?
        .keys
        .into_iter()
        .map(|key| RegisteredKey {
            path: key.path,
            lifetime_secs: key.lifetime_secs,
            active: key.active,
        })
        .collect())
}

/// Used only when the user explicitly chooses credential removal at uninstall.
/// A deletion failure preserves the disabled registration for a later retry.
pub fn forget_all(root: &Path) -> Result<()> {
    let paths: Vec<_> = read_registry(root)?
        .keys
        .into_iter()
        .map(|key| key.path)
        .collect();
    for path in paths {
        forget(root, &path)?;
    }
    Ok(())
}

pub fn registration_status(root: &Path) -> Result<(usize, usize)> {
    let registry = read_registry(root)?;
    let active = registry.keys.iter().filter(|k| k.active).count();
    Ok((active, registry.keys.len() - active))
}

pub fn registered_count(root: &Path) -> Result<usize> {
    Ok(registration_status(root)?.0)
}

fn unlock_worker(root: &Path, index: usize) -> Result<()> {
    if registered_count(root)? <= index {
        bail!("SSH registration no longer exists");
    }
    agent::verify(None)?;
    unlock_with(root, index, &SystemVault, loader::load)
}
fn unlock_with(
    root: &Path,
    index: usize,
    vault: &dyn Vault,
    load: impl FnOnce(&Path, Option<u32>, &str) -> Result<()>,
) -> Result<()> {
    if read_registry(root)?
        .keys
        .iter()
        .filter(|k| k.active)
        .nth(index)
        .is_none()
    {
        bail!("SSH registration no longer exists");
    }
    let _lock = lock(root)?;
    let registry = read_registry(root)?;
    let key = registry
        .keys
        .iter()
        .filter(|k| k.active)
        .nth(index)
        .ok_or_else(|| anyhow::anyhow!("SSH registration no longer exists"))?;
    verified(root, key)?;
    let secret = vault.get(&account(key)?)?;
    loader::validate_passphrase(&secret)?;
    load(&key.path, key.lifetime_secs, &secret)?;
    verified(root, key)
}

/// Must run before logging/telemetry in both host executables. This entry point
/// can load a registered key but can never print or return its passphrase.
pub fn worker_exit_code() -> Option<i32> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_none_or(|s| s != WORKER_COMMAND) {
        return None;
    }
    // A worker owns its session, so timeout cleanup cannot signal the host's
    // terminal/process group. The watchdog also bounds a worker whose host dies
    // while a native keyring call is blocked.
    #[cfg(unix)]
    unsafe {
        if libc::setsid() == -1 && libc::getsid(0) != libc::getpid() {
            return Some(1);
        }
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(35));
        #[cfg(unix)]
        unsafe {
            libc::kill(-libc::getpid(), libc::SIGKILL);
        }
        std::process::exit(2);
    });
    let code = if args.len() == 4 {
        args[3]
            .to_str()
            .and_then(|s| s.parse::<usize>().ok())
            .is_some_and(|index| unlock_worker(Path::new(&args[2]), index).is_ok())
    } else {
        false
    };
    Some(if code { 0 } else { 1 })
}

/// A bounded worker keeps native keyring IPC and PTY handling out of the live
/// runtime. Empty registrations are a no-op and create no files or processes.
pub fn unlock_registered(root: &Path, executable: &Path) -> Result<usize> {
    unlock_registered_cancellable(root, executable, || false)
}

pub fn unlock_registered_cancellable(
    root: &Path,
    executable: &Path,
    cancelled: impl Fn() -> bool,
) -> Result<usize> {
    let count = registered_count(root)?;
    for index in 0..count {
        if cancelled() {
            bail!("SSH loading cancelled; already-loaded identities may remain in the agent");
        }
        let mut command = Command::new(executable);
        command
            .arg(WORKER_COMMAND)
            .arg(root)
            .arg(index.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for name in [
            "LD_PRELOAD",
            "LD_LIBRARY_PATH",
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
        ] {
            command.env_remove(name);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .spawn()
            .map_err(|_| anyhow::anyhow!("Cannot start SSH key-loading worker"))?;
        let deadline = Instant::now() + Duration::from_secs(35);
        let success = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Err(_) => {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    break false;
                }
                _ => {}
            }
            if cancelled() || Instant::now() >= deadline {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if !success {
            bail!("SSH key loading failed: check the agent, unlocked OS keyring, saved passphrase and unchanged key. No interactive fallback or automatic retry was attempted");
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    #[derive(Default)]
    struct TestVault {
        values: RefCell<HashMap<String, String>>,
        fail_set: Cell<bool>,
        fail_delete: Cell<bool>,
        locked: Cell<bool>,
    }
    impl Vault for TestVault {
        fn set(&self, account: &str, value: &str) -> Result<()> {
            if self.fail_set.get() {
                bail!("test storage unavailable");
            }
            self.values
                .borrow_mut()
                .insert(account.into(), value.into());
            Ok(())
        }
        fn get(&self, account: &str) -> Result<Zeroizing<String>> {
            if self.locked.get() {
                bail!("test locked");
            }
            self.values
                .borrow()
                .get(account)
                .cloned()
                .map(Zeroizing::new)
                .ok_or_else(|| anyhow::anyhow!("missing"))
        }
        fn delete(&self, account: &str) -> Result<()> {
            if self.fail_delete.get() {
                bail!("test delete failed");
            }
            self.values.borrow_mut().remove(account);
            Ok(())
        }
    }
    fn key(root: &Path) -> PathBuf {
        let path = root.join("key");
        fs::write(&path, "test encrypted key bytes").unwrap();
        path
    }
    fn accept(_: &Path, _: Option<u32>, _: &str) -> Result<()> {
        Ok(())
    }
    #[test]
    fn default_lifetime_stays_optional_through_registration_and_reload() {
        let root = tempfile::tempdir().unwrap();
        let key = key(root.path());
        let vault = TestVault::default();
        remember_with(
            root.path(),
            &key,
            None,
            Zeroizing::new("test-secret".into()),
            &vault,
            |_, lifetime, _| {
                assert_eq!(lifetime, None);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(registered_keys(root.path()).unwrap()[0].lifetime_secs, None);
        unlock_with(root.path(), 0, &vault, |_, lifetime, _| {
            assert_eq!(lifetime, None);
            Ok(())
        })
        .unwrap();
        assert!(validate_lifetime(Some(0)).is_err());
        assert!(validate_lifetime(Some(86401)).is_err());
    }

    #[test]
    fn cancellation_after_keyring_write_never_enables_autoload() {
        let root = tempfile::tempdir().unwrap();
        let key = key(root.path());
        let vault = TestVault::default();
        let result = remember_with_cancellation(
            root.path(),
            &key,
            Some(28800),
            Zeroizing::new("sentinel".into()),
            &vault,
            accept,
            &|| !vault.values.borrow().is_empty(),
        );
        assert!(result.is_err());
        assert!(vault.values.borrow().is_empty());
        assert_eq!(registered_count(root.path()).unwrap(), 0);
    }

    #[test]
    fn cancelled_cleanup_failure_leaves_only_a_disabled_record() {
        let root = tempfile::tempdir().unwrap();
        let key = key(root.path());
        let vault = TestVault::default();
        vault.fail_delete.set(true);
        assert!(remember_with_cancellation(
            root.path(),
            &key,
            Some(28800),
            Zeroizing::new("sentinel".into()),
            &vault,
            accept,
            &|| !vault.values.borrow().is_empty(),
        )
        .is_err());
        let keys = registered_keys(root.path()).unwrap();
        assert_eq!(keys.len(), 1);
        assert!(!keys[0].active);
    }

    #[test]
    fn registration_roundtrips_without_storing_the_secret_in_metadata() {
        let root = tempfile::tempdir().unwrap();
        let key = key(root.path());
        let vault = TestVault::default();
        remember_with(
            root.path(),
            &key,
            Some(3600),
            Zeroizing::new("never-write-this-secret".into()),
            &vault,
            accept,
        )
        .unwrap();
        assert_eq!(registered_count(root.path()).unwrap(), 1);
        assert!(!fs::read_to_string(registry_path(root.path()))
            .unwrap()
            .contains("never-write-this-secret"));
        unlock_with(root.path(), 0, &vault, |actual, ttl, secret| {
            assert_eq!(actual, fs::canonicalize(&key).unwrap());
            assert_eq!(ttl, Some(3600));
            assert_eq!(secret, "never-write-this-secret");
            Ok(())
        })
        .unwrap();
        vault.locked.set(true);
        assert!(unlock_with(root.path(), 0, &vault, |_, _, _| panic!(
            "locked vault must not start ssh-add"
        ))
        .is_err());
        vault.locked.set(false);
        fs::write(&key, "replacement key").unwrap();
        assert!(unlock_with(root.path(), 0, &vault, |_, _, _| panic!(
            "changed key must not load"
        ))
        .is_err());
        // A changed or deleted key still permits removal of its original registration.
        fs::remove_file(&key).unwrap();
        forget_with(
            root.path(),
            &fs::canonicalize(root.path()).unwrap().join("key"),
            &vault,
        )
        .unwrap();
        assert_eq!(registered_count(root.path()).unwrap(), 0);
        assert!(vault.values.borrow().is_empty());
    }
    #[test]
    fn uncertain_saves_and_failed_deletes_keep_a_disabled_cleanup_record() {
        let root = tempfile::tempdir().unwrap();
        let key = key(root.path());
        let vault = TestVault::default();
        vault.fail_set.set(true);
        assert!(remember_with(
            root.path(),
            &key,
            Some(60),
            Zeroizing::new("test-secret".into()),
            &vault,
            accept
        )
        .is_err());
        assert_eq!(registered_count(root.path()).unwrap(), 0);
        assert_eq!(read_registry(root.path()).unwrap().keys.len(), 1);
        vault.fail_delete.set(true);
        assert!(forget_with(root.path(), &key, &vault).is_err());
        assert_eq!(registered_count(root.path()).unwrap(), 0);
        vault.fail_delete.set(false);
        forget_with(root.path(), &key, &vault).unwrap();
        assert!(read_registry(root.path()).unwrap().keys.is_empty());
    }
    #[test]
    fn registrations_are_namespaced_and_invalid_inputs_never_reach_storage() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let key = key(a.path());
        let vault = TestVault::default();
        for root in [a.path(), b.path()] {
            remember_with(
                root,
                &key,
                Some(60),
                Zeroizing::new("test-secret".into()),
                &vault,
                accept,
            )
            .unwrap();
        }
        assert_eq!(vault.values.borrow().len(), 2);
        assert!(remember_with(
            a.path(),
            &key,
            Some(0),
            Zeroizing::new("test-secret".into()),
            &vault,
            accept
        )
        .is_err());
        assert!(remember_with(
            a.path(),
            &key,
            Some(60),
            Zeroizing::new("two\nlines".into()),
            &vault,
            accept
        )
        .is_err());
        assert!(remember_with(
            a.path(),
            &key,
            Some(60),
            Zeroizing::new("test-secret".into()),
            &vault,
            accept
        )
        .is_err());
        assert_eq!(vault.values.borrow().len(), 2);
    }
    #[test]
    fn moving_registration_metadata_does_not_grant_access_but_still_allows_cleanup() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let key = key(a.path());
        let vault = TestVault::default();
        remember_with(
            a.path(),
            &key,
            Some(60),
            Zeroizing::new("test-secret".into()),
            &vault,
            accept,
        )
        .unwrap();
        fs::rename(a.path().join("auth"), b.path().join("auth")).unwrap();
        assert!(unlock_with(b.path(), 0, &vault, |_, _, _| panic!(
            "a moved product home must not load credentials"
        ))
        .is_err());
        forget_with(b.path(), &key, &vault).unwrap();
        assert!(vault.values.borrow().is_empty());
    }

    #[test]
    fn an_empty_setup_is_side_effect_free_and_unknown_formats_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("missing");
        assert_eq!(
            unlock_registered(&missing, Path::new("nonexistent-worker")).unwrap(),
            0
        );
        assert!(!missing.exists());
        let _lock = lock(root.path()).unwrap();
        fs::write(registry_path(root.path()), r#"{"version":99,"keys":[]}"#).unwrap();
        assert!(registered_count(root.path()).is_err());
    }
}
