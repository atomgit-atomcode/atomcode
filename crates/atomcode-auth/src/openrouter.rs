//! OpenRouter 免费模型快捷接入:OAuth PKCE 取 key、免费模型发现。
//! 独立于 atomgit 自家 OAuth(那是 state 轮询式,协议不同)。

use base64::Engine as _;
use sha2::{Digest, Sha256};

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
}
