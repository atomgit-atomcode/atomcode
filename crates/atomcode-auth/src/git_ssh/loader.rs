use anyhow::{bail, Result};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

pub(super) fn ssh_add() -> Result<PathBuf> {
    #[cfg(windows)]
    let candidates = vec![super::agent::system_directory()?.join("OpenSSH/ssh-add.exe")];
    #[cfg(not(windows))]
    let candidates = vec![
        PathBuf::from("/usr/bin/ssh-add"),
        PathBuf::from("/bin/ssh-add"),
    ];
    candidates
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow::anyhow!("System OpenSSH ssh-add is unavailable"))
}

// No public API accepts an executable: only the fixed system OpenSSH binary
// receives the secret. No output, prompt text or secret is forwarded to callers.
pub(super) fn load(key: &Path, lifetime: Option<u32>, passphrase: &str) -> Result<()> {
    load_at(&ssh_add()?, key, lifetime, passphrase, None)
}

fn load_at(
    program: &Path,
    key: &Path,
    lifetime: Option<u32>,
    passphrase: &str,
    socket: Option<&Path>,
) -> Result<()> {
    super::agent::verify(socket)?;
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|_| anyhow::anyhow!("Cannot create a private SSH key-loading terminal"))?;
    let mut cmd = CommandBuilder::new(program);
    // No preload hooks, provider selectors, or inherited askpass helpers may
    // execute inside a process that receives a saved passphrase.
    cmd.env_clear();
    for name in [
        "HOME",
        "USERPROFILE",
        "TMPDIR",
        "TMP",
        "TEMP",
        "USER",
        "LOGNAME",
    ] {
        if let Some(value) = std::env::var_os(name) {
            cmd.env(name, value);
        }
    }
    #[cfg(unix)]
    {
        cmd.env("PATH", "/usr/bin:/bin");
        if let Some(value) = std::env::var_os("SSH_AUTH_SOCK") {
            cmd.env("SSH_AUTH_SOCK", value);
        }
    }
    #[cfg(windows)]
    {
        let system = super::agent::system_directory()?;
        cmd.env(
            "SystemRoot",
            system
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Invalid system directory"))?,
        );
        cmd.env(
            "PATH",
            std::env::join_paths([system.join("OpenSSH"), system])
                .map_err(|_| anyhow::anyhow!("Invalid system path"))?,
        );
    }
    if let Some(lifetime) = lifetime {
        cmd.args(["-t", &lifetime.to_string()]);
    }
    cmd.arg(key);
    cmd.env("LC_ALL", "C");
    cmd.env("SSH_ASKPASS_REQUIRE", "never");
    cmd.env_remove("SSH_ASKPASS");
    cmd.env_remove("DISPLAY");
    if let Some(socket) = socket {
        cmd.env("SSH_AUTH_SOCK", socket);
    }
    let mut reader = pty
        .master
        .try_clone_reader()
        .map_err(|_| anyhow::anyhow!("Cannot read key-loading terminal"))?;
    let mut writer = pty
        .master
        .take_writer()
        .map_err(|_| anyhow::anyhow!("Cannot write key-loading terminal"))?;
    let mut child = pty
        .slave
        .spawn_command(cmd)
        .map_err(|_| anyhow::anyhow!("Cannot start system ssh-add"))?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Zeroizing::new([0u8; 1024]);
        let mut tail = Zeroizing::new(Vec::new());
        while let Ok(n) = reader.read(&mut *bytes) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&bytes[..n]);
            if tail
                .windows(b"Bad passphrase".len())
                .any(|s| s == b"Bad passphrase")
            {
                let _ = tx.send(false);
                tail.clear();
            }
            if tail
                .windows(b"Enter passphrase".len())
                .any(|s| s == b"Enter passphrase")
                && tail.ends_with(b": ")
            {
                let _ = tx.send(true);
                tail.clear();
            }
            if tail.len() > 8192 {
                let keep = tail.len() - 4096;
                tail.drain(..keep);
            }
        }
    });
    // Start draining before closing the slave: macOS can wait for pending PTY
    // output while a process exits. Keep draining even after rejection/kill.
    drop(pty.slave);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut sent = false;
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                break if status.success() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "ssh-add rejected the key or could not reach the agent"
                    ))
                }
            }
            Err(_) => break Err(anyhow::anyhow!("Cannot inspect ssh-add completion")),
            _ => {}
        }
        match rx.try_recv() {
            Ok(true) if !sent => {
                sent = true;
                if writer
                    .write_all(passphrase.as_bytes())
                    .and_then(|_| writer.write_all(b"\n"))
                    .and_then(|_| writer.flush())
                    .is_err()
                {
                    break Err(anyhow::anyhow!("Cannot supply SSH passphrase"));
                }
            }
            Ok(_) => {
                break Err(anyhow::anyhow!(
                    "SSH passphrase was rejected; no automatic retry"
                ))
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            break Err(anyhow::anyhow!("SSH key loading timed out"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    drop(writer);
    drop(pty.master);
    result
}

pub(super) fn validate_passphrase(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 4096 || value.contains(['\r', '\n', '\0']) {
        bail!("Passphrase must be a nonempty single line of at most 4096 bytes");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    struct Agent(std::process::Child);
    impl Drop for Agent {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    fn encrypted_key_loads_into_an_isolated_agent_and_bad_passphrases_fail() {
        let root = tempfile::tempdir().unwrap();
        let key = root.path().join("key");
        assert!(Command::new("/usr/bin/ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "test-only-secret", "-f"])
            .arg(&key)
            .status()
            .unwrap()
            .success());
        let socket = root.path().join("agent.sock");
        let _agent = Agent(
            Command::new("/usr/bin/ssh-agent")
                .args(["-D", "-a"])
                .arg(&socket)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(socket.exists());
        let program = ssh_add().unwrap();
        assert!(load_at(&program, &key, Some(60), "wrong-secret", Some(&socket)).is_err());
        load_at(&program, &key, Some(60), "test-only-secret", Some(&socket)).unwrap();
        load_at(&program, &key, None, "test-only-secret", Some(&socket)).unwrap();
        assert!(Command::new(program)
            .arg("-T")
            .arg(key.with_extension("pub"))
            .env("SSH_AUTH_SOCK", &socket)
            .status()
            .unwrap()
            .success());
    }
}
