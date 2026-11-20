use anyhow::{bail, Result};
use zeroize::Zeroizing;

pub(super) trait Vault {
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    fn get(&self, account: &str) -> Result<Zeroizing<String>>;
    fn delete(&self, account: &str) -> Result<()>;
}
pub(super) struct SystemVault;
const SERVICE: &str = "ssh-agent-passphrase";

#[cfg(any(target_os = "macos", windows))]
impl Vault for SystemVault {
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        keyring::Entry::new(SERVICE, account)
            .and_then(|entry| entry.set_password(secret))
            .map_err(|_| anyhow::anyhow!("Cannot save SSH passphrase in the OS keyring"))
    }
    fn get(&self, account: &str) -> Result<Zeroizing<String>> {
        // Only called in the isolated worker, never in the live host. This API
        // changes process-wide Keychain interaction policy on macOS.
        #[cfg(target_os = "macos")]
        let _no_ui =
            security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
                .map_err(|_| anyhow::anyhow!("Cannot disable keyring interaction"))?;
        keyring::Entry::new(SERVICE, account)
            .and_then(|entry| entry.get_password())
            .map(Zeroizing::new)
            .map_err(|_| {
                anyhow::anyhow!("OS keyring is locked, unavailable, or has no saved passphrase")
            })
    }
    fn delete(&self, account: &str) -> Result<()> {
        match keyring::Entry::new(SERVICE, account).and_then(|entry| entry.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => bail!("Cannot delete SSH passphrase from the OS keyring"),
        }
    }
}

#[cfg(target_os = "linux")]
fn secret_tool(args: &[&str], input: Option<&str>) -> Result<Zeroizing<Vec<u8>>> {
    use std::{
        io::{Read, Write},
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let program = ["/usr/bin/secret-tool", "/bin/secret-tool"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
        .ok_or_else(|| {
            anyhow::anyhow!("Secret Service requires the system libsecret secret-tool utility")
        })?;
    let mut command = Command::new(program);
    command.env_clear().env("PATH", "/usr/bin:/bin");
    // Preserve the user's OS credential-store session, not loader/module hooks.
    for name in [
        "HOME",
        "USER",
        "LOGNAME",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "XDG_CURRENT_DESKTOP",
        "DISPLAY",
        "WAYLAND_DISPLAY",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let mut child = command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| anyhow::anyhow!("Cannot start system Secret Service client"))?;
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Zeroizing::new(Vec::new());
        stdout
            .by_ref()
            .take(32769)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    if let Some(input) = input {
        if child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .is_err()
        {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Cannot provide passphrase to Secret Service");
        }
    } else {
        drop(child.stdin.take());
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    let success = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Err(_) => break false,
            _ => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let bytes = reader
        .join()
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| anyhow::anyhow!("Invalid Secret Service response"))?;
    if !success || bytes.len() > 32768 {
        bail!("Secret Service is unavailable, locked, or returned an invalid response");
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
impl Vault for SystemVault {
    fn set(&self, account: &str, secret: &str) -> Result<()> {
        secret_tool(
            &[
                "store",
                "--label=SSH agent key passphrase",
                "service",
                SERVICE,
                "account",
                account,
            ],
            Some(secret),
        )?;
        Ok(())
    }
    fn get(&self, account: &str) -> Result<Zeroizing<String>> {
        // Unlike lookup, search without --unlock never requests unlocking a
        // locked item. No output is exposed outside this worker.
        let bytes = secret_tool(
            &["search", "--all", "service", SERVICE, "account", account],
            None,
        )?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid stored passphrase"))?;
        let mut values = text
            .lines()
            .filter_map(|line| line.strip_prefix("secret = "));
        let value = values
            .next()
            .ok_or_else(|| anyhow::anyhow!("Secret Service item is locked or missing"))?;
        if values.next().is_some() {
            bail!("Ambiguous Secret Service entries");
        }
        Ok(Zeroizing::new(value.to_owned()))
    }
    fn delete(&self, account: &str) -> Result<()> {
        secret_tool(&["clear", "service", SERVICE, "account", account], None)?;
        Ok(())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
impl Vault for SystemVault {
    fn set(&self, _: &str, _: &str) -> Result<()> {
        bail!("OS keyring unsupported on this platform")
    }
    fn get(&self, _: &str) -> Result<Zeroizing<String>> {
        bail!("OS keyring unsupported on this platform")
    }
    fn delete(&self, _: &str) -> Result<()> {
        bail!("OS keyring unsupported on this platform")
    }
}
