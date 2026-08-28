//! OpenRouter 免费模型快捷接入:OAuth PKCE 取 key、免费模型发现。
//! 独立于 atomgit 自家 OAuth(那是 state 轮询式,协议不同)。

use anyhow::{Context as _, Result};
use base64::Engine as _;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const OPENROUTER_AUTH_URL: &str = "https://openrouter.ai/auth";
pub const OPENROUTER_KEYS_URL: &str = "https://openrouter.ai/api/v1/auth/keys";
pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// base64url(sha256(verifier)),无填充 —— PKCE S256。
pub fn code_challenge_s256(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// 生成 96 字节随机 → base64url(无填充)得到 128 字符的 verifier(unreserved 字符集),
/// 及其 S256 challenge。
pub fn generate_pkce() -> PkcePair {
    use rand::RngCore;
    let mut bytes = [0u8; 96];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let challenge = code_challenge_s256(&verifier);
    PkcePair { verifier, challenge }
}

/// 拼 OpenRouter 授权 URL。`callback_url=None` 走 headless(不带回调,code 上屏)。
pub fn build_auth_url(callback_url: Option<&str>, code_challenge: &str) -> String {
    let mut url = format!(
        "{OPENROUTER_AUTH_URL}?code_challenge={}&code_challenge_method=S256",
        urlencoding_component(code_challenge),
    );
    if let Some(cb) = callback_url {
        url.push_str(&format!("&callback_url={}", urlencoding_component(cb)));
    }
    url
}

/// 最小 RFC3986 component 编码(unreserved 之外全部 %XX)。避免为编码单独引依赖。
fn urlencoding_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct FreeModel {
    pub id: String,
    pub name: Option<String>,
    pub context_length: u64,
}

pub fn parse_key_response(body: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct KeyResp {
        key: Option<String>,
    }
    let parsed: KeyResp = serde_json::from_str(body).context("parse /auth/keys response")?;
    parsed
        .key
        .filter(|k| !k.trim().is_empty())
        .context("/auth/keys response missing `key`")
}

pub fn select_top_free_models(models_json: &str, limit: usize) -> Result<Vec<FreeModel>> {
    #[derive(Deserialize)]
    struct ModelsResp {
        data: Vec<RawModel>,
    }
    #[derive(Deserialize)]
    struct RawModel {
        id: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        context_length: u64,
        #[serde(default)]
        pricing: Option<Pricing>,
    }
    #[derive(Deserialize)]
    struct Pricing {
        #[serde(default)]
        prompt: String,
        #[serde(default)]
        completion: String,
    }

    fn is_zero(p: &str) -> bool {
        // "0" / "0.0" / "0.00" 都算零价。
        p.trim().parse::<f64>().map(|v| v == 0.0).unwrap_or(false)
    }

    let resp: ModelsResp = serde_json::from_str(models_json).context("parse /models response")?;
    let mut free: Vec<FreeModel> = resp
        .data
        .into_iter()
        .filter(|m| {
            m.id.ends_with(":free")
                || m.pricing
                    .as_ref()
                    .map(|p| is_zero(&p.prompt) && is_zero(&p.completion))
                    .unwrap_or(false)
        })
        .map(|m| FreeModel {
            id: m.id,
            name: m.name,
            context_length: m.context_length,
        })
        .collect();
    // context 降序;并列时按 id 稳定排序,保证测试确定性。
    free.sort_by(|a, b| {
        b.context_length
            .cmp(&a.context_length)
            .then_with(|| a.id.cmp(&b.id))
    });
    free.truncate(limit);
    Ok(free)
}

pub fn parse_code_from_request_line(line: &str) -> Option<String> {
    // "GET /callback?code=XXX&foo=bar HTTP/1.1" → XXX
    let target = line.split_whitespace().nth(1)?; // "/callback?code=..."
    let query = target.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("code=") {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

pub struct LocalCallback {
    listener: TcpListener,
}

pub fn start_local_callback() -> Result<LocalCallback> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("bind loopback callback port")?;
    listener
        .set_nonblocking(true)
        .context("set callback listener non-blocking")?;
    Ok(LocalCallback { listener })
}

impl LocalCallback {
    pub fn port(&self) -> u16 {
        self.listener.local_addr().map(|a| a.port()).unwrap_or(0)
    }

    /// 阻塞等待浏览器命中回调,取 `code`。轮询 accept 以便 `cancel`/`timeout` 生效。
    /// `Ok(None)` = 取消或超时(不视为错误)。
    pub fn wait_for_code(
        self,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Ok(None);
            }
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let text = String::from_utf8_lossy(&buf[..n]);
                    let first_line = text.lines().next().unwrap_or("");
                    let code = parse_code_from_request_line(first_line);
                    // 回一页让用户知道可以关掉标签。
                    let body = "<html><body>已接入 OpenRouter,可关闭此页返回终端。</body></html>";
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.as_bytes().len(),
                            body
                        )
                        .as_bytes(),
                    );
                    return Ok(code);
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(anyhow::Error::new(e).context("accept callback connection")),
            }
        }
    }
}

fn blocking_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(crate::ATOMCODE_USER_AGENT)
        .build()
        .context("build OpenRouter HTTP client")
}

/// POST /api/v1/auth/keys {code, code_verifier, code_challenge_method:"S256"} → key。
pub fn exchange_code_for_key(code: &str, verifier: &str) -> Result<String> {
    let client = blocking_client()?;
    let resp = client
        .post(OPENROUTER_KEYS_URL)
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            "code_challenge_method": "S256",
        }))
        .send()
        .context("call OpenRouter /auth/keys")?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("OpenRouter /auth/keys 返回 HTTP {}", status.as_u16());
    }
    parse_key_response(&body)
}

/// GET /api/v1/models(Bearer)→ 过滤 free、按 context 降序、取 limit。
pub fn fetch_top_free_models(api_key: &str, limit: usize) -> Result<Vec<FreeModel>> {
    let client = blocking_client()?;
    let resp = client
        .get(OPENROUTER_MODELS_URL)
        .bearer_auth(api_key)
        .send()
        .context("call OpenRouter /models")?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("OpenRouter /models 返回 HTTP {}", status.as_u16());
    }
    select_top_free_models(&body, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 7636 附录 B 官方向量:verifier → S256 challenge。
    #[test]
    fn s256_challenge_matches_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_pair_roundtrips() {
        let p = generate_pkce();
        // verifier 满足 RFC 7636 长度(43..=128)与 unreserved 字符集。
        assert!((43..=128).contains(&p.verifier.len()), "len={}", p.verifier.len());
        assert!(p.verifier.chars().all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
        // challenge 与 verifier 自洽,且 base64url 无填充(不含 '=' '+' '/')。
        assert_eq!(p.challenge, code_challenge_s256(&p.verifier));
        assert!(!p.challenge.contains(['=', '+', '/']));
    }

    #[test]
    fn auth_url_has_callback_and_challenge() {
        let url = build_auth_url(Some("http://localhost:51234/callback"), "CHAL");
        assert!(url.starts_with("https://openrouter.ai/auth?"));
        assert!(url.contains("code_challenge=CHAL"));
        assert!(url.contains("code_challenge_method=S256"));
        // callback_url 需 URL 编码(':' '/' 转义)。
        assert!(url.contains("callback_url=http%3A%2F%2Flocalhost%3A51234%2Fcallback"));
    }

    #[test]
    fn auth_url_headless_omits_callback() {
        let url = build_auth_url(None, "CHAL");
        assert!(!url.contains("callback_url="));
        assert!(url.contains("code_challenge=CHAL"));
    }

    #[test]
    fn parse_key_extracts_field() {
        assert_eq!(parse_key_response(r#"{"key":"sk-or-v1-abc"}"#).unwrap(), "sk-or-v1-abc");
    }

    #[test]
    fn parse_key_errors_on_missing() {
        assert!(parse_key_response(r#"{"error":"bad"}"#).is_err());
    }

    const MODELS_FIXTURE: &str = r#"{
      "data": [
        {"id":"vendor/big:free","name":"Big Free","context_length":128000,
         "pricing":{"prompt":"0","completion":"0"}},
        {"id":"vendor/paid","name":"Paid","context_length":200000,
         "pricing":{"prompt":"0.001","completion":"0.002"}},
        {"id":"vendor/small:free","name":"Small Free","context_length":8000,
         "pricing":{"prompt":"0","completion":"0"}},
        {"id":"vendor/zero-priced","name":"Zero Priced","context_length":32000,
         "pricing":{"prompt":"0","completion":"0"}},
        {"id":"vendor/nopricing","context_length":16000}
      ]
    }"#;

    #[test]
    fn top_free_filters_paid_and_sorts_by_context_desc() {
        let got = select_top_free_models(MODELS_FIXTURE, 5).unwrap();
        // paid 被剔除;nopricing 无 pricing 字段 → 不视为 free(保守),被剔除。
        let ids: Vec<&str> = got.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["vendor/big:free", "vendor/zero-priced", "vendor/small:free"]);
    }

    #[test]
    fn top_free_respects_limit() {
        let got = select_top_free_models(MODELS_FIXTURE, 2).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "vendor/big:free"); // 最大 context 优先
    }

    #[test]
    fn top_free_empty_when_none_free() {
        let json = r#"{"data":[{"id":"x/paid","context_length":9,"pricing":{"prompt":"0.01","completion":"0"}}]}"#;
        assert!(select_top_free_models(json, 5).unwrap().is_empty());
    }

    #[test]
    fn code_parsed_from_request_line() {
        let line = "GET /callback?code=abc123&scope=x HTTP/1.1";
        assert_eq!(parse_code_from_request_line(line).as_deref(), Some("abc123"));
    }

    #[test]
    fn code_none_when_absent() {
        assert_eq!(parse_code_from_request_line("GET /callback HTTP/1.1"), None);
    }

    #[test]
    fn local_callback_receives_code_over_loopback() {
        use std::io::Write;
        use std::sync::atomic::AtomicBool;
        let cb = start_local_callback().unwrap();
        let port = cb.port();
        // 后台线程模拟浏览器回调命中 127.0.0.1:<port>。
        let h = std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(b"GET /callback?code=deadbeef HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
        });
        let cancel = AtomicBool::new(false);
        let code = cb
            .wait_for_code(std::time::Duration::from_secs(3), &cancel)
            .unwrap();
        h.join().unwrap();
        assert_eq!(code.as_deref(), Some("deadbeef"));
    }
}
