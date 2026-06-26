#![cfg(unix)]

use crate::cache::PasswordCache;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use zeroize::Zeroizing;

/// Forwarded to the TUI event loop so it can show a password modal and reply.
pub struct AskpassPrompt {
    pub prompt: String,
    pub key: String,
    pub reply: tokio::sync::oneshot::Sender<Option<String>>,
}

/// Environment values the caller passes to child processes (sudo/ssh)
/// so they can find the server and authenticate.
#[derive(Clone)]
pub struct AskpassEnv {
    pub sock_path: PathBuf,
    pub token: String,
    /// Path to the wrapper script that sudo/ssh invoke as their ASKPASS helper.
    /// Populated by Task 10 before calling `set_env`; empty placeholder until then.
    pub askpass_script: PathBuf,
}

/// Removes the socket file when dropped.
pub struct AskpassServerGuard {
    sock_path: PathBuf,
}

impl Drop for AskpassServerGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn socket_path() -> io::Result<PathBuf> {
    let pid = std::process::id();
    let filename = format!("atomcode-askpass-{}.sock", pid);

    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return Ok(PathBuf::from(dir).join(filename));
    }

    let home = std::env::var("HOME")
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "HOME env var not set"))?;
    let dir = PathBuf::from(home).join(".atomcode").join("run");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(filename))
}

/// Generate a 32-char hex token from 16 random bytes (reads /dev/urandom;
/// intentionally avoids the `rand` crate).
fn gen_token() -> io::Result<String> {
    use std::io::Read as _;
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = [0u8; 16];
    f.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{:02x}", b)).collect())
}

// ── public API ────────────────────────────────────────────────────────────────

/// Map a password prompt string to a stable cache key.
///
/// - `[sudo]` or `Password:` prefix → `"sudo"`
/// - `<user>@<host>'s password:` → `"ssh:<host>"`
/// - anything else → `"generic"`
pub fn key_for_prompt(prompt: &str) -> String {
    if prompt.contains("[sudo]") || prompt.starts_with("Password:") {
        return "sudo".to_string();
    }
    if let Some(at_pos) = prompt.rfind('@') {
        let rest = &prompt[at_pos + 1..];
        if let Some(apos) = rest.find("'s password:") {
            let host = &rest[..apos];
            return format!("ssh:{}", host);
        }
    }
    "generic".to_string()
}

/// Handle a single client connection inside the accept loop.
async fn handle_connection(
    stream: tokio::net::UnixStream,
    expected_token: String,
    cache: Arc<PasswordCache>,
    tx: tokio::sync::mpsc::Sender<AskpassPrompt>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();

    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }

    let req: crate::protocol::Request = match serde_json::from_str(line.trim_end()) {
        Ok(r) => r,
        Err(_) => return,
    };

    // Drop silently on token mismatch.
    if req.token != expected_token {
        return;
    }

    let key = key_for_prompt(&req.prompt);
    let now = Instant::now();

    // Cache hit: respond immediately without queuing a prompt.
    if let Some(pw) = cache.get(&key, now) {
        let resp = crate::protocol::Response { password: Some(pw.as_str().to_string()) };
        if let Ok(s) = serde_json::to_string(&resp) {
            let _ = w.write_all(s.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
        }
        return;
    }

    // Cache miss: forward the prompt to the event loop via mpsc.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let prompt_msg = AskpassPrompt { prompt: req.prompt, key: key.clone(), reply: reply_tx };

    if tx.send(prompt_msg).await.is_err() {
        // No consumer; respond with None.
        let resp = crate::protocol::Response { password: None };
        if let Ok(s) = serde_json::to_string(&resp) {
            let _ = w.write_all(s.as_bytes()).await;
            let _ = w.write_all(b"\n").await;
        }
        return;
    }

    // Await the modal reply.
    let password = reply_rx.await.ok().flatten();

    if let Some(ref pw) = password {
        cache.put(&key, Zeroizing::new(pw.clone()), Instant::now());
    }

    let resp = crate::protocol::Response { password };
    if let Ok(s) = serde_json::to_string(&resp) {
        let _ = w.write_all(s.as_bytes()).await;
        let _ = w.write_all(b"\n").await;
    }
}

/// Bind a 0600 Unix-domain socket, generate an auth token, and spawn the
/// accept loop.  Returns:
/// - `AskpassEnv`  — set as env vars on child processes (sudo/ssh/…)
/// - `Receiver`    — the TUI event loop reads `AskpassPrompt`s from here
/// - `AskpassServerGuard` — removes the socket file on drop
///
/// Must be called from within a Tokio runtime context.
pub fn start(
    cache: Arc<PasswordCache>,
) -> io::Result<(AskpassEnv, tokio::sync::mpsc::Receiver<AskpassPrompt>, AskpassServerGuard)> {
    let sock_path = socket_path()?;

    // Remove any stale socket from a previous run.
    let _ = std::fs::remove_file(&sock_path);

    // Tighten umask so the socket is created 0600 from the start (no TOCTOU
    // window where group/world bits are briefly visible).
    let old_mask = unsafe { libc::umask(0o177) };
    let bind_result = tokio::net::UnixListener::bind(&sock_path);
    unsafe { libc::umask(old_mask) }; // always restore, even on error
    let listener = bind_result?;

    // Belt-and-suspenders: explicitly enforce 0600 regardless of umask.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))?;

    let token = gen_token()?;
    let (tx, rx) = tokio::sync::mpsc::channel::<AskpassPrompt>(32);

    let guard = AskpassServerGuard { sock_path: sock_path.clone() };
    let env = AskpassEnv { sock_path, token: token.clone(), askpass_script: std::path::PathBuf::new() };

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let token_c = token.clone();
                    let cache_c = cache.clone();
                    let tx_c = tx.clone();
                    tokio::spawn(handle_connection(stream, token_c, cache_c, tx_c));
                }
                Err(_) => break,
            }
        }
    });

    Ok((env, rx, guard))
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn key_for_prompt_classifies_sudo_and_ssh() {
        assert_eq!(key_for_prompt("[sudo] password for alice:"), "sudo");
        assert_eq!(key_for_prompt("alice@host.example.com's password:"), "ssh:host.example.com");
        assert_eq!(key_for_prompt("Enter passphrase for key '/x':"), "generic");
        // multi-@ (e.g. user@proxy@host): host must be the segment after the LAST '@'
        assert_eq!(key_for_prompt("user@proxy@host's password:"), "ssh:host");
    }

    #[tokio::test]
    async fn server_prompts_then_caches() {
        let cache = std::sync::Arc::new(crate::cache::PasswordCache::new(Duration::from_secs(300)));
        let (env, mut rx, _guard) = start(cache).unwrap();

        // Consumer (stands in for the event loop): answer the first prompt, then expect cache hit (no 2nd prompt).
        tokio::spawn(async move {
            if let Some(p) = rx.recv().await {
                let _ = p.reply.send(Some("secret".to_string()));
            }
            // If a second prompt arrives, fail by sending None — the test asserts only one came.
            if let Some(p) = rx.recv().await {
                let _ = p.reply.send(None);
            }
        });

        // Client #1: miss → prompt → "secret".
        let pw1 = client_ask(&env, "[sudo] password for x:").await;
        assert_eq!(pw1.as_deref(), Some("secret"));
        // Client #2: same sudo key → cache hit, no prompt.
        let pw2 = client_ask(&env, "[sudo] password for x:").await;
        assert_eq!(pw2.as_deref(), Some("secret"));
    }

    // Minimal in-test client mirroring the `__askpass` helper.
    async fn client_ask(env: &AskpassEnv, prompt: &str) -> Option<String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let stream = tokio::net::UnixStream::connect(&env.sock_path).await.unwrap();
        let (r, mut w) = stream.into_split();
        let req = serde_json::to_string(&crate::protocol::Request {
            token: env.token.clone(),
            prompt: prompt.to_string(),
        })
        .unwrap();
        w.write_all(req.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        w.flush().await.unwrap();
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await.unwrap();
        let resp: crate::protocol::Response = serde_json::from_str(line.trim_end()).unwrap();
        resp.password
    }
}
