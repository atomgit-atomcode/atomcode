//! Grape device consent and origin-bound private credentials. No model-visible secrets.
use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path, time::Duration};
use url::Url;

const FILE: &str = "grape-auth.json";
pub const DEFAULT_SERVER: &str = "https://www.grapedev.cn/";
pub const CLOUD_APP: &str = "https://www.grapedev.cn/app/";
pub const SCOPE: &str = "atomcode.integration.v1";
const LIMIT: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Connection {
    pub server: String,
    pub username: String,
    pub token_id: u64,
    secret: String,
}
impl Connection {
    /// Only HTTP adapters should call this. Never print or register in config.
    pub fn bearer(&self) -> &str {
        &self.secret
    }
    pub fn save(&self, user_dir: &Path) -> Result<()> {
        crate::write_auth_file_secure(&user_dir.join(FILE), &serde_json::to_string(self)?)
    }
}

pub fn server_url(raw: &str) -> Result<Url> {
    if raw.len() > 2048 {
        bail!("Grape URL is too long");
    }
    let mut url = Url::parse(raw).context("Use a complete Grape HTTPS URL")?;
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("Grape requires HTTPS (HTTP is allowed only for literal loopback); no credentials, query or fragment in its URL");
    }
    let path = format!("{}/", url.path().trim_end_matches('/'));
    url.set_path(&path);
    Ok(url)
}

pub fn load(user_dir: &Path) -> Result<Option<Connection>> {
    let path = user_dir.join(FILE);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.len() > 8192 {
        bail!("Invalid Grape credential file; reconnect");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            bail!("Grape credentials must be private (chmod 600)");
        }
    }
    let connection: Connection = serde_json::from_str(&std::fs::read_to_string(path)?)
        .map_err(|_| anyhow::anyhow!("Invalid Grape credentials; reconnect"))?;
    server_url(&connection.server)?;
    if !connection.secret.starts_with("grape_") || connection.secret.len() > 256 {
        bail!("Invalid Grape credentials; reconnect");
    }
    Ok(Some(connection))
}

pub fn forget(user_dir: &Path) -> Result<()> {
    match std::fs::remove_file(user_dir.join(FILE)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Serialize human connect/disconnect operations without locking normal readers.
pub struct Lease {
    _file: std::fs::File,
}
pub fn lease(user_dir: &Path) -> Result<Lease> {
    #[cfg(unix)]
    crate::ensure_private_dir(user_dir)?;
    #[cfg(not(unix))]
    std::fs::create_dir_all(user_dir)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(user_dir.join("grape-auth.lock"))?;
    fs2::FileExt::try_lock_exclusive(&file)
        .context("Another Grape connection operation is already in progress")?;
    Ok(Lease { _file: file })
}

fn client() -> Result<Client> {
    atomcode_config::proxy::ensure_runtime_initialized();
    let builder = if std::env::var(atomcode_config::proxy::MODE_ENV)
        .ok()
        .as_deref()
        == Some(atomcode_config::proxy::ProxyMode::NoProxy.as_str())
    {
        Client::builder().no_proxy()
    } else {
        Client::builder()
    };
    Ok(builder
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(crate::ATOMCODE_USER_AGENT)
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .build()?)
}
fn read<T: serde::de::DeserializeOwned>(response: reqwest::blocking::Response) -> Result<T> {
    let mut bytes = Vec::new();
    response.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        bail!("Grape response exceeds its limit");
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("Invalid Grape authorization response"))
}

#[derive(Deserialize)]
struct Grant {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}
pub struct Pending {
    server: Url,
    device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: u64,
    pub interval: u64,
}
pub enum Poll {
    Pending,
    SlowDown,
    Connected(Connection),
}

pub fn start(raw: &str) -> Result<Pending> {
    let server = server_url(raw)?;
    let response = client()?
        .post(server.join("api/auth/device/start")?)
        .json(&serde_json::json!({"client_id":"atomcode"}))
        .send()
        .context("Cannot reach Grape authorization service")?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        bail!("Grape authorization is not available on this service yet. The server must deploy AtomCode authorization support. Cloud login / beta application: {CLOUD_APP}");
    }
    if !response.status().is_success() {
        bail!(
            "Grape authorization unavailable (HTTP {})",
            response.status().as_u16()
        );
    }
    let grant: Grant = read(response)?;
    let verification = Url::parse(&grant.verification_uri_complete)?;
    // The server cannot redirect the browser (or leak a code) to another origin.
    let expected = server.join("api/auth/device/verify")?;
    if verification.origin() != server.origin()
        || verification.path() != expected.path()
        || verification.fragment().is_some()
        || !verification.username().is_empty()
        || verification.password().is_some()
        || verification.query_pairs().collect::<Vec<_>>()
            != vec![("user_code".into(), grant.user_code.as_str().into())]
        || grant.device_code.len() < 32
        || grant.device_code.len() > 128
        || grant.user_code.len() != 9
        || grant.user_code.as_bytes()[4] != b'-'
        || !grant
            .user_code
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || b.is_ascii_uppercase() || (b'2'..=b'9').contains(&b))
        || !(1..=600).contains(&grant.expires_in)
        || !(1..=60).contains(&grant.interval)
    {
        bail!("Grape returned an invalid authorization grant");
    }
    Ok(Pending {
        server,
        device_code: grant.device_code,
        user_code: grant.user_code,
        verification_url: verification.to_string(),
        expires_in: grant.expires_in,
        interval: grant.interval,
    })
}
impl Pending {
    pub fn poll(&self) -> Result<Poll> {
        let response = client()?
            .post(self.server.join("api/auth/device/token")?)
            .json(
                &serde_json::json!({"client_id":"atomcode","device_code":self.device_code,
                "grant_type":"urn:ietf:params:oauth:grant-type:device_code"}),
            )
            .send()
            .context("Grape authorization polling failed; the previous connection is unchanged")?;
        if response.status().as_u16() == 429 {
            return Ok(Poll::SlowDown);
        }
        if !response.status().is_success() {
            #[derive(Deserialize)]
            struct Failure {
                error: String,
            }
            let failure: Failure = read(response)?;
            return match failure.error.as_str() {
                "authorization_pending" => Ok(Poll::Pending),
                "slow_down" => Ok(Poll::SlowDown),
                "access_denied" => anyhow::bail!("Grape authorization was denied"),
                "expired_token" => {
                    anyhow::bail!("Grape authorization expired or was already consumed")
                }
                _ => anyhow::bail!("Grape authorization failed"),
            };
        }
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            token_type: String,
            scope: String,
            token_id: u64,
            username: String,
        }
        let token: Token = read(response)?;
        if token.token_type != "Bearer"
            || token.scope != SCOPE
            || token.token_id == 0
            || !token.access_token.starts_with("grape_")
            || token.access_token.len() > 256
            || !token
                .access_token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            || token.username.len() > 256
            || token.username.chars().any(char::is_control)
        {
            bail!("Grape returned an invalid credential");
        }
        Ok(Poll::Connected(Connection {
            server: self.server.to_string(),
            username: token.username,
            token_id: token.token_id,
            secret: token.access_token,
        }))
    }
}

pub fn disconnect(user_dir: &Path) -> Result<()> {
    let Some(connection) = load(user_dir)? else {
        return Ok(());
    };
    revoke(&connection)?;
    forget(user_dir)
}

/// Revoke an issued credential without changing any local connection.
pub fn revoke(connection: &Connection) -> Result<()> {
    let response = client()?
        .post(server_url(&connection.server)?.join("api/auth/device/revoke")?)
        .bearer_auth(connection.bearer())
        .send()
        .context("Grape revocation failed; local connection retained")?;
    if !response.status().is_success() && response.status().as_u16() != 401 {
        bail!(
            "Grape revocation failed (HTTP {}); local connection retained",
            response.status().as_u16()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn service_urls_are_origin_bound() {
        for url in [
            "http://example.com",
            "https://user:secret@example.com",
            "https://example.com/?token=x",
            "file:///tmp/auth",
            "https://example.com/#x",
        ] {
            assert!(server_url(url).is_err());
        }
        assert_eq!(
            server_url("https://example.com/grape").unwrap().as_str(),
            "https://example.com/grape/"
        );
        assert!(server_url("http://127.0.0.1:1234").is_ok());
    }
    #[test]
    fn credentials_are_private_and_not_in_config() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_none());
        Connection {
            server: "https://example.com/".into(),
            username: "test".into(),
            token_id: 1,
            secret: "grape_test".into(),
        }
        .save(dir.path())
        .unwrap();
        let stored = load(dir.path()).unwrap().unwrap();
        assert_eq!(stored.username, "test");
        assert_eq!(stored.bearer(), "grape_test");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join(FILE))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        forget(dir.path()).unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }
}

#[cfg(test)]
mod protocol_tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
    };
    // Real local sockets and a server thread: no virtual clock or live account.
    fn server(
        count: usize,
        respond: impl Fn(usize, &str, serde_json::Value, &str) -> (u16, serde_json::Value)
            + Send
            + 'static,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let url = base.clone();
        listener.set_nonblocking(true).unwrap();
        let thread = std::thread::spawn(move || {
            for index in 0..count {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(s) => break s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "expected HTTP request did not arrive"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(e) => panic!("{e}"),
                    }
                };
                // Accepted sockets inherit O_NONBLOCK on macOS; a read timeout
                // alone does not restore blocking reads for this test server.
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_string();
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let request = if body.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                };
                let (status, json) = respond(index, &path, request, &url);
                let body = json.to_string();
                write!(stream,"HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
            }
        });
        (base, thread)
    }
    fn grant(base: &str) -> serde_json::Value {
        serde_json::json!({"device_code":"device-secret-never-in-browser-1234567890","user_code":"ABCD-EFGH","verification_uri_complete":format!("{base}/api/auth/device/verify?user_code=ABCD-EFGH"),"expires_in":600,"interval":5})
    }
    #[test]
    fn device_exchange_saves_only_after_authorization_and_revokes_the_same_origin() {
        let (url, thread) = server(4, |index, path, body, base| match index {
            0 => {
                assert_eq!(path, "/api/auth/device/start");
                assert_eq!(body["client_id"], "atomcode");
                (200, grant(base))
            }
            1 => {
                assert_eq!(path, "/api/auth/device/token");
                assert_eq!(
                    body["device_code"],
                    "device-secret-never-in-browser-1234567890"
                );
                (400, serde_json::json!({"error":"authorization_pending"}))
            }
            2 => (
                200,
                serde_json::json!({"access_token":"grape_test_only","token_type":"Bearer","scope":SCOPE,"token_id":7,"username":"tester"}),
            ),
            _ => {
                assert_eq!(path, "/api/auth/device/revoke");
                (200, serde_json::json!({"revoked":true}))
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let pending = start(&url).unwrap();
        assert!(!pending.verification_url.contains("device-secret"));
        assert!(matches!(pending.poll().unwrap(), Poll::Pending));
        assert!(load(dir.path()).unwrap().is_none());
        let Poll::Connected(connection) = pending.poll().unwrap() else {
            panic!("expected successful exchange")
        };
        connection.save(dir.path()).unwrap();
        assert_eq!(load(dir.path()).unwrap().unwrap().token_id, 7);
        disconnect(dir.path()).unwrap();
        assert!(load(dir.path()).unwrap().is_none());
        thread.join().unwrap();
    }
    #[test]
    fn older_services_explain_the_missing_authorization_support() {
        let (url, thread) = server(1, |_, _, _, _| {
            (404, serde_json::json!({"detail":"Not Found"}))
        });
        let error = start(&url)
            .err()
            .expect("old service must fail")
            .to_string();
        assert!(error.contains("server must deploy"));
        assert!(error.contains(CLOUD_APP));
        thread.join().unwrap();
    }
    #[test]
    fn an_untrusted_verification_origin_does_not_replace_an_existing_connection() {
        let (url, thread) = server(1, |_, _, _, base| {
            let mut g = grant(base);
            g["verification_uri_complete"] = serde_json::json!(
                "https://other.example/api/auth/device/verify?user_code=ABCD-EFGH"
            );
            (200, g)
        });
        let dir = tempfile::tempdir().unwrap();
        Connection {
            server: "https://previous.example/".into(),
            username: "previous".into(),
            token_id: 1,
            secret: "grape_previous".into(),
        }
        .save(dir.path())
        .unwrap();
        assert!(start(&url).is_err());
        assert_eq!(load(dir.path()).unwrap().unwrap().username, "previous");
        thread.join().unwrap();
    }
    #[test]
    fn denied_authorization_preserves_the_old_credential_and_poll_backoff_is_explicit() {
        let (url, thread) = server(4, |i, _, _, base| match i {
            0 => (200, grant(base)),
            1 => (400, serde_json::json!({"error":"slow_down"})),
            2 => (429, serde_json::json!({"detail":"limited"})),
            _ => (400, serde_json::json!({"error":"access_denied"})),
        });
        let dir = tempfile::tempdir().unwrap();
        Connection {
            server: "https://previous.example/".into(),
            username: "previous".into(),
            token_id: 1,
            secret: "grape_previous".into(),
        }
        .save(dir.path())
        .unwrap();
        let pending = start(&url).unwrap();
        assert!(matches!(pending.poll().unwrap(), Poll::SlowDown));
        assert!(matches!(pending.poll().unwrap(), Poll::SlowDown));
        assert!(pending.poll().is_err());
        assert_eq!(
            load(dir.path()).unwrap().unwrap().bearer(),
            "grape_previous"
        );
        thread.join().unwrap();
    }
    #[test]
    fn simultaneous_connection_operations_do_not_race() {
        let dir = tempfile::tempdir().unwrap();
        let held = lease(dir.path()).unwrap();
        assert!(lease(dir.path()).is_err());
        drop(held);
        assert!(lease(dir.path()).is_ok());
    }
}
