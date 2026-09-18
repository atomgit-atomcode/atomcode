# OpenRouter 免费模型一键接入 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用户额度用尽或新用户未领 CodingPlan 时,一键(OAuth PKCE 或 `/openrouter <key>`)接入 OpenRouter,自动装配 top5 免费模型。

**Architecture:** 在 `atomcode-auth` 新增 `openrouter` 模块(PKCE 生成 / 本地回调 listener / code→key 交换 / 免费模型发现,纯逻辑可单测)。在 `atomcode-tuix` 新增 `event_loop/openrouter_connect.rs`(后台线程取 key+发现模型→事件→主循环装配 config + reload),挂 `/openrouter [key]` 命令与两个 nudge。取 key 两条入口在"发现+装配"下游汇合。

**Tech Stack:** Rust,reqwest blocking(atomcode-auth 已有),std::net::TcpListener(本地回调,无新依赖),sha2 / base64 0.22 / rand 0.8(PKCE,workspace 已存在,加进 atomcode-auth),atomcode-config 新 schema(`ProviderAccountConfig` + `ModelProfileConfig`),tokio mpsc(TUI 事件)。

**Spec:** `docs/superpowers/specs/2026-08-28-openrouter-quick-connect-design.md`

## Global Constraints

- 目标分支 `release/v5.1.0`。工作区可能有并行改动;**每个 task 只 `git add` 自己触碰的文件**,绝不 `git add -A`。
- **行号会漂**(分支在动)。计划里给的行号是探查时快照;实现前先 `grep` 符号名定位真锚点。
- **禁止在代码/注释/commit 里出现 "opencode" 等外部参考名**(用中性描述)。
- **key 安全**:OpenRouter 返回的是能花用户钱的真实 key。**只添加免费模型、默认切到免费模型,绝不静默启用付费模型**。含 key 的字符串不得进日志/telemetry。
- OpenRouter 免费判定:模型 id 带 `:free` 后缀 **或** `pricing.prompt` 与 `pricing.completion` 均为 `"0"`。排序:`context_length` 降序。取前 **5**。
- OAuth PKCE 事实(已核实):auth URL = `https://openrouter.ai/auth?callback_url=<cb>&code_challenge=<S256>&code_challenge_method=S256`;换 key = `POST https://openrouter.ai/api/v1/auth/keys` body `{code, code_verifier, code_challenge_method}` → `{"key":"..."}`;模型 = `GET https://openrouter.ai/api/v1/models`(Bearer key)。localhost 回调任意端口,无需 client_id。
- commit 结尾加 `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`。
- v1 仅交互式 TUI;webui/headless/ACP 不做。持久化保存(`ephemeral: false`)。

---

### Task 1: PKCE 原语 + auth URL(atomcode-auth 新模块)

**Files:**
- Create: `crates/atomcode-auth/src/openrouter.rs`
- Modify: `crates/atomcode-auth/src/lib.rs`(加 `pub mod openrouter;`,放在 `pub mod oauth;` 附近)
- Modify: `crates/atomcode-auth/Cargo.toml`(`[dependencies]` 加 `sha2 = "0.10"`、`base64 = "0.22"`、`rand = "0.8"`)
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub struct PkcePair { pub verifier: String, pub challenge: String }`
  - `pub fn generate_pkce() -> PkcePair`
  - `pub fn code_challenge_s256(verifier: &str) -> String`
  - `pub fn build_auth_url(callback_url: Option<&str>, code_challenge: &str) -> String`
  - `pub const OPENROUTER_AUTH_URL: &str = "https://openrouter.ai/auth";`
  - `pub const OPENROUTER_KEYS_URL: &str = "https://openrouter.ai/api/v1/auth/keys";`
  - `pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";`

- [ ] **Step 1: 加依赖**

在 `crates/atomcode-auth/Cargo.toml` 的 `[dependencies]` 末尾加(sha2/base64/rand 已在 workspace Cargo.lock,只是 atomcode-auth 未直接声明):

```toml
sha2 = "0.10"
base64 = "0.22"
rand = "0.8"
```

- [ ] **Step 2: 注册模块**

在 `crates/atomcode-auth/src/lib.rs` 中 `pub mod oauth;` 下一行加:

```rust
pub mod openrouter;
```

- [ ] **Step 3: 写失败测试**

`crates/atomcode-auth/src/openrouter.rs`:

```rust
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
```

- [ ] **Step 4: 跑测试确认失败**

Run: `cargo test -p atomcode-auth openrouter::tests 2>&1 | tail -20`
Expected: 编译失败(`code_challenge_s256` 等未定义)。

- [ ] **Step 5: 实现**

`crates/atomcode-auth/src/openrouter.rs` 顶部:

```rust
//! OpenRouter 免费模型快捷接入:OAuth PKCE 取 key、免费模型发现。
//! 独立于 atomgit 自家 OAuth(那是 state 轮询式,协议不同)。

use anyhow::{Context, Result};
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
```

> 注:`build_auth_url` 测试断言 `code_challenge=CHAL`(CHAL 无需转义,原样)。若 challenge 含 base64url 字符 `-`/`_` 也在 unreserved 集,不会被编码 —— 与 OpenRouter 期望一致。

- [ ] **Step 6: 跑测试确认通过**

Run: `cargo test -p atomcode-auth openrouter::tests 2>&1 | tail -20`
Expected: 4 passed。

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-auth/src/openrouter.rs crates/atomcode-auth/src/lib.rs crates/atomcode-auth/Cargo.toml
git commit -m "$(printf 'feat(auth): OpenRouter PKCE 原语 + auth URL 构造\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 2: 免费模型发现的纯逻辑(解析 + 过滤 + 排序)

**Files:**
- Modify: `crates/atomcode-auth/src/openrouter.rs`
- Test: 同文件 tests

**Interfaces:**
- Produces:
  - `pub struct FreeModel { pub id: String, pub name: Option<String>, pub context_length: u64 }`
  - `pub fn parse_key_response(body: &str) -> Result<String>` —— 从 `{"key":"..."}` 取 key。
  - `pub fn select_top_free_models(models_json: &str, limit: usize) -> Result<Vec<FreeModel>>` —— 解析 `/models` 响应、过滤 free、按 context 降序、取 limit。

- [ ] **Step 1: 写失败测试**

追加到 `openrouter.rs` 的 tests:

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-auth openrouter::tests 2>&1 | tail -20`
Expected: 编译失败(`parse_key_response`/`select_top_free_models` 未定义)。

- [ ] **Step 3: 实现**

追加到 `openrouter.rs`(顶部 use 处补 `use serde::Deserialize;`):

```rust
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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p atomcode-auth openrouter::tests 2>&1 | tail -20`
Expected: 全绿(含 Task 1 的 4 个)。

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-auth/src/openrouter.rs
git commit -m "$(printf 'feat(auth): OpenRouter 免费模型发现纯逻辑(解析/过滤/排序)\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 3: 本地回调 listener + HTTP 请求行解析

**Files:**
- Modify: `crates/atomcode-auth/src/openrouter.rs`
- Test: 同文件 tests

**Interfaces:**
- Produces:
  - `pub fn parse_code_from_request_line(line: &str) -> Option<String>` —— 从 `GET /callback?code=XXX&... HTTP/1.1` 取 code。
  - `pub struct LocalCallback { listener: std::net::TcpListener }`
  - `pub fn start_local_callback() -> Result<LocalCallback>` —— 绑 `127.0.0.1:0`。
  - `impl LocalCallback { pub fn port(&self) -> u16; pub fn wait_for_code(self, timeout: std::time::Duration, cancel: &std::sync::atomic::AtomicBool) -> Result<Option<String>> }`(返回 `Ok(None)` 表示取消/超时)。

- [ ] **Step 1: 写失败测试**

追加 tests:

```rust
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
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-auth openrouter::tests::local_callback 2>&1 | tail -20`
Expected: 编译失败。

- [ ] **Step 3: 实现**

追加到 `openrouter.rs`:

```rust
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

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
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p atomcode-auth openrouter 2>&1 | tail -20`
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-auth/src/openrouter.rs
git commit -m "$(printf 'feat(auth): OpenRouter 本地回调 listener + 请求行 code 解析\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 4: HTTP 交换 + 发现(联网胶水,复用 atomcode-auth blocking client)

**Files:**
- Modify: `crates/atomcode-auth/src/openrouter.rs`

**Interfaces:**
- Consumes: Task 1 `build_auth_url`/常量,Task 2 `parse_key_response`/`select_top_free_models`。
- Produces:
  - `pub fn exchange_code_for_key(code: &str, verifier: &str) -> Result<String>`
  - `pub fn fetch_top_free_models(api_key: &str, limit: usize) -> Result<Vec<FreeModel>>`
  - `fn blocking_client() -> Result<reqwest::blocking::Client>`(私有,镜像 oauth.rs 的 `blocking_client_with_tls12(false)` 超时 5s/10s + `crate::ATOMCODE_USER_AGENT`)

> 说明:这两个函数是网络 I/O,不写单测(纯解析逻辑已在 Task 2 覆盖)。实现须复用 `oauth.rs` 里 `blocking_client_with_tls12` 的 client 构造惯例(connect 5s / total 10s / user-agent)。

- [ ] **Step 1: 实现 exchange + fetch**

追加到 `openrouter.rs`:

```rust
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
```

- [ ] **Step 2: 编译确认**

Run: `cargo build -p atomcode-auth 2>&1 | tail -20`
Expected: 编译通过,无 warning。

- [ ] **Step 3: Commit**

```bash
git add crates/atomcode-auth/src/openrouter.rs
git commit -m "$(printf 'feat(auth): OpenRouter code→key 交换 + 免费模型拉取\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 5: 装配纯函数(把 key+models 写进 Config,幂等)

**Files:**
- Create: `crates/atomcode-tuix/src/event_loop/openrouter_connect.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`(顶部 `mod` 声明处加 `pub(crate) mod openrouter_connect;`;grep `mod oauth_poll;` 定位邻近插入)
- Test: 新文件 tests

**Interfaces:**
- Consumes: `atomcode_auth::openrouter::FreeModel`;`atomcode_config::config::{Config, ProviderAccountConfig, ModelProfileConfig}`;`atomcode_config::config::provider_preset::{preset_or_compatible}`;`atomcode_config::config::provider::default_context_window_for`。
- Produces:
  - `pub struct ProvisionOutcome { pub account_id: String, pub added: Vec<String>, pub default_model: String }`
  - `pub fn provision_openrouter(config: &mut Config, api_key: &str, models: &[FreeModel]) -> ProvisionOutcome`

> 幂等约定:account id 固定 `"openrouter"`。已存在则原地更新 `api_key`(不新建);模型 selection id `openrouter/<model>`,已存在则跳过(用 `config.selection_exists`)。`default_model` 为空时设为首个新模型。全部 `ephemeral: false`。

- [ ] **Step 1: 写失败测试**

`crates/atomcode-tuix/src/event_loop/openrouter_connect.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_auth::openrouter::FreeModel;
    use atomcode_config::config::Config;

    fn models() -> Vec<FreeModel> {
        vec![
            FreeModel { id: "vendor/big:free".into(), name: Some("Big".into()), context_length: 128000 },
            FreeModel { id: "vendor/small:free".into(), name: None, context_length: 8000 },
        ]
    }

    #[test]
    fn fresh_config_gets_account_models_and_default() {
        let mut c = Config::default();
        let out = provision_openrouter(&mut c, "sk-or-v1-x", &models());
        assert_eq!(out.account_id, "openrouter");
        assert!(c.provider_accounts.contains_key("openrouter"));
        assert_eq!(c.provider_accounts["openrouter"].api_key.as_deref(), Some("sk-or-v1-x"));
        assert!(!c.provider_accounts["openrouter"].ephemeral);
        assert!(c.models.contains_key("openrouter/vendor/big:free"));
        assert!(c.models.contains_key("openrouter/vendor/small:free"));
        assert_eq!(out.default_model, "openrouter/vendor/big:free");
        assert_eq!(c.default_model.as_deref(), Some("openrouter/vendor/big:free"));
    }

    #[test]
    fn existing_account_key_updated_not_duplicated() {
        let mut c = Config::default();
        provision_openrouter(&mut c, "sk-or-v1-OLD", &models());
        let out = provision_openrouter(&mut c, "sk-or-v1-NEW", &models());
        // 仍只有一个 openrouter 账号,key 被更新,模型不翻倍。
        assert_eq!(c.provider_accounts["openrouter"].api_key.as_deref(), Some("sk-or-v1-NEW"));
        assert_eq!(c.models.keys().filter(|k| k.starts_with("openrouter/")).count(), 2);
        assert!(out.added.is_empty()); // 二次运行无新增
    }

    #[test]
    fn preexisting_default_model_is_preserved() {
        let mut c = Config::default();
        c.default_model = Some("someacct/somemodel".into());
        let out = provision_openrouter(&mut c, "k", &models());
        assert_eq!(c.default_model.as_deref(), Some("someacct/somemodel"));
        // 未改动全局默认,但 outcome 仍报告首个新模型供 UI 提示。
        assert_eq!(out.default_model, "openrouter/vendor/big:free");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-tuix openrouter_connect::tests 2>&1 | tail -20`
Expected: 编译失败(模块/函数未定义)。

- [ ] **Step 3: 注册模块 + 实现**

在 `crates/atomcode-tuix/src/event_loop/mod.rs` 顶部,`grep -n "mod oauth_poll;"` 定位后邻近加:

```rust
pub(crate) mod openrouter_connect;
```

`openrouter_connect.rs` 顶部实现:

```rust
//! OpenRouter 一键接入:把 key + top5 免费模型装配进 Config(幂等纯函数),
//! 以及后台连接任务(Task 6)。

use atomcode_auth::openrouter::FreeModel;
use atomcode_config::config::provider::default_context_window_for;
use atomcode_config::config::provider_preset::preset_or_compatible;
use atomcode_config::config::{Config, ModelProfileConfig, ProviderAccountConfig};

const OPENROUTER_ACCOUNT_ID: &str = "openrouter";

pub struct ProvisionOutcome {
    pub account_id: String,
    pub added: Vec<String>,
    pub default_model: String,
}

/// 幂等装配:account 固定 id,存在则更新 key;模型 selection `openrouter/<model>`,
/// 存在则跳过。全部持久(ephemeral=false)。
pub fn provision_openrouter(
    config: &mut Config,
    api_key: &str,
    models: &[FreeModel],
) -> ProvisionOutcome {
    let preset = preset_or_compatible(OPENROUTER_ACCOUNT_ID);
    let provider_type_wire = preset.provider_type.wire().to_string();

    // upsert 账号(仅更新 key/base_url,保留其它)。
    config
        .provider_accounts
        .entry(OPENROUTER_ACCOUNT_ID.to_string())
        .and_modify(|a| a.api_key = Some(api_key.to_string()))
        .or_insert_with(|| ProviderAccountConfig {
            provider: OPENROUTER_ACCOUNT_ID.to_string(),
            display_name: None,
            api_key: Some(api_key.to_string()),
            base_url: None, // 用 preset 默认 https://openrouter.ai/api/v1
            user_agent: None,
            skip_tls_verify: false,
            enterprise_url: None,
            ephemeral: false,
        });

    let mut added = Vec::new();
    let mut first_selection: Option<String> = None;
    for m in models {
        let selection_id = format!("{OPENROUTER_ACCOUNT_ID}/{}", m.id);
        if first_selection.is_none() {
            first_selection = Some(selection_id.clone());
        }
        if config.selection_exists(&selection_id) {
            continue;
        }
        config.models.insert(
            selection_id.clone(),
            ModelProfileConfig {
                account: OPENROUTER_ACCOUNT_ID.to_string(),
                model: m.id.clone(),
                display_name: m.name.clone(),
                system_prompt: None,
                supports_vision: None,
                context_window: if m.context_length > 0 {
                    m.context_length as usize
                } else {
                    default_context_window_for(&provider_type_wire)
                },
                max_tokens: None,
                capable_model: None,
                thinking_type: None,
                thinking_keep: None,
                reasoning_history: None,
                reasoning_effort: None,
                reasoning_effort_levels: None,
                thinking_enabled: None,
                thinking_budget: None,
                retry_max_attempts: None,
            },
        );
        added.push(selection_id);
    }

    let default_model = first_selection.unwrap_or_default();
    if config.default_model.is_none() && !default_model.is_empty() {
        config.default_model = Some(default_model.clone());
    }

    ProvisionOutcome {
        account_id: OPENROUTER_ACCOUNT_ID.to_string(),
        added,
        default_model,
    }
}
```

> 实现前 `grep -n "fn default_context_window_for" crates/atomcode-config/src/config/provider.rs` 确认签名接受 `&str`(wire type)。若签名不同(如接受 `ProviderType`),按实际调整这一行。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p atomcode-tuix openrouter_connect::tests 2>&1 | tail -20`
Expected: 3 passed。

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/openrouter_connect.rs crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "$(printf 'feat(tuix): OpenRouter 装配纯函数(key+top5 模型,幂等)\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 6: 后台连接任务 + 事件 + 通道 + select! 处理

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/openrouter_connect.rs`(加事件枚举 + spawn 任务 + 应用函数)
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`(LoopCtx 加通道字段 + 构造处初始化 + select! 新臂)

**Interfaces:**
- Consumes: Task 1-5 全部;`atomcode_auth::openrouter::{generate_pkce, build_auth_url, start_local_callback, exchange_code_for_key, fetch_top_free_models}`;`crate::event_loop::oauth_poll` 的 wake 模式(`wake_tx: mpsc::Sender<()>`)。
- Produces:
  - `pub enum ConnectMode { Oauth, ProvidedKey(String) }`
  - `pub fn parse_connect_mode(arg: &str) -> ConnectMode`
  - `pub enum OpenRouterConnectEvent { Ready { api_key: String, models: Vec<FreeModel> }, Failed(String) }`
  - `pub fn spawn_openrouter_connect(mode: ConnectMode, event_tx: tokio::sync::mpsc::UnboundedSender<OpenRouterConnectEvent>, wake_tx: tokio::sync::mpsc::Sender<()>, cancel: std::sync::Arc<std::sync::atomic::AtomicBool>)`
  - LoopCtx 字段:`pub openrouter_event_tx: mpsc::UnboundedSender<openrouter_connect::OpenRouterConnectEvent>`、`pub openrouter_event_rx: mpsc::UnboundedReceiver<...>`、`pub openrouter_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>`

> 线程只做网络(取 key + 发现模型),发 `Ready{key,models}`;**装配 + 存盘 + reload 在主循环 select! 臂**上做(镜像 `OauthEvent::Authorized` 在循环里跑 `run_login_flow`)。这样 `config_store.update` / `reload_runtime_provider` 都在循环线程,避免 Send/借用问题。

- [ ] **Step 1: 写失败测试(parse_connect_mode)**

追加 openrouter_connect tests:

```rust
    #[test]
    fn arg_parsing_selects_mode() {
        assert!(matches!(parse_connect_mode(""), ConnectMode::Oauth));
        assert!(matches!(parse_connect_mode("   "), ConnectMode::Oauth));
        match parse_connect_mode("  sk-or-v1-abc  ") {
            ConnectMode::ProvidedKey(k) => assert_eq!(k, "sk-or-v1-abc"),
            _ => panic!("expected ProvidedKey"),
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-tuix openrouter_connect::tests::arg_parsing 2>&1 | tail -20`
Expected: 失败。

- [ ] **Step 3: 实现事件 + 任务**

追加到 `openrouter_connect.rs`(顶部补 `use std::sync::Arc; use std::sync::atomic::AtomicBool;`,以及 `use tokio::sync::mpsc;`):

```rust
pub enum ConnectMode {
    Oauth,
    ProvidedKey(String),
}

pub fn parse_connect_mode(arg: &str) -> ConnectMode {
    let t = arg.trim();
    if t.is_empty() {
        ConnectMode::Oauth
    } else {
        ConnectMode::ProvidedKey(t.to_string())
    }
}

pub enum OpenRouterConnectEvent {
    Ready {
        api_key: String,
        models: Vec<FreeModel>,
    },
    Failed(String),
}

const FREE_MODEL_LIMIT: usize = 5;

/// 后台线程:取 key(OAuth 或直传)+ 发现 top5 免费模型 → 发事件 + 唤醒循环。
pub fn spawn_openrouter_connect(
    mode: ConnectMode,
    event_tx: mpsc::UnboundedSender<OpenRouterConnectEvent>,
    wake_tx: mpsc::Sender<()>,
    cancel: Arc<AtomicBool>,
) {
    use atomcode_auth::openrouter as or;
    std::thread::spawn(move || {
        let result: Result<(String, Vec<or::FreeModel>), String> = (|| {
            let key = match mode {
                ConnectMode::ProvidedKey(k) => k,
                ConnectMode::Oauth => {
                    let pkce = or::generate_pkce();
                    let cb = or::start_local_callback().map_err(|e| format!("{e:#}"))?;
                    let callback_url = format!("http://localhost:{}/callback", cb.port());
                    let auth_url = or::build_auth_url(Some(&callback_url), &pkce.challenge);
                    let _ = crate::event_loop::oauth_poll::open_browser_best_effort(&auth_url);
                    // 3 分钟等回调;cancel 由 ESC 置位(Task 6 Step 6 接线)。
                    let code = cb
                        .wait_for_code(std::time::Duration::from_secs(180), &cancel)
                        .map_err(|e| format!("{e:#}"))?
                        .ok_or_else(|| "已取消或超时".to_string())?;
                    or::exchange_code_for_key(&code, &pkce.verifier).map_err(|e| format!("{e:#}"))?
                }
            };
            let models = or::fetch_top_free_models(&key, FREE_MODEL_LIMIT)
                .map_err(|e| format!("{e:#}"))?;
            if models.is_empty() {
                return Err("OpenRouter 未返回可用免费模型".to_string());
            }
            Ok((key, models))
        })();

        let event = match result {
            Ok((api_key, models)) => OpenRouterConnectEvent::Ready { api_key, models },
            Err(reason) => OpenRouterConnectEvent::Failed(reason),
        };
        let _ = event_tx.send(event);
        let _ = wake_tx.blocking_send(());
    });
}
```

> `open_browser_best_effort` 若 oauth_poll 未暴露自由函数,改用 `atomcode_auth` 里已有的 `open_browser`(Task 探查:`oauth.rs` 有 `open_browser`,可 `pub` 复用或在 openrouter.rs 内联一个)。实现前 `grep -n "fn open_browser" crates/atomcode-auth/src/oauth.rs` 确认可见性并按实际调整。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p atomcode-tuix openrouter_connect::tests 2>&1 | tail -20`
Expected: 全绿(含 Task 5 的 3 个 + arg 解析)。

- [ ] **Step 5: LoopCtx 加通道 + 初始化**

在 `crates/atomcode-tuix/src/event_loop/mod.rs`:
1. `grep -n "oauth_event_rx" ` 定位 LoopCtx 字段声明(~3985),仿照加:

```rust
    pub openrouter_event_rx: mpsc::UnboundedReceiver<openrouter_connect::OpenRouterConnectEvent>,
    pub openrouter_event_tx: mpsc::UnboundedSender<openrouter_connect::OpenRouterConnectEvent>,
    pub openrouter_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
```

2. `grep -n "oauth_event_tx" ` 定位 LoopCtx 构造处(通道创建 `mpsc::unbounded_channel()`),仿照加:

```rust
        let (openrouter_event_tx, openrouter_event_rx) = mpsc::unbounded_channel();
```

并在结构体字面量里填 `openrouter_event_tx`、`openrouter_event_rx`、`openrouter_cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))`。

- [ ] **Step 6: 主循环 select! 新臂(装配 + 存盘 + reload)**

`grep -n "Some(ev) = ctx.oauth_event_rx.recv()"` 定位 OAuth 臂(~9931),在其后加新臂:

```rust
            Some(ev) = ctx.openrouter_event_rx.recv() => {
                use openrouter_connect::OpenRouterConnectEvent;
                match ev {
                    OpenRouterConnectEvent::Ready { api_key, models } => {
                        match ctx.config_store.update(|latest| {
                            openrouter_connect::provision_openrouter(latest, &api_key, &models);
                            Ok(())
                        }) {
                            Ok(commit) => {
                                let count = models.len();
                                let default_model = commit
                                    .snapshot
                                    .config
                                    .default_model
                                    .clone()
                                    .unwrap_or_default();
                                apply_persisted_config(
                                    &mut ctx,
                                    commit.snapshot.config,
                                    commit.snapshot.revision,
                                    renderer,
                                );
                                let _ = reload_runtime_provider(&ctx);
                                renderer.render(crate::render::UiLine::CommandOutput(format!(
                                    "已接入 OpenRouter,添加 {count} 个免费模型,已切到 {default_model}。/model 可切换。"
                                )));
                                renderer.flush();
                            }
                            Err(e) => {
                                renderer.render(crate::render::UiLine::Error(format!(
                                    "OpenRouter 配置保存失败: {e}"
                                )));
                                renderer.flush();
                            }
                        }
                    }
                    OpenRouterConnectEvent::Failed(reason) => {
                        renderer.render(crate::render::UiLine::Error(format!(
                            "OpenRouter 接入失败: {reason}。可重试 /openrouter,或 /openrouter <你的key> 直接接入。"
                        )));
                        renderer.flush();
                    }
                }
            }
```

> `apply_persisted_config` / `reload_runtime_provider` 签名见 mod.rs(探查已确认存在);`grep` 确认参数(可变 ctx vs &ctx)并按实际微调。若 `apply_persisted_config` 已内部 reload,则删掉显式 `reload_runtime_provider` 调用避免重复。

- [ ] **Step 7: 编译 + 全量测试**

Run: `cargo test -p atomcode-tuix 2>&1 | grep -E "test result:|error\[" | head`
Expected: 编译通过,测试全绿。

- [ ] **Step 8: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/openrouter_connect.rs crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "$(printf 'feat(tuix): OpenRouter 后台连接任务 + 事件通道 + 装配落盘\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 7: `/openrouter [key]` 命令注册 + 分派

**Files:**
- Modify: `crates/atomcode-tuix/src/commands.rs`(`BUILTIN_COMMANDS` 加条目 + `cmd_desc_i18n` 加 arm)
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`(`execute_slash_command_impl` 的 match 加 `"openrouter" =>` 臂)

**Interfaces:**
- Consumes: Task 6 `parse_connect_mode`、`spawn_openrouter_connect`;`ctx.openrouter_event_tx`、`ctx.openrouter_cancel`、`ctx.wake_tx`。

- [ ] **Step 1: 写失败测试(命令已注册)**

`grep -n "fn registry\|CommandRegistry::" crates/atomcode-tuix/src/commands.rs` 找现有测试构造方式;在 commands.rs tests 加:

```rust
    #[test]
    fn openrouter_command_is_registered() {
        let reg = CommandRegistry::builtin();
        let cmd = reg.find("openrouter").expect("/openrouter registered");
        assert!(cmd.needs_args == false || cmd.needs_args == true); // 存在即可
        assert!(!cmd.acp, "openrouter 走 TUI-only,不进 ACP");
    }
```

> `CommandRegistry::builtin()` 名称按实际(grep 现有测试里如何取 registry)。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-tuix openrouter_command_is_registered 2>&1 | tail -15`
Expected: 失败(find 返回 None)。

- [ ] **Step 3: 注册命令**

`grep -n 'name: "provider"' crates/atomcode-tuix/src/commands.rs` 定位,`BUILTIN_COMMANDS` 内(按字母序,provider 之后 proxy 之前)加:

```rust
    Command { name: "openrouter", desc: "接入 OpenRouter 免费模型(/openrouter 或 /openrouter <key>)", needs_args: false, hidden: false, acp: false },
```

`needs_args: false` —— 无参走 OAuth 是合法用法。若 `cmd_desc_i18n(name)` 是 exhaustive match,加一条 `"openrouter" => ...`(照现有 arm 风格返回本地化描述;无对应 `Msg` 则先用中性英文字面量,后续可补 i18n)。

- [ ] **Step 4: 分派臂**

`grep -n '"provider" =>' crates/atomcode-tuix/src/event_loop/commands.rs` 定位,加新臂:

```rust
        "openrouter" => {
            let mode = crate::event_loop::openrouter_connect::parse_connect_mode(arg);
            // 每次接入前清取消标志。
            ctx.openrouter_cancel
                .store(false, std::sync::atomic::Ordering::Relaxed);
            crate::event_loop::openrouter_connect::spawn_openrouter_connect(
                mode,
                ctx.openrouter_event_tx.clone(),
                ctx.wake_tx.clone(),
                ctx.openrouter_cancel.clone(),
            );
            renderer.render(UiLine::CommandOutput(
                "正在连接 OpenRouter…(浏览器授权,或稍候)".to_string(),
            ));
            renderer.flush();
        }
```

> `ctx.wake_tx` 字段名按实际(grep OAuth 分派处如何拿 wake sender;探查显示 spawn_oauth_poll 用 `wake_tx`)。

- [ ] **Step 5: 跑测试确认通过 + 编译**

Run: `cargo test -p atomcode-tuix openrouter_command_is_registered 2>&1 | tail -15 && cargo build -p atomcode-tuix 2>&1 | tail -5`
Expected: 测试 pass,编译通过。

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/commands.rs crates/atomcode-tuix/src/event_loop/commands.rs
git commit -m "$(printf 'feat(tuix): /openrouter [key] 命令注册与分派\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 8: 触发 nudge —— 额度用尽(Trigger A)

> **对 spec 的偏差(需知悉)**:spec 描述 nudge 为 `[Enter 接入 / Esc 忽略]` 的交互式提示。交互式内联捕获键需要 modal/键路由改造。v1 改为**一行可见提示**(scrollback,指向 `/openrouter`),复用现有 `defer_background_notice` 去重机制 —— 更轻、零新键路由。若坚持交互式 Enter/Esc,升级为独立 task(引入一个轻量 nudge modal)。

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/openrouter_connect.rs`(加纯谓词)
- Modify: `crates/atomcode-tuix/src/state.rs`(UiState 加一次性标志 `pub(crate) openrouter_quota_nudge_shown: bool`,构造处默认 false)
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`(usage 刷新点接线)

**Interfaces:**
- Produces: `pub fn quota_exhausted(usage: &atomcode_codingplan::types::UsageInfo) -> bool`

- [ ] **Step 1: 写失败测试**

追加 openrouter_connect tests:

```rust
    #[test]
    fn quota_predicate_fires_at_full_usage() {
        use atomcode_codingplan::types::UsageInfo;
        let mut u = UsageInfo::default_for_test(); // 见 Step 3 说明
        u.usage_percent = 100.0;
        assert!(quota_exhausted(&u));
        u.usage_percent = 87.0;
        assert!(!quota_exhausted(&u));
    }
```

> 若 `UsageInfo` 无 `Default`/构造助手,测试改为构造一个字面量(所有字段 `#[serde(default)]`,可 `UsageInfo { usage_percent: 100.0, ..serde_json::from_str("{}").unwrap() }`)。实现前 `grep -n "pub struct UsageInfo" crates/atomcode-codingplan/src/types.rs` 确认可否 `Deserialize` from `{}`(字段全 default → 可以)。用 `serde_json::from_str::<UsageInfo>("{}").unwrap()` 造基底最稳。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-tuix quota_predicate 2>&1 | tail -15`
Expected: 失败。

- [ ] **Step 3: 实现谓词**

追加到 `openrouter_connect.rs`:

```rust
/// CodingPlan 当前窗口是否耗尽。usage_percent 以百分比计(0..=100+)。
pub fn quota_exhausted(usage: &atomcode_codingplan::types::UsageInfo) -> bool {
    usage.usage_percent >= 100.0
}
```

- [ ] **Step 4: state 标志**

`crates/atomcode-tuix/src/state.rs`:UiState 加字段(grep `deferred_background_notices` 定位邻近):

```rust
    /// 本会话是否已弹过"额度用尽 → 接入 OpenRouter"提示(一次性去重)。
    pub(crate) openrouter_quota_nudge_shown: bool,
```

构造 UiState 处默认 `openrouter_quota_nudge_shown: false`(grep UiState 构造/`Default` impl)。

- [ ] **Step 5: 接线**

`grep -n "usage_slot.lock()" crates/atomcode-tuix/src/event_loop/mod.rs`(~12276,读 usage 的 redraw 路径)或 usage 刷新后 —— 找到读取 `ctx.usage_slot` 得到 `(UsageInfo, Instant)` 的地方,在渲染 usage hint 附近加:

```rust
            if !state.openrouter_quota_nudge_shown
                && crate::event_loop::openrouter_connect::quota_exhausted(&usage_info)
            {
                state.openrouter_quota_nudge_shown = true;
                state.defer_background_notice(
                    "CodingPlan 额度已用尽 —— 输入 /openrouter 一键接入 OpenRouter 免费模型".to_string(),
                );
            }
```

> `usage_info` 变量名按实际(该处解构 `usage_slot`)。`defer_background_notice` 已存在(探查确认),会在回合终结后单行渲染并自动去重。

- [ ] **Step 6: 跑测试 + 编译**

Run: `cargo test -p atomcode-tuix quota_predicate 2>&1 | tail -10 && cargo build -p atomcode-tuix 2>&1 | tail -5`
Expected: pass + 编译通过。

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/openrouter_connect.rs crates/atomcode-tuix/src/state.rs crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "$(printf 'feat(tuix): 额度用尽时 nudge 引导 /openrouter\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 9: 触发 nudge —— 新用户未领 CodingPlan(Trigger B)

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/openrouter_connect.rs`(加 `has_codingplan` 谓词)
- Modify: `crates/atomcode-tuix/src/state.rs`(UiState 加 `pub(crate) openrouter_noplan_nudge_shown: bool`)
- Modify: `crates/atomcode-tuix/src/modals/onboarding_wizard.rs` 或 onboarding 结束处(接线)

**Interfaces:**
- Produces: `pub fn has_codingplan(config: &atomcode_config::config::Config) -> bool`

- [ ] **Step 1: 写失败测试**

追加 openrouter_connect tests:

```rust
    #[test]
    fn has_codingplan_detects_atomgit_account() {
        use atomcode_config::config::Config;
        let empty = Config::default();
        assert!(!has_codingplan(&empty));
        // 装配一个 codingplan/atomgit 账号后应为 true(用真实 account id 前缀)。
        // 具体断言在实现时对齐 has_codingplan 的判定字段。
    }
```

> 实现前 `grep -rn "codingplan\|atomgit" crates/atomcode-config/src/config/` 确认 CodingPlan 账号在 config 里的稳定标识(provider id / account id 前缀 / provider_type)。据此写 `has_codingplan` 与本测试的正例。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p atomcode-tuix has_codingplan 2>&1 | tail -15`
Expected: 失败。

- [ ] **Step 3: 实现谓词**

追加到 `openrouter_connect.rs`(判定字段按 Step 1 grep 结果对齐;下例以 provider id 含 "codingplan"/"atomgit" 为准):

```rust
/// 用户是否已有 CodingPlan 权益(据 config 里的账号判定)。
pub fn has_codingplan(config: &atomcode_config::config::Config) -> bool {
    config.logical_accounts().values().any(|a| {
        let p = a.provider.to_ascii_lowercase();
        p.contains("codingplan") || p.contains("atomgit")
    })
}
```

> `logical_accounts()` 已存在(探查确认,合并新旧 schema)。若 CodingPlan 在 legacy `providers` 里以别的 key 存,用 grep 结果调整判定。

- [ ] **Step 4: state 标志**

state.rs 加 `pub(crate) openrouter_noplan_nudge_shown: bool`,构造默认 false。

- [ ] **Step 5: 接线(onboarding 结束)**

`grep -n "pending_run_login_setup\|paint_welcome" crates/atomcode-tuix/src/event_loop/mod.rs crates/atomcode-tuix/src/modals/onboarding_wizard.rs` 定位 onboarding 收尾处。在用户结束向导、且**未**触发登录/手动配置(`!ctx.pending_run_login_setup && !ctx.pending_open_provider_wizard`)的路径上加:

```rust
            if !state.openrouter_noplan_nudge_shown
                && !crate::event_loop::openrouter_connect::has_codingplan(&ctx.config)
            {
                state.openrouter_noplan_nudge_shown = true;
                state.defer_background_notice(
                    "还没有可用模型?输入 /openrouter 一键接入 OpenRouter 免费模型".to_string(),
                );
            }
```

> 接线点须能同时看到 `state`(可变)与 `ctx.config`。若 `paint_welcome` 当前签名不带 `state`,在其**调用点**(event_loop 里)加判定,而非改 `paint_welcome` 签名 —— 减小改动面。

- [ ] **Step 6: 跑测试 + 编译**

Run: `cargo test -p atomcode-tuix has_codingplan 2>&1 | tail -10 && cargo build -p atomcode-tuix 2>&1 | tail -5`
Expected: pass + 编译通过。

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/openrouter_connect.rs crates/atomcode-tuix/src/state.rs crates/atomcode-tuix/src/event_loop/mod.rs crates/atomcode-tuix/src/modals/onboarding_wizard.rs
git commit -m "$(printf 'feat(tuix): 新用户未领 CodingPlan 时 nudge 引导 /openrouter\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

### Task 10: ESC 取消接线 + 全量回归 + 真机清单

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`(ESC 键处理:连接进行中时置 `ctx.openrouter_cancel`)

- [ ] **Step 1: ESC 取消**

`grep -n "KeyCode::Esc\|handle_key" crates/atomcode-tuix/src/event_loop/mod.rs` 找主框 ESC 处理。在合适分支(无 modal、非流式)加:连接任务在跑时,ESC 置 `ctx.openrouter_cancel.store(true, Relaxed)`,让 `wait_for_code` 尽快返回 `Ok(None)` → 任务发 `Failed("已取消或超时")`。

> 判定"连接是否在跑"可用一个轻量 bool(如 `ctx.openrouter_in_flight`,spawn 时置 true,select! 臂收到事件后置 false),或直接无条件置 cancel(幂等,无副作用)。选后者最简:ESC 时 `ctx.openrouter_cancel.store(true, ...)` 不影响未在跑的情形。

- [ ] **Step 2: 全量回归**

Run: `cargo test -p atomcode-auth 2>&1 | grep "test result:" && cargo test -p atomcode-tuix 2>&1 | grep "test result:" | head`
Expected: 全绿。

Run(全 workspace 编译,防跨 crate 破坏):`cargo build --workspace 2>&1 | tail -5`
Expected: 通过。

- [ ] **Step 3: fmt + clippy(仅本功能触碰文件,避免全仓 churn)**

Run: `cargo fmt -p atomcode-auth -p atomcode-tuix && git diff --stat`
确认只动了本功能文件(若 fmt 波及无关文件,`git checkout` 还原之)。

- [ ] **Step 4: 真机验证清单(人工,记录结果)**

- [ ] `/openrouter` 无参:浏览器打开授权页 → 授权 → 回到终端显示"已接入…N 个免费模型" → `/model` 能看到 5 个 openrouter 模型 → 切一个能正常对话。
- [ ] `/openrouter <key>`:直传 key 跳过浏览器 → 同样装配成功。
- [ ] ESC:授权等待中按 ESC → 及时显示"接入失败: 已取消",UI 不卡。
- [ ] 重启 atomcode:openrouter 账号与模型仍在(持久化生效)。
- [ ] 幂等:再次 `/openrouter <key>` 不产生重复模型,key 被更新。
- [ ] 额度用尽:CodingPlan 跑到窗口耗尽 → 回合结束后出现一行 nudge 指向 /openrouter。
- [ ] 新用户:全新环境完成 onboarding 且未领 CodingPlan → 出现 nudge。

- [ ] **Step 5: Commit(若 Step 1 有改动)**

```bash
git add crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "$(printf 'feat(tuix): OpenRouter 接入 ESC 取消接线\n\nCo-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>')"
```

---

## Self-Review

**Spec coverage:**
- 触发 A(额度用尽)→ Task 8;触发 B(新用户未领 CodingPlan)→ Task 9。✅
- OAuth PKCE 取 key → Task 1/3/4/6;`/openrouter <key>` 直传 → Task 6(ConnectMode)/Task 7。✅
- 发现 free 按 context top5 → Task 2/4。✅
- 幂等装配 + 持久 + reload → Task 5/6。✅
- `/openrouter [key]` 命令 → Task 7。✅
- 错误处理(网络/0 模型/HTTP 状态)→ Task 4/6;友好中文错误在 select! 臂。✅
- key 安全(只加 free、不入日志)→ Global Constraints + Task 5 只装 free 模型。✅
- 测试(PKCE 向量、发现过滤排序、装配幂等、谓词)→ 各 task Step。✅
- **偏差**:nudge 由交互式 Enter/Esc 降级为提示行(Task 8 顶部已标注,待用户裁定)。

**Placeholder scan:** 无 TBD/TODO;所有代码步给了完整代码或明确 grep-then-adjust 指令(因分支在动,行号/个别签名需实现时对齐,已逐处标注)。

**Type consistency:** `FreeModel`(auth crate)贯穿 Task 2/5/6;`ProvisionOutcome`/`ConnectMode`/`OpenRouterConnectEvent` 定义与消费一致;`provision_openrouter`/`parse_connect_mode`/`spawn_openrouter_connect`/`quota_exhausted`/`has_codingplan` 命名前后统一。

## 已知实现期需对齐项(分支在动,勿盲抄行号)

1. `default_context_window_for` 签名(接受 `&str` wire 还是 `ProviderType`)—— Task 5。
2. `open_browser` 在 atomcode-auth 的可见性 —— Task 6。
3. `apply_persisted_config` / `reload_runtime_provider` 参数与是否内部已 reload —— Task 6。
4. `CommandRegistry` 测试构造入口名、`cmd_desc_i18n` 是否 exhaustive —— Task 7。
5. usage 刷新读取点的变量名与最佳插入行 —— Task 8。
6. CodingPlan 账号在 config 里的稳定标识字段 —— Task 9。
7. 主框 ESC 处理分支位置 —— Task 10。
