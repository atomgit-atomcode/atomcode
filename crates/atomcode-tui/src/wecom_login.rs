//! WeCom (企业微信) QR-code OAuth login for GitCode internal employees.
//!
//! Flow (standard OAuth 2.0 Authorization Code):
//!   1. Open BROKER_AUTHORIZE_URL in browser with state + parent_origin.
//!   2. Broker drives WeCom scan, redirects to 127.0.0.1:8766/callback?code=...&state=...
//!   3. Client POSTs {"code":"..."} to BROKER_LOGIN_URL.
//!   4. Broker returns {user, api_key, model, api_base}.
//!
//! Credentials are held in memory only (ephemeral ProviderConfig). Never
//! written to disk — matches main's post-refactor security model.
//!
//! See `docs/zouwu-broker-integration.md` for the broker contract.

use std::io::{BufRead, Write};
use std::net::TcpListener;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const BROKER_AUTHORIZE_URL: &str = "https://test-zouwu.gitcode.com/login?popup=1&corp=csdn";
pub const BROKER_LOGIN_URL: &str = "https://test-zouwu.gitcode.com/api/v1/broker/exchange";
pub const REDIRECT_PORT: u16 = 8766;
pub const REDIRECT_URI: &str = "http://127.0.0.1:8766/callback";
pub const INTERNAL_PROVIDER_NAME: &str = "GitCode-Internal";

#[cfg(not(debug_assertions))]
const _RELEASE_GUARD: () = {
    assert!(
        !starts_with_placeholder(BROKER_AUTHORIZE_URL.as_bytes()),
        "wecom_login: BROKER_AUTHORIZE_URL not filled — cannot build release with placeholder"
    );
    assert!(
        !starts_with_placeholder(BROKER_LOGIN_URL.as_bytes()),
        "wecom_login: BROKER_LOGIN_URL not filled — cannot build release with placeholder"
    );
};

#[cfg(not(debug_assertions))]
const fn starts_with_placeholder(s: &[u8]) -> bool {
    s.len() >= 3 && s[0] == b'_' && s[1] == b'_' && s[2] == b'_'
}

/// Identity returned by the broker after a successful exchange.
#[derive(Debug, Clone)]
pub struct WecomIdentity {
    pub userid: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

/// Credentials minted by the broker for use with the internal LLM gateway.
#[derive(Debug, Clone)]
pub struct BrokerCreds {
    pub api_key: String,
    pub model: String,
    pub api_base: String,
}

#[derive(Debug, Deserialize)]
struct BrokerResponse {
    #[serde(default)]
    user: Option<BrokerUser>,
    #[serde(default, alias = "llm_token")]
    api_key: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    api_base: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BrokerUser {
    userid: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

fn urlencoding_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn generate_state() -> String {
    format!(
        "atomcode_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", url]).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let _ = url;
}

pub fn build_authorize_url(state: &str) -> String {
    build_authorize_url_with(BROKER_AUTHORIZE_URL, state, REDIRECT_URI)
}

fn build_authorize_url_with(authorize_url: &str, state: &str, parent_origin: &str) -> String {
    let sep = if authorize_url.contains('?') { '&' } else { '?' };
    format!(
        "{}{}state={}&parent_origin={}",
        authorize_url,
        sep,
        urlencoding_encode(state),
        urlencoding_encode(parent_origin),
    )
}

fn receive_code(port: u16, expected_state: &str) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("Bind 127.0.0.1:{} — port in use?", port))?;
    let (mut stream, _) = listener.accept().context("Accept OAuth callback")?;

    let mut reader = std::io::BufReader::new(&mut stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).context("Read callback HTTP line")?;

    let path = request_line
        .split_whitespace()
        .nth(1)
        .context("Malformed callback HTTP request")?
        .to_string();

    let query = path
        .find('?')
        .map(|i| &path[i + 1..])
        .context("Callback missing query string")?;

    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let k = parts.next().unwrap_or("");
        let v = parts.next().unwrap_or("");
        let decoded: String = url::form_urlencoded::parse(format!("x={}", v).as_bytes())
            .next()
            .map(|(_, val)| val.into_owned())
            .unwrap_or_default();
        match k {
            "code" => code = Some(decoded),
            "state" => state = Some(decoded),
            "error" => error = Some(decoded),
            _ => {}
        }
    }

    let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
        <html><head><style>body{font-family:system-ui;display:flex;justify-content:center;align-items:center;height:100vh;margin:0;background:#1a1a2e;color:#eee}\
        .container{text-align:center}h1{color:#7c3aed}.success{color:#22c55e;font-size:4rem}</style></head>\
        <body><div class=\"container\"><div class=\"success\">✓</div><h1>Authorization Successful</h1>\
        <p>You can close this window and return to AtomCode.</p></div></body></html>";
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();

    if let Some(err) = error {
        bail!("Broker returned error: {}", err);
    }
    let code = code.context("Callback missing `code` param")?;
    let state = state.unwrap_or_default();
    if state != expected_state {
        bail!("OAuth state mismatch (CSRF guard)");
    }
    Ok(code)
}

/// POST code → broker. Returns identity + credentials.
pub fn exchange_code_via_broker(code: &str) -> Result<(WecomIdentity, Option<BrokerCreds>)> {
    exchange_code_via_broker_with_url(BROKER_LOGIN_URL, code)
}

pub fn exchange_code_via_broker_with_url(
    url: &str,
    code: &str,
) -> Result<(WecomIdentity, Option<BrokerCreds>)> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("Build HTTP client")?;

    let body = serde_json::json!({ "code": code });
    let resp = client
        .post(url)
        .json(&body)
        .send()
        .with_context(|| {
            format!("Cannot reach broker login endpoint at {}. Check VPN/internal network.", url)
        })?;

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        let snippet: String = text.chars().take(500).collect();
        bail!("Broker login failed ({}): {}", status, snippet);
    }

    parse_broker_response(&text)
}

pub fn parse_broker_response(body: &str) -> Result<(WecomIdentity, Option<BrokerCreds>)> {
    let resp: BrokerResponse = serde_json::from_str(body)
        .with_context(|| format!("Parse broker JSON: {}", body.chars().take(500).collect::<String>()))?;

    if let Some(err) = resp.error {
        bail!("{}", err);
    }
    let user = resp.user.context("Broker response missing `user` field")?;

    let identity = WecomIdentity {
        userid: user.userid,
        name: user.name,
        email: user.email,
    };

    let creds = match (resp.api_key, resp.model, resp.api_base) {
        (Some(api_key), Some(model), Some(api_base)) if !api_key.is_empty() => {
            Some(BrokerCreds { api_key, model, api_base })
        }
        _ => None,
    };

    Ok((identity, creds))
}

/// Full WeCom browser-QR login flow. Caller is responsible for leaving the
/// TUI alt screen before calling. Blocking; wrap in `spawn_blocking` when
/// invoked from an async context (reqwest::blocking owns a tokio runtime).
pub fn login() -> Result<(WecomIdentity, Option<BrokerCreds>)> {
    println!("\n  AtomCode Login (WeCom / GitCode Internal)");
    println!("  =========================================\n");

    let state = generate_state();
    let auth_url = build_authorize_url(&state);

    println!("  Opening browser for WeCom QR login...");
    println!("  If browser doesn't open, visit:\n    {}\n", auth_url);
    open_browser(&auth_url);

    println!("  Waiting for callback on port {}...", REDIRECT_PORT);
    let code = receive_code(REDIRECT_PORT, &state)?;
    println!("  Scan received, exchanging with broker...\n");

    let (identity, creds) = exchange_code_via_broker(&code)?;
    println!(
        "  Identity: {} ({})\n",
        identity.name.clone().unwrap_or_else(|| identity.userid.clone()),
        identity.userid
    );
    Ok((identity, creds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_authorize_url_appends_with_ampersand_when_query_exists() {
        let url = build_authorize_url_with(
            "https://test-zouwu.gitcode.com/login?popup=1&corp=csdn",
            "state_xyz",
            "http://127.0.0.1:8766/callback",
        );
        assert!(url.starts_with("https://test-zouwu.gitcode.com/login?popup=1&corp=csdn&state=state_xyz&parent_origin="));
    }

    #[test]
    fn build_authorize_url_uses_question_mark_when_no_query() {
        let url = build_authorize_url_with("https://broker/login", "s", "http://127.0.0.1:8766/callback");
        assert!(url.starts_with("https://broker/login?state=s&parent_origin="));
    }

    #[test]
    fn parse_broker_response_full_shape() {
        let json = r#"{
            "user": {"userid":"lichao","name":"李超","email":"lichao@gitcode.com"},
            "api_key":"sk-x","model":"MiniMax-M2.7","api_base":"https://api-ai.gitcode.com/v1"
        }"#;
        let (id, creds) = parse_broker_response(json).unwrap();
        assert_eq!(id.userid, "lichao");
        assert_eq!(id.name.as_deref(), Some("李超"));
        let c = creds.expect("creds present");
        assert_eq!(c.api_key, "sk-x");
        assert_eq!(c.model, "MiniMax-M2.7");
        assert_eq!(c.api_base, "https://api-ai.gitcode.com/v1");
    }

    #[test]
    fn parse_broker_response_user_only_no_creds() {
        let json = r#"{"user":{"userid":"lichao"}}"#;
        let (id, creds) = parse_broker_response(json).unwrap();
        assert_eq!(id.userid, "lichao");
        assert!(creds.is_none());
    }

    #[test]
    fn parse_broker_response_error_field() {
        let json = r#"{"error":"not a member"}"#;
        let err = parse_broker_response(json).unwrap_err();
        assert!(err.to_string().contains("not a member"));
    }

    #[test]
    fn parse_broker_response_legacy_llm_token_alias() {
        let json = r#"{"user":{"userid":"lichao"},"llm_token":"sk-legacy","model":"m","api_base":"https://u"}"#;
        let (_, creds) = parse_broker_response(json).unwrap();
        assert_eq!(creds.unwrap().api_key, "sk-legacy");
    }
}
