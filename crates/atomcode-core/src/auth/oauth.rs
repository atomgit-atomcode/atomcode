use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Deserializer, Serialize};

/// Deserialize an ID that may be a string or a number into a String.
fn deserialize_id_as_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrNum {
        Str(String),
        Int(i64),
    }
    match StringOrNum::deserialize(deserializer)? {
        StringOrNum::Str(s) => Ok(s),
        StringOrNum::Int(n) => Ok(n.to_string()),
    }
}

/// URL encode a string
fn urlencoding_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// AtomGit OAuth configuration
pub const CLIENT_ID: &str = "b9956e5327e544578128af8979ba3ccb";
pub const CLIENT_SECRET: &str = "756ef00061884c7aa1ac64bd4eae3be7";
pub const REDIRECT_PORT: u16 = 8765;
pub const REDIRECT_URI: &str = "http://127.0.0.1:8765/callback";

/// AtomGit OAuth endpoints
pub const AUTHORIZE_URL: &str = "https://atomgit.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://atomgit.com/oauth/token";
pub const USER_URL: &str = "https://atomgit.com/api/v5/user";

/// OAuth scopes needed
pub const SCOPES: &str = "user_info projects";

/// Stored authentication data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthInfo {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_in: Option<i64>,
    /// Unix timestamp (seconds) when this token was obtained
    #[serde(default)]
    pub created_at: i64,
    pub user: UserInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub username: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct UserResponse {
    #[serde(deserialize_with = "deserialize_id_as_string")]
    id: String,
    login: String,
    name: Option<String>,
    email: Option<String>,
    avatar_url: Option<String>,
}

/// Perform OAuth login flow
pub fn login() -> Result<AuthInfo> {
    println!("\n  AtomCode Login");
    println!("  ==============\n");

    // Generate random state for CSRF protection
    let state = generate_state();

    // Build authorization URL
    let auth_url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&state={}&scope={}",
        AUTHORIZE_URL,
        urlencoding_encode(CLIENT_ID),
        urlencoding_encode(REDIRECT_URI),
        state,
        urlencoding_encode(SCOPES),
    );

    println!("  Opening browser for authorization...");
    println!("  If browser doesn't open, visit this URL:\n");
    println!("  {}\n", auth_url);

    // Open browser (best-effort — paste path below covers headless / WSL).
    if let Err(e) = open_browser(&auth_url) {
        println!("  Failed to open browser: {}", e);
        println!("  (paste path below will still work)\n");
    }

    let (code, returned_state) = await_callback(REDIRECT_PORT)?;

    // Verify state. Most common cause of mismatch in practice is the user
    // pasting a callback URL left over from an earlier /login attempt;
    // re-running /login regenerates state and fixes it.
    if returned_state != state {
        anyhow::bail!(
            "OAuth state mismatch — the pasted URL likely came from an earlier \
            /login attempt. Re-run /login and paste the newly-authorized URL."
        );
    }

    println!("  Authorization received, exchanging token...\n");

    // Exchange code for token
    let token = exchange_code_for_token(&code)?;

    // Get user info
    let user = get_user_info(&token.access_token)?;

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let auth_info = AuthInfo {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        token_type: token.token_type.unwrap_or_else(|| "Bearer".to_string()),
        expires_in: token.expires_in,
        created_at,
        user: UserInfo {
            id: user.id,
            username: user.login,
            name: user.name,
            email: user.email,
            avatar_url: user.avatar_url,
        },
    };

    println!(
        "  Logged in as: {} ({})\n",
        auth_info.user.username, auth_info.user.id
    );

    Ok(auth_info)
}

/// Generate random state string for CSRF protection
fn generate_state() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("atomcode_{}", timestamp)
}

/// Open browser with the authorization URL
#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_browser(url: &str) -> Result<()> {
    std::process::Command::new("xdg-open")
        .arg(url)
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new("cmd")
        .raw_arg(format!("/C start \"\" \"{}\"", url))
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn open_browser(_url: &str) -> Result<()> {
    anyhow::bail!("Unsupported platform for browser auto-open");
}

/// Race a local TCP listener against stdin paste; return the first
/// `(code, state)` that arrives. Listener handles the normal desktop path
/// where the browser hits `127.0.0.1:8765`; stdin path handles WSL /
/// headless Linux where the user copies the callback URL from their
/// browser's address bar and pastes it in.
fn await_callback(port: u16) -> Result<(String, String)> {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => Some(l),
        Err(e) => {
            println!("  Could not bind port {} ({}). Paste path only.", port, e);
            None
        }
    };

    println!(
        "  Waiting for callback on http://127.0.0.1:{}/callback",
        port
    );
    println!("  Or paste the full callback URL here and press Enter:");
    println!("  (Ctrl+C to cancel)\n");

    let (tx, rx) = mpsc::channel::<Result<(String, String)>>();
    let stop = Arc::new(AtomicBool::new(false));

    if let Some(listener) = listener {
        let tx_l = tx.clone();
        let stop_l = Arc::clone(&stop);
        thread::spawn(move || {
            let r = accept_callback_until_stopped(listener, &stop_l);
            let _ = tx_l.send(r);
        });
    }

    // Stdin reader. `read_line` is blocking and cannot be cancelled
    // cross-platform, so we spawn and let it die with the process if the
    // listener wins. Send on a closed channel is silently dropped.
    thread::spawn(move || {
        let stdin = io::stdin();
        let mut line = String::new();
        let r = match stdin.lock().read_line(&mut line) {
            Ok(0) => Err(anyhow::anyhow!("stdin closed")),
            Ok(_) => parse_pasted_callback(&line),
            Err(e) => Err(anyhow::Error::new(e).context("Failed to read from stdin")),
        };
        let _ = tx.send(r);
    });

    let result = rx.recv().context("login cancelled")?;
    stop.store(true, Ordering::Relaxed);
    result
}

/// Accept a single OAuth callback on an already-bound listener, polling a
/// `stop` flag every 200ms so the caller can cancel (e.g. when the paste
/// path won the race).
fn accept_callback_until_stopped(
    listener: TcpListener,
    stop: &AtomicBool,
) -> Result<(String, String)> {
    listener
        .set_nonblocking(true)
        .context("Failed to set non-blocking mode")?;

    let mut stream = loop {
        if stop.load(Ordering::Relaxed) {
            anyhow::bail!("listener cancelled");
        }
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e).context("Failed to accept connection"),
        }
    };

    stream.set_nonblocking(false)?;

    // Read HTTP request
    let mut reader = io::BufReader::new(&mut stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    // Parse the request line (GET /callback?code=...&state=... HTTP/1.1)
    let url: String = request_line
        .split_whitespace()
        .nth(1)
        .context("Invalid HTTP request")?
        .to_string();

    // Parse query parameters
    let query_start = url.find('?').context("No query parameters in callback")?;
    let query = &url[query_start + 1..];

    let params: HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts
                .next()
                .map(|v| urlencoding_decode(v))
                .unwrap_or_default();
            Some((key.to_string(), value))
        })
        .collect();

    // Check for error — redirect browser to AtomGit
    if let Some(error) = params.get("error") {
        let error_desc = params
            .get("error_description")
            .map(|s| s.as_str())
            .unwrap_or(error);
        let response = "HTTP/1.1 302 Found\r\nLocation: https://atomgit.com\r\n\r\n";
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
        anyhow::bail!("OAuth error: {}", error_desc);
    }

    let code = params.get("code").context("No code in callback")?.clone();
    let state = params.get("state").cloned().unwrap_or_default();

    // Send success response to browser
    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
        <html><head><title>AtomCode Login</title>\
        <style>body{font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#1a1a2e;color:#eee}\
        .container{text-align:center;padding:2rem}h1{color:#7c3aed;margin:0}p{color:#888}\
        .success{color:#22c55e;font-size:4rem}</style></head>\
        <body><div class=\"container\">\
        <div class=\"success\">✓</div>\
        <h1>Authorization Successful</h1>\
        <p>You can close this window and return to AtomCode.</p>\
        </div></body></html>";

    stream.write_all(response.as_bytes())?;
    stream.flush()?;

    Ok((code, state))
}

/// Simple URL decoding
fn urlencoding_decode(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            }
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }

    result
}

/// Exchange authorization code for access token
fn exchange_code_for_token(code: &str) -> Result<TokenResponse> {
    let client = reqwest::blocking::Client::new();

    let params = [
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("code", code),
        ("redirect_uri", REDIRECT_URI),
        ("grant_type", "authorization_code"),
    ];

    let response = client
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .context("Failed to send token request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        anyhow::bail!("Token request failed ({}): {}", status, body);
    }

    response
        .json::<TokenResponse>()
        .context("Failed to parse token response")
}

/// Get user information using access token
fn get_user_info(access_token: &str) -> Result<UserResponse> {
    let client = reqwest::blocking::Client::new();

    let response = client
        .get(USER_URL)
        .bearer_auth(access_token)
        .send()
        .context("Failed to get user info")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        anyhow::bail!("User info request failed ({}): {}", status, body);
    }

    response
        .json::<UserResponse>()
        .context("Failed to parse user response")
}

/// Refresh the access token using the stored refresh_token.
/// Returns updated AuthInfo with new tokens, and saves it to disk.
#[allow(dead_code)]
pub fn refresh_access_token(auth: &AuthInfo) -> Result<AuthInfo> {
    let refresh_token = auth
        .refresh_token
        .as_deref()
        .context("No refresh_token available — please /login again")?;

    let client = reqwest::blocking::Client::new();
    let params = [
        ("client_id", CLIENT_ID),
        ("client_secret", CLIENT_SECRET),
        ("refresh_token", refresh_token),
        ("grant_type", "refresh_token"),
    ];

    let response = client
        .post(TOKEN_URL)
        .form(&params)
        .send()
        .context("Failed to send refresh token request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        anyhow::bail!(
            "Token refresh failed ({}): {} — please /login again",
            status,
            body
        );
    }

    let token: TokenResponse = response
        .json()
        .context("Failed to parse refresh token response")?;

    let created_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let new_auth = AuthInfo {
        access_token: token.access_token,
        refresh_token: token.refresh_token.or_else(|| auth.refresh_token.clone()),
        token_type: token.token_type.unwrap_or_else(|| auth.token_type.clone()),
        expires_in: token.expires_in.or(auth.expires_in),
        created_at,
        user: auth.user.clone(),
    };

    save_auth(&new_auth)?;
    Ok(new_auth)
}

/// Get a valid access token, refreshing automatically if expired.
/// Returns the access token string ready to use.
#[allow(dead_code)]
pub fn get_valid_token() -> Result<String> {
    let auth = get_stored_auth().context("Not logged in — please use /login first")?;

    // Check if token is expired (with 5-minute safety margin)
    if let Some(expires_in) = auth.expires_in {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let expires_at = auth.created_at + expires_in;

        if now >= expires_at - 300 {
            // Token expired or about to expire — try refresh
            match refresh_access_token(&auth) {
                Ok(new_auth) => return Ok(new_auth.access_token),
                Err(e) => anyhow::bail!("Token expired and refresh failed: {}", e),
            }
        }
    } else if auth.created_at == 0 {
        // Legacy auth.toml without created_at — no way to know if expired,
        // try refresh if refresh_token is available, otherwise use as-is
        if auth.refresh_token.is_some() {
            if let Ok(new_auth) = refresh_access_token(&auth) {
                return Ok(new_auth.access_token);
            }
        }
    }

    Ok(auth.access_token)
}

/// Logout - clear stored auth
pub fn logout() -> Result<()> {
    let auth_path = auth_file_path();
    if auth_path.exists() {
        std::fs::remove_file(&auth_path).context("Failed to remove auth file")?;
        println!("  Logged out successfully.\n");
    } else {
        println!("  No active session found.\n");
    }
    Ok(())
}

/// Get stored auth info
pub fn get_stored_auth() -> Option<AuthInfo> {
    let auth_path = auth_file_path();
    if !auth_path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&auth_path).ok()?;
    toml::from_str(&content).ok()
}

/// Save auth info to file
pub fn save_auth(auth: &AuthInfo) -> Result<()> {
    let auth_path = auth_file_path();

    // Ensure parent directory exists
    if let Some(parent) = auth_path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create auth directory")?;
    }

    let content = toml::to_string_pretty(auth).context("Failed to serialize auth info")?;

    std::fs::write(&auth_path, content).context("Failed to write auth file")?;

    println!("  Auth saved to: {}\n", auth_path.display());

    Ok(())
}

/// Get path to auth file
pub fn auth_file_path() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".atomcode")
        .join("auth.toml")
}

/// Check if user is logged in
#[allow(dead_code)]
pub fn is_logged_in() -> bool {
    get_stored_auth().is_some()
}

/// Get current user info (if logged in)
#[allow(dead_code)]
pub fn current_user() -> Option<UserInfo> {
    get_stored_auth().map(|auth| auth.user)
}

/// Parse a user-pasted OAuth callback URL into (code, state).
///
/// Accepts any URL with a query string containing `code` and `state`.
/// Rejects raw `code` without URL context — state validation is CSRF
/// protection and we want the full round-trip, not a manually typed code.
fn parse_pasted_callback(input: &str) -> Result<(String, String)> {
    // Defensively strip bracketed-paste markers. The TUI disables DECSET
    // 2004 before calling us, but a user pasting into a terminal we didn't
    // configure (or with a stray prior session) can still deliver these.
    let cleaned = input
        .trim()
        .trim_start_matches("\x1b[200~")
        .trim_end_matches("\x1b[201~")
        .trim();

    let query_start = cleaned.find('?').context(
        "Could not parse callback URL — paste the full http://127.0.0.1:8765/callback?... URL",
    )?;
    let query = &cleaned[query_start + 1..];

    let params: HashMap<String, String> = query
        .split('&')
        .filter_map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts
                .next()
                .map(|v| urlencoding_decode(v))
                .unwrap_or_default();
            Some((key.to_string(), value))
        })
        .collect();

    if let Some(error) = params.get("error") {
        let desc = params
            .get("error_description")
            .map(|s| s.as_str())
            .unwrap_or(error);
        anyhow::bail!("OAuth error: {}", desc);
    }

    let code = params
        .get("code")
        .context("Callback URL missing 'code' parameter")?
        .clone();
    let state = params
        .get("state")
        .context("Callback URL missing 'state' parameter (paste the full URL, not just the code)")?
        .clone();

    Ok((code, state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_happy_path_loopback_url() {
        let (code, state) =
            parse_pasted_callback("http://127.0.0.1:8765/callback?code=abc&state=xyz").unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_any_host_with_extra_params() {
        let (code, state) =
            parse_pasted_callback("https://example.com/x?foo=1&code=abc&state=xyz&bar=2").unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_missing_state_errors_with_full_url_hint() {
        let err = parse_pasted_callback("http://127.0.0.1:8765/callback?code=abc")
            .unwrap_err()
            .to_string();
        assert!(err.contains("state"), "got: {err}");
        assert!(err.contains("full URL"), "got: {err}");
    }

    #[test]
    fn parse_missing_code_errors() {
        let err = parse_pasted_callback("http://127.0.0.1:8765/callback?state=xyz")
            .unwrap_err()
            .to_string();
        assert!(err.contains("code"), "got: {err}");
    }

    #[test]
    fn parse_error_response_includes_description() {
        let err = parse_pasted_callback(
            "http://127.0.0.1:8765/callback?error=access_denied&error_description=User+denied",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("User denied"), "got: {err}");
    }

    #[test]
    fn parse_not_a_url_errors() {
        let err = parse_pasted_callback("this is not a url")
            .unwrap_err()
            .to_string();
        assert!(err.contains("full"), "got: {err}");
    }

    #[test]
    fn parse_url_encoded_state_is_decoded() {
        let (_, state) =
            parse_pasted_callback("http://127.0.0.1:8765/callback?code=c&state=atomcode_%3Atest")
                .unwrap();
        assert_eq!(state, "atomcode_:test");
    }

    #[test]
    fn parse_strips_bracketed_paste_markers() {
        let input = "\x1b[200~http://127.0.0.1:8765/callback?code=abc&state=xyz\x1b[201~";
        let (code, state) = parse_pasted_callback(input).unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }

    #[test]
    fn parse_trims_surrounding_whitespace() {
        let (code, state) =
            parse_pasted_callback("   http://127.0.0.1:8765/callback?code=abc&state=xyz\n")
                .unwrap();
        assert_eq!(code, "abc");
        assert_eq!(state, "xyz");
    }
}
