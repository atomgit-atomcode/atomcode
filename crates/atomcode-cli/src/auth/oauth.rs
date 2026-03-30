use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::net::TcpListener;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
    pub user: UserInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: i64,
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
    id: i64,
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
    
    // Open browser
    if let Err(e) = open_browser(&auth_url) {
        println!("  Failed to open browser: {}", e);
        println!("  Please visit the URL above manually.\n");
    }
    
    // Start local server to receive callback
    println!("  Waiting for authorization callback on port {}...\n", REDIRECT_PORT);
    
    let (code, returned_state) = receive_callback(REDIRECT_PORT)?;
    
    // Verify state
    if returned_state != state {
        anyhow::bail!("OAuth state mismatch - possible CSRF attack");
    }
    
    println!("  Authorization received, exchanging token...\n");
    
    // Exchange code for token
    let token = exchange_code_for_token(&code)?;
    
    // Get user info
    let user = get_user_info(&token.access_token)?;
    
    let auth_info = AuthInfo {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        token_type: token.token_type.unwrap_or_else(|| "Bearer".to_string()),
        expires_in: token.expires_in,
        user: UserInfo {
            id: user.id,
            username: user.login,
            name: user.name,
            email: user.email,
            avatar_url: user.avatar_url,
        },
    };
    
    println!("  Logged in as: {} ({})\n", auth_info.user.username, auth_info.user.id);
    
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
    std::process::Command::new("cmd")
        .args(["/C", "start", url])
        .spawn()
        .context("Failed to open browser")?;
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn open_browser(_url: &str) -> Result<()> {
    anyhow::bail!("Unsupported platform for browser auto-open");
}

/// Receive OAuth callback on local server
fn receive_callback(port: u16) -> Result<(String, String)> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("Failed to bind to port {}. Is it already in use?", port))?;
    
    // Accept first connection
    let (mut stream, _) = listener.accept()
        .context("Failed to accept connection")?;
    
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
            let value = parts.next().map(|v| urlencoding_decode(v)).unwrap_or_default();
            Some((key.to_string(), value))
        })
        .collect();
    
    // Check for error
    if let Some(error) = params.get("error") {
        let error_desc = params.get("error_description").map(|s| s.as_str()).unwrap_or(error);
        anyhow::bail!("OAuth error: {}", error_desc);
    }
    
    let code = params.get("code").context("No code in callback")?.clone();
    let state = params.get("state").cloned().unwrap_or_default();
    
    // Send response to browser
    let response = if params.contains_key("code") {
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
        <html><head><title>AtomCode Login</title>\
        <style>body{font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#1a1a2e;color:#eee}\
        .container{text-align:center;padding:2rem}h1{color:#7c3aed;margin:0}p{color:#888}\
        .success{color:#22c55e;font-size:4rem}</style></head>\
        <body><div class=\"container\">\
        <div class=\"success\">✓</div>\
        <h1>Authorization Successful</h1>\
        <p>You can close this window and return to AtomCode.</p>\
        </div></body></html>"
    } else {
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html\r\n\r\n\
        <html><body><h1>Bad Request</h1></body></html>"
    };
    
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
    
    response.json::<TokenResponse>()
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
    
    response.json::<UserResponse>()
        .context("Failed to parse user response")
}

/// Logout - clear stored auth
pub fn logout() -> Result<()> {
    let auth_path = auth_file_path();
    if auth_path.exists() {
        std::fs::remove_file(&auth_path)
            .context("Failed to remove auth file")?;
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
        std::fs::create_dir_all(parent)
            .context("Failed to create auth directory")?;
    }
    
    let content = toml::to_string_pretty(auth)
        .context("Failed to serialize auth info")?;
    
    std::fs::write(&auth_path, content)
        .context("Failed to write auth file")?;
    
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
