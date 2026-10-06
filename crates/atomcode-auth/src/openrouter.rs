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
/// The page whose "Free models" list ranks free models by how much they are
/// used. Not an API: see [`parse_discover_free_ranking`].
pub const OPENROUTER_DISCOVER_URL: &str = "https://openrouter.ai/discover";
/// The account `/openrouter` writes, in both front ends.
pub const OPENROUTER_ACCOUNT_ID: &str = "openrouter";

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
    PkcePair {
        verifier,
        challenge,
    }
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

/// Every model id `/api/v1/models` lists — what OpenRouter offers at all, free
/// or not. What [`provision_with_listed`] checks the configured models against.
pub fn listed_model_ids(models_json: &str) -> Result<std::collections::HashSet<String>> {
    #[derive(Deserialize)]
    struct ModelsResp {
        data: Vec<Listed>,
    }
    #[derive(Deserialize)]
    struct Listed {
        id: String,
    }
    let resp: ModelsResp = serde_json::from_str(models_json).context("parse /models response")?;
    Ok(resp.data.into_iter().map(|m| m.id).collect())
}

/// What one look at OpenRouter's model list found: the free models to offer,
/// and every id it lists at all.
#[derive(Debug, Default, Clone)]
pub struct FreeCatalog {
    pub free: Vec<FreeModel>,
    pub listed: std::collections::HashSet<String>,
}

pub fn select_top_free_models(models_json: &str, limit: usize) -> Result<Vec<FreeModel>> {
    select_top_free_models_ranked(models_json, &[], limit)
}

/// [`select_top_free_models`], ordered by `ranking` first — the ids OpenRouter's
/// own "Free models" list names, most used first ([`parse_discover_free_ranking`])
/// — and by context length after it, for a model the ranking does not name or
/// when there is no ranking at all.
///
/// The ranking only orders. Whether a model is offered is still decided here,
/// from the API: free, takes tools, answers in text. A ranked model the API
/// says is paid, or cannot call tools, is left out like any other.
pub fn select_top_free_models_ranked(
    models_json: &str,
    ranking: &[String],
    limit: usize,
) -> Result<Vec<FreeModel>> {
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
        /// OpenRouter's per-model capability list (e.g. `"tools"`, `"reasoning"`).
        /// A model WITHOUT `"tools"` 404s a coding agent's first (tool-carrying)
        /// request with "No endpoints found that support tool use".
        #[serde(default)]
        supported_parameters: Vec<String>,
        #[serde(default)]
        architecture: Option<Architecture>,
    }
    #[derive(Deserialize)]
    struct Architecture {
        /// What the model EMITS. A chat model outputs `"text"`; a media model
        /// (e.g. Google Lyria music-gen) outputs `"audio"`/`"image"` — free and
        /// large-context, but unusable for chat/tool use, so it must be excluded.
        #[serde(default)]
        output_modalities: Vec<String>,
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
        // Capability gate — only surface models a coding agent can actually use,
        // so the auto-added set never includes a model that 404/403s on the first
        // message (the reported symptom: Google Lyria music-gen 404'd "no tool
        // use", picked purely because it was free with a huge context window).
        .filter(|m| {
            // 1) Must support tool calls (atomcode's first request carries tools).
            m.supported_parameters.iter().any(|p| p == "tools")
                // 2) Must EMIT text (chat), not audio/image. Absent architecture
                //    (older/partial API rows) is not judged on modality.
                && match &m.architecture {
                    Some(a) if !a.output_modalities.is_empty() => {
                        a.output_modalities.iter().any(|x| x == "text")
                    }
                    _ => true,
                }
        })
        .map(|m| FreeModel {
            id: m.id,
            name: m.name,
            context_length: m.context_length,
        })
        .collect();
    // 榜单上的按榜单顺序在前;其余按 context 降序;并列时按 id 稳定排序,保证测试确定性。
    let rank = |id: &str| ranking.iter().position(|r| r == id).unwrap_or(usize::MAX);
    free.sort_by(|a, b| {
        rank(&a.id)
            .cmp(&rank(&b.id))
            .then_with(|| b.context_length.cmp(&a.context_length))
            .then_with(|| a.id.cmp(&b.id))
    });
    free.truncate(limit);
    Ok(free)
}

/// The ids OpenRouter's "Free models" list (<https://openrouter.ai/discover>)
/// names, most used first, as `/api/v1/models` spells them.
///
/// **Not an API.** The page is rendered on the server and carries its data in
/// the `self.__next_f.push([1,"…"])` chunks a Next.js page streams; the list is
/// the object `{"id":"free", …, "models":[…], "sheetModels":[…]}` in them
/// (`sheetModels` is the "View more" list, `models` its first five). Its ids are
/// dated variants — `nvidia/x-20260604:free` — and the date is dropped to get
/// the API's `nvidia/x:free`. Anything that does not have that shape gives an
/// empty ranking, and the caller falls back to ordering by context length: the
/// page can change without notice, and a page that did must not stop
/// `/openrouter` from working.
pub fn parse_discover_free_ranking(html: &str) -> Vec<String> {
    const CHUNK: &str = "self.__next_f.push(";
    let mut text = String::new();
    let mut rest = html;
    while let Some(at) = rest.find(CHUNK) {
        rest = rest.get(at + CHUNK.len()..).unwrap_or("");
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>();
        match values.next() {
            Some(Ok(serde_json::Value::Array(items))) => {
                if let Some(serde_json::Value::String(chunk)) = items.get(1) {
                    text.push_str(chunk);
                }
            }
            // A chunk that runs to the end of the page without closing: every
            // later one is inside it, and re-reading from each would read the
            // rest of the page once per chunk. What was read so far is all
            // there is.
            Some(Err(error)) if error.is_eof() => break,
            _ => {}
        }
    }
    const SECTION: &str = r#"{"id":"free","#;
    let mut rest = text.as_str();
    while let Some(at) = rest.find(SECTION) {
        let from = rest.get(at..).unwrap_or("");
        rest = rest.get(at + SECTION.len()..).unwrap_or("");
        let mut values = serde_json::Deserializer::from_str(from).into_iter::<serde_json::Value>();
        let Some(Ok(section)) = values.next() else {
            continue;
        };
        let list = section
            .get("sheetModels")
            .or_else(|| section.get("models"))
            .and_then(serde_json::Value::as_array);
        let Some(list) = list else {
            continue;
        };
        let mut ids: Vec<String> = Vec::new();
        for slug in list.iter().filter_map(|m| {
            m.get("variantPermaslug")
                .and_then(serde_json::Value::as_str)
        }) {
            let id = undated(slug);
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        if !ids.is_empty() {
            return ids;
        }
    }
    Vec::new()
}

/// `vendor/model-20260604:free` → `vendor/model:free`: a dated variant's id as
/// the models API names it. An id without an 8-digit date before the optional
/// `:free` is returned as it is.
fn undated(slug: &str) -> String {
    let (base, free) = match slug.strip_suffix(":free") {
        Some(base) => (base, ":free"),
        None => (slug, ""),
    };
    let dated = base.len() > 9
        && base.is_char_boundary(base.len() - 9)
        && base.get(base.len() - 9..).is_some_and(|tail| {
            tail.starts_with('-') && tail[1..].bytes().all(|b| b.is_ascii_digit())
        });
    match dated {
        true => format!("{}{free}", base.get(..base.len() - 9).unwrap_or(base)),
        false => slug.to_string(),
    }
}

/// How long the optional ranking page may take before `/openrouter` goes on
/// without it.
const DISCOVER_TIMEOUT: Duration = Duration::from_secs(5);

/// The most of the ranking page that is read.
const DISCOVER_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Fetch the "Free models" ranking. Empty on any failure — see
/// [`parse_discover_free_ranking`] for why that is the right answer.
fn fetch_discover_ranking(client: &reqwest::blocking::Client) -> Vec<String> {
    // Shorter than the client's own budget: the ranking only orders what the
    // models API already answered, so a slow page is not worth waiting for.
    let fetched = client
        .get(OPENROUTER_DISCOVER_URL)
        .timeout(DISCOVER_TIMEOUT)
        .send()
        .and_then(|resp| resp.error_for_status())
        .map_err(|error| error.to_string())
        .and_then(|resp| {
            // Bounded: the page is about 1MB, and nothing about it is worth more.
            use std::io::Read as _;
            let mut body = Vec::new();
            resp.take(DISCOVER_MAX_BYTES)
                .read_to_end(&mut body)
                .map_err(|error| error.to_string())?;
            Ok(String::from_utf8_lossy(&body).into_owned())
        });
    match fetched {
        Ok(html) => {
            let ranking = parse_discover_free_ranking(&html);
            if ranking.is_empty() {
                tracing::warn!(
                    "openrouter: no free-model ranking on the discover page; ordering by context"
                );
            }
            ranking
        }
        Err(error) => {
            tracing::warn!("openrouter: discover page not fetched ({error}); ordering by context");
            Vec::new()
        }
    }
}

/// What [`provision`] did to the configuration.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Provisioned {
    /// Free models added this time, by selection id.
    pub added: Vec<String>,
    /// Free models a previous run added that are no longer among the current
    /// ones — gone from the list, paid now, or overtaken — and were removed.
    /// Includes [`Self::retired`].
    pub removed: Vec<String>,
    /// Of `removed`, entries this command did not mark — added by hand, or by
    /// a build older than the mark — taken out because OpenRouter does not
    /// offer the model at all any more: it cannot answer anybody, and kept it
    /// stays on the list to be picked and fail.
    pub retired: Vec<String>,
    /// The default model after this run.
    pub default_model: Option<String>,
    /// The default this run replaced, when the default was one of the removed
    /// free models: a default naming a model that is gone would be worse.
    pub default_replaced: Option<String>,
}

/// Write the OpenRouter account and its current free models into `config`.
/// The one implementation both front ends call, so `/openrouter` in either
/// leaves the same configuration.
///
/// - **The account** has a fixed id; an existing one only has its key replaced.
/// - **The free models are a set this command owns**, marked with
///   [`OPENROUTER_FREE_ORIGIN`] (`ModelProfileConfig::origin`). Run again, it
///   swaps that set for `models`: marked entries no longer among them are
///   removed, ones still among them are refreshed, new ones are added. Free
///   models change often and a free one can start costing money, so keeping the
///   old ones would be keeping a bill the person did not ask for.
/// - **Anything unmarked is the person's** and is never changed or removed —
///   an entry they added by hand, one they have since saved from a panel
///   (saving clears the mark; see `ModelProfileConfig::origin` for what does
///   not), or one an older build added before the mark existed. The same
///   selection id among `models` does not take it over.
/// - **The default** is set when there is none (a legacy `default_provider`
///   that names something counts as one), and moved to the first of `models`
///   when it named a free model this run removed. A default the person chose
///   is left alone. A window that is using a removed model while the default is
///   something else is the front end's to move (see the classic UI's handling).
/// - **No models** changes nothing but the key: a fetch that found none is not
///   a reason to empty the set.
pub fn provision(
    config: &mut atomcode_config::config::Config,
    api_key: &str,
    models: &[FreeModel],
) -> Provisioned {
    provision_with_listed(config, api_key, models, &std::collections::HashSet::new())
}

/// [`provision`], knowing every model OpenRouter lists (`listed`, from the same
/// `/api/v1/models` the free ones were picked from).
///
/// One more thing is removed then, whoever added it: an entry on the OpenRouter
/// account whose model OpenRouter no longer offers at all. Its being unmarked
/// is why `provision` leaves the person's own models alone, and that is still
/// so for every model OpenRouter offers; one it has taken down cannot be
/// called by anyone, and left in place it is picked and answers 404 ("No
/// endpoints found"). An empty `listed` says nothing is known, and nothing
/// more is removed.
///
/// A variant suffix the list does not spell out (`x:online`, `x:nitro`) is
/// judged by its base model; `:free` is not a variant but a model of its own,
/// so a free model gone while its paid twin stays is gone. Only ids shaped like
/// the list's own — `vendor/model`, no `@`/`~` prefix — are judged at all:
/// OpenRouter answers ids it never lists (`@preset/…`), and an entry that is
/// not on the list's terms is not known to be gone. Case is not a difference.
pub fn provision_with_listed(
    config: &mut atomcode_config::config::Config,
    api_key: &str,
    models: &[FreeModel],
    listed: &std::collections::HashSet<String>,
) -> Provisioned {
    use atomcode_config::config::provider::{
        default_context_window_for, ModelProfileConfig, ProviderAccountConfig,
        OPENROUTER_FREE_ORIGIN,
    };
    use atomcode_config::config::provider_preset::preset_or_compatible;

    let provider_type = preset_or_compatible(OPENROUTER_ACCOUNT_ID)
        .provider_type
        .wire()
        .to_string();
    config
        .provider_accounts
        .entry(OPENROUTER_ACCOUNT_ID.to_string())
        .and_modify(|account| account.api_key = Some(api_key.to_string()))
        .or_insert_with(|| ProviderAccountConfig {
            provider: OPENROUTER_ACCOUNT_ID.to_string(),
            display_name: None,
            api_key: Some(api_key.to_string()),
            base_url: None,
            user_agent: None,
            skip_tls_verify: false,
            enterprise_url: None,
            ephemeral: false,
        });
    // Nothing current is no reason to throw away what there is: an empty list
    // is a fetch that found nothing, not OpenRouter saying every free model
    // went. Only the key is taken.
    if models.is_empty() {
        return Provisioned {
            default_model: config.default_model.clone(),
            ..Provisioned::default()
        };
    }

    let managed = |m: &ModelProfileConfig| {
        m.account == OPENROUTER_ACCOUNT_ID && m.origin.as_deref() == Some(OPENROUTER_FREE_ORIGIN)
    };
    let wanted: Vec<String> = models
        .iter()
        .map(|m| format!("{OPENROUTER_ACCOUNT_ID}/{}", m.id))
        .collect();

    let listed: std::collections::HashSet<String> =
        listed.iter().map(|id| id.to_ascii_lowercase()).collect();
    let judged = |model: &str| {
        !model.starts_with(['@', '~'])
            && model
                .split_once('/')
                .is_some_and(|(vendor, name)| !vendor.is_empty() && !name.is_empty())
    };
    let offered = |model: &str| {
        let model = model.to_ascii_lowercase();
        listed.contains(&model)
            || model
                .rsplit_once(':')
                .is_some_and(|(base, variant)| variant != "free" && listed.contains(base))
    };
    let retired: Vec<String> = match listed.is_empty() {
        true => Vec::new(),
        false => config
            .models
            .iter()
            .filter(|(id, m)| {
                m.account == OPENROUTER_ACCOUNT_ID
                    && !managed(m)
                    && !wanted.contains(id)
                    && judged(&m.model)
                    && !offered(&m.model)
            })
            .map(|(id, _)| id.clone())
            .collect(),
    };
    let mut removed: Vec<String> = config
        .models
        .iter()
        .filter(|(id, m)| managed(m) && !wanted.contains(id))
        .map(|(id, _)| id.clone())
        .collect();
    removed.extend(retired.iter().cloned());
    for id in &removed {
        config.models.remove(id);
    }

    let window = |m: &FreeModel| {
        if m.context_length > 0 {
            m.context_length as usize
        } else {
            default_context_window_for(&provider_type)
        }
    };
    let mut added = Vec::new();
    // `models` arrives in the order to offer them (OpenRouter's own ranking,
    // `select_top_free_models_ranked`); the map forgets it, so it is written down.
    for (at, (selection, model)) in wanted.iter().zip(models).enumerate() {
        let rank = Some(u32::try_from(at + 1).unwrap_or(u32::MAX));
        if let Some(existing) = config.models.get_mut(selection) {
            // Ours: refreshed. The person's own entry under the same id: theirs,
            // untouched.
            if managed(existing) {
                existing.display_name = model.name.clone();
                existing.context_window = window(model);
                existing.rank = rank;
            }
            continue;
        }
        match config.selection_exists(selection) {
            // A legacy `[providers.<id>]` entry answers to this id: also theirs.
            true => {}
            false => {
                config.models.insert(
                    selection.clone(),
                    ModelProfileConfig {
                        account: OPENROUTER_ACCOUNT_ID.to_string(),
                        model: model.id.clone(),
                        display_name: model.name.clone(),
                        system_prompt: None,
                        supports_vision: None,
                        context_window: window(model),
                        max_tokens: None,
                        capable_model: None,
                        note: None,
                        thinking_type: None,
                        thinking_keep: None,
                        reasoning_history: None,
                        reasoning_effort: None,
                        reasoning_effort_levels: None,
                        thinking_enabled: None,
                        thinking_budget: None,
                        retry_max_attempts: None,
                        origin: Some(OPENROUTER_FREE_ORIGIN.to_string()),
                        rank,
                    },
                );
                added.push(selection.clone());
            }
        }
    }

    let first = wanted.first().cloned();
    let mut default_replaced = None;
    // A legacy `default_provider` naming something that exists is a default the
    // person chose; `default_model` would take precedence over it, so it is not
    // set on top.
    let legacy_default = !config.default_provider.trim().is_empty()
        && config.selection_exists(&config.default_provider);
    match config.default_model.clone() {
        None if legacy_default => {}
        None => config.default_model = first,
        Some(current) if removed.contains(&current) && first.is_some() => {
            config.default_model = first;
            default_replaced = Some(current);
        }
        Some(_) => {}
    }
    Provisioned {
        added,
        removed,
        retired,
        default_model: config.default_model.clone(),
        default_replaced,
    }
}

pub fn parse_code_from_request_line(line: &str) -> Option<String> {
    // "GET /callback?code=XXX&foo=bar HTTP/1.1" → XXX
    let target = line.split_whitespace().nth(1)?; // "/callback?code=..."
    let query = target.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("code=") {
            if !v.is_empty() {
                // query 值可能被 percent-encode(如 code 含 '+' → %2B);
                // 解码后再交换,否则编码后的字符串与服务端 code 不匹配 → 401。
                return Some(percent_decode(v));
            }
        }
    }
    None
}

/// 最小 percent-decode:`%XX` → 字节,`+` 保持原样(query 值里 '+' 不代表空格,
/// OAuth code 用的是 application/x-www-form 之外的原始 query)。非法 `%XX` 原样保留。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
    ///
    /// 无 code 的连接(浏览器预检、favicon 等)会收到一个响应但被跳过,循环继续等待真正
    /// 带 code 的回调。慢连接建立后若 2s 内不发送请求,视为无效连接同样跳过。
    pub fn wait_for_code(self, timeout: Duration, cancel: &AtomicBool) -> Result<Option<String>> {
        let deadline = Instant::now() + timeout;
        let success_body = "<html><body>已接入 OpenRouter,可关闭此页返回终端。</body></html>";
        let waiting_body = "<html><body>等待授权中,请稍候...</body></html>";
        loop {
            if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Ok(None);
            }
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    // 关键:listener 是非阻塞的,macOS/BSD 上 accept 出的 stream 会
                    // 继承 O_NONBLOCK。此时 set_read_timeout 无效、read 会在浏览器
                    // 的 HTTP 请求到达前立即返回 WouldBlock,导致真实回调被当作无效
                    // 连接丢弃、OAuth 卡到超时。显式转回阻塞,让下方的 read 超时生效。
                    let _ = stream.set_nonblocking(false);
                    // F1: 设置 read 超时,防止慢连接建立后迟迟不发请求时无限阻塞,
                    // 使 cancel/deadline 轮询得以继续。
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));

                    let mut buf = [0u8; 2048];
                    let n = match stream.read(&mut buf) {
                        Ok(0) | Err(_) => {
                            // F1: 读超时(WouldBlock/TimedOut)或连接提前关闭 → 无效连接,
                            // 丢弃并 continue 重新检查 cancel/deadline。
                            continue;
                        }
                        Ok(n) => n,
                    };

                    let text = String::from_utf8_lossy(&buf[..n]);
                    let first_line = text.lines().next().unwrap_or("");
                    let code = parse_code_from_request_line(first_line);

                    if let Some(ref c) = code {
                        // 真正的回调:回成功页面,返回 code。
                        let _ = stream.write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                success_body.as_bytes().len(),
                                success_body,
                            )
                            .as_bytes(),
                        );
                        return Ok(Some(c.clone()));
                    }

                    // F2: 无 code(预检、favicon 等)→ 写一个响应后 continue,继续等待。
                    let _ = stream.write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            waiting_body.as_bytes().len(),
                            waiting_body,
                        )
                        .as_bytes(),
                    );
                    // 继续外层循环,等待真正带 code 的回调。
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
    // Reuse the shared proxy-aware factory (connect 5s / total 10s / user-agent
    // + system-proxy policy). A bare `Client::builder()` here would bypass the
    // `/proxy` / no-proxy handling, so on corporate networks the code→key
    // exchange and /models fetch could ignore the configured proxy. OpenRouter
    // is a standard TLS 1.3 endpoint, so no TLS 1.2 cap (force_tls12 = false).
    crate::oauth::blocking_client_with_tls12(false)
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

/// GET /api/v1/models(Bearer)→ 过滤 free,按 OpenRouter「Free models」榜单(用量)
/// 排序、榜单外按 context 降序,取 limit。榜单取不到时整体退回按 context 排。
pub fn fetch_top_free_models(api_key: &str, limit: usize) -> Result<Vec<FreeModel>> {
    fetch_free_catalog(api_key, limit).map(|catalog| catalog.free)
}

/// [`fetch_top_free_models`], with every id the list carries beside them — one
/// request for both, for [`provision_with_listed`].
pub fn fetch_free_catalog(api_key: &str, limit: usize) -> Result<FreeCatalog> {
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
    let ranking = fetch_discover_ranking(&client);
    Ok(FreeCatalog {
        free: select_top_free_models_ranked(&body, &ranking, limit)?,
        listed: listed_model_ids(&body)?,
    })
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
        assert!(
            (43..=128).contains(&p.verifier.len()),
            "len={}",
            p.verifier.len()
        );
        assert!(p
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)));
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
        assert_eq!(
            parse_key_response(r#"{"key":"sk-or-v1-abc"}"#).unwrap(),
            "sk-or-v1-abc"
        );
    }

    #[test]
    fn parse_key_errors_on_missing() {
        assert!(parse_key_response(r#"{"error":"bad"}"#).is_err());
    }

    const MODELS_FIXTURE: &str = r#"{
      "data": [
        {"id":"vendor/big:free","name":"Big Free","context_length":128000,
         "pricing":{"prompt":"0","completion":"0"},
         "supported_parameters":["tools","temperature"],
         "architecture":{"output_modalities":["text"]}},
        {"id":"vendor/paid","name":"Paid","context_length":200000,
         "pricing":{"prompt":"0.001","completion":"0.002"},
         "supported_parameters":["tools"]},
        {"id":"vendor/small:free","name":"Small Free","context_length":8000,
         "pricing":{"prompt":"0","completion":"0"},
         "supported_parameters":["tools"],
         "architecture":{"output_modalities":["text"]}},
        {"id":"vendor/zero-priced","name":"Zero Priced","context_length":32000,
         "pricing":{"prompt":"0","completion":"0"},
         "supported_parameters":["tools"],
         "architecture":{"output_modalities":["text"]}},
        {"id":"vendor/nopricing","context_length":16000,
         "supported_parameters":["tools"]},
        {"id":"vendor/music:free","name":"Music Gen","context_length":256000,
         "pricing":{"prompt":"0","completion":"0"},
         "supported_parameters":["temperature"],
         "architecture":{"output_modalities":["audio"]}},
        {"id":"vendor/notools:free","name":"No Tools","context_length":300000,
         "pricing":{"prompt":"0","completion":"0"},
         "architecture":{"output_modalities":["text"]}}
      ]
    }"#;

    #[test]
    fn top_free_filters_paid_and_sorts_by_context_desc() {
        let got = select_top_free_models(MODELS_FIXTURE, 5).unwrap();
        // paid 被剔除;nopricing 无 pricing 字段 → 不视为 free(保守),被剔除。
        let ids: Vec<&str> = got.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["vendor/big:free", "vendor/zero-priced", "vendor/small:free"]
        );
    }

    #[test]
    fn top_free_excludes_non_tool_and_non_text_models() {
        // `music:free` (audio output) and `notools:free` (no `tools` support)
        // are the LARGEST free contexts, so a price-only filter would surface
        // them first — but they 404 on chat/tool use. They must be excluded.
        let got = select_top_free_models(MODELS_FIXTURE, 5).unwrap();
        let ids: Vec<&str> = got.iter().map(|m| m.id.as_str()).collect();
        assert!(
            !ids.contains(&"vendor/music:free"),
            "audio-gen model excluded"
        );
        assert!(
            !ids.contains(&"vendor/notools:free"),
            "non-tool model excluded"
        );
    }

    #[test]
    fn top_free_respects_limit() {
        let got = select_top_free_models(MODELS_FIXTURE, 2).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "vendor/big:free"); // 最大 context 优先
    }

    /// OpenRouter's own ranking orders what is offered; the API still decides
    /// what may be offered. A ranked model the API says cannot call tools stays
    /// out, and models the ranking does not name follow by context length.
    #[test]
    fn a_ranking_orders_the_free_models_and_the_api_still_filters_them() {
        let ranking = vec![
            "vendor/small:free".to_string(),
            "vendor/notools:free".to_string(),
            "vendor/zero-priced".to_string(),
        ];
        let got = select_top_free_models_ranked(MODELS_FIXTURE, &ranking, 5).unwrap();
        let ids: Vec<&str> = got.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["vendor/small:free", "vendor/zero-priced", "vendor/big:free"]
        );
    }

    /// The page as it streams: the list split across two chunks, escaped the
    /// way a JS string escapes it, with dated ids.
    const DISCOVER_FIXTURE: &str = r#"<html><script>self.__next_f.push([1,"prefix {\"id\":\"popular\",\"models\":[]} {\"id\":\"free\",\"title\":\"Free models\",\"models\":[{\"variantPermaslug\":\"stealth/space-bunny-alpha\"}],\"sheetModels\":[{\"variantPermaslug\":\"stealth/space-bunny-alpha\"},"])</script><script>self.__next_f.push([1,"{\"variantPermaslug\":\"nvidia/nemotron-3-ultra-550b-a55b-20260604:free\"},{\"variantPermaslug\":\"dots-studio/dots-3-note-preview-20260813:free\"},{\"variantPermaslug\":\"dots-studio/dots-3-note-preview-20260813\"}]} tail"])</script></html>"#;

    #[test]
    fn the_discover_page_ranking_is_read_in_order_with_the_dates_dropped() {
        assert_eq!(
            parse_discover_free_ranking(DISCOVER_FIXTURE),
            vec![
                "stealth/space-bunny-alpha",
                "nvidia/nemotron-3-ultra-550b-a55b:free",
                "dots-studio/dots-3-note-preview:free",
                "dots-studio/dots-3-note-preview",
            ]
        );
    }

    /// A page that changed shape is no ranking, not an error: `/openrouter`
    /// goes on ordering by context length.
    #[test]
    fn a_page_without_the_list_is_no_ranking() {
        assert!(parse_discover_free_ranking("").is_empty());
        assert!(parse_discover_free_ranking("<html>nothing here</html>").is_empty());
        assert!(parse_discover_free_ranking(
            r#"self.__next_f.push([1,"{\"id\":\"free\",\"models\":\"not a list\"}"])"#
        )
        .is_empty());
        assert!(
            parse_discover_free_ranking(r#"self.__next_f.push([1,"{\"id\":\"free\", broken"#)
                .is_empty()
        );
    }

    /// A page cut off inside a chunk — a truncated download, a changed page —
    /// is no ranking, quickly, rather than an error or a hang.
    #[test]
    fn a_chunk_that_never_closes_ends_the_read() {
        let mut page = String::from(r#"self.__next_f.push([1,"never closed "#);
        for _ in 0..20_000 {
            page.push_str(r#"self.__next_f.push([1,"x"]) "#);
        }
        let started = std::time::Instant::now();
        assert!(parse_discover_free_ranking(&page).is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "read in one pass: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_dated_id_loses_its_date_and_nothing_else() {
        assert_eq!(undated("a/b-20260604:free"), "a/b:free");
        assert_eq!(undated("a/b-20260604"), "a/b");
        assert_eq!(undated("a/b:free"), "a/b:free");
        assert_eq!(undated("a/qwen3-2507"), "a/qwen3-2507", "not eight digits");
        assert_eq!(undated("-20260604"), "-20260604", "nothing before the date");
    }

    mod provisioning {
        use super::super::*;
        use atomcode_config::config::provider::{ModelProfileConfig, OPENROUTER_FREE_ORIGIN};
        use atomcode_config::config::Config;

        fn free(id: &str) -> FreeModel {
            FreeModel {
                id: id.to_string(),
                name: Some(format!("{id} (free)")),
                context_length: 128_000,
            }
        }

        fn own(account: &str, model: &str) -> ModelProfileConfig {
            let mut c = Config::default();
            provision(&mut c, "k", &[free(model)]);
            let mut m = c.models.remove(&format!("openrouter/{model}")).unwrap();
            m.account = account.to_string();
            m.origin = None;
            m.rank = None;
            m.context_window = 7;
            m
        }

        fn listed(ids: &[&str]) -> std::collections::HashSet<String> {
            ids.iter().map(|s| s.to_string()).collect()
        }

        /// The reported case: a model an older build added, unmarked, that
        /// OpenRouter has since taken down. It is removed — whoever added it, it
        /// cannot answer anybody — and the default that named it moves on.
        /// Everything OpenRouter still offers stays the person's.
        #[test]
        fn a_model_openrouter_no_longer_offers_is_removed_whoever_added_it() {
            let mut c = Config::default();
            c.models.insert(
                "openrouter/stealth/space-bunny-alpha".into(),
                own("openrouter", "stealth/space-bunny-alpha"),
            );
            c.models.insert(
                "openrouter/openai/gpt-5".into(),
                own("openrouter", "openai/gpt-5"),
            );
            c.models.insert(
                "openrouter/openai/gpt-5:online".into(),
                own("openrouter", "openai/gpt-5:online"),
            );
            c.models.insert(
                "openrouter/vendor/gone:free".into(),
                own("openrouter", "vendor/gone:free"),
            );
            c.models.insert(
                "elsewhere/stealth".into(),
                own("elsewhere", "stealth/space-bunny-alpha"),
            );
            c.models.insert(
                "openrouter/@preset/my-coding".into(),
                own("openrouter", "@preset/my-coding"),
            );
            c.models.insert(
                "openrouter/OpenAI/GPT-5".into(),
                own("openrouter", "OpenAI/GPT-5"),
            );
            c.default_model = Some("openrouter/stealth/space-bunny-alpha".into());

            let out = provision_with_listed(
                &mut c,
                "k",
                &[free("a/x:free")],
                &listed(&["a/x:free", "openai/gpt-5", "vendor/gone"]),
            );

            let mut retired = out.retired.clone();
            retired.sort();
            assert_eq!(
                retired,
                vec![
                    "openrouter/stealth/space-bunny-alpha",
                    "openrouter/vendor/gone:free"
                ],
                "taken down: gone, and a free model whose paid twin stays"
            );
            assert!(out.retired.iter().all(|id| out.removed.contains(id)));
            assert!(
                c.models.contains_key("openrouter/openai/gpt-5"),
                "offered: theirs"
            );
            assert!(
                c.models.contains_key("openrouter/openai/gpt-5:online"),
                "a variant of an offered model: theirs"
            );
            assert!(
                c.models.contains_key("elsewhere/stealth"),
                "another account: not ours to judge"
            );
            assert!(
                c.models.contains_key("openrouter/@preset/my-coding"),
                "an id the list never carries is not known to be gone"
            );
            assert!(
                c.models.contains_key("openrouter/OpenAI/GPT-5"),
                "case is not a difference"
            );
            assert_eq!(
                out.default_replaced.as_deref(),
                Some("openrouter/stealth/space-bunny-alpha")
            );
            assert_eq!(c.default_model.as_deref(), Some("openrouter/a/x:free"));
        }

        /// Not knowing what OpenRouter lists is not knowing a model is gone.
        #[test]
        fn without_the_list_nothing_unmarked_is_removed() {
            let mut c = Config::default();
            c.models.insert(
                "openrouter/stealth/space-bunny-alpha".into(),
                own("openrouter", "stealth/space-bunny-alpha"),
            );
            let out = provision(&mut c, "k", &[free("a/x:free")]);
            assert!(out.retired.is_empty());
            assert!(c
                .models
                .contains_key("openrouter/stealth/space-bunny-alpha"));
        }

        #[test]
        fn every_listed_id_is_read_off_the_models_response() {
            let ids = listed_model_ids(
                r#"{"data":[{"id":"a/x:free"},{"id":"openai/gpt-5","name":"GPT"}]}"#,
            )
            .unwrap();
            assert_eq!(ids, listed(&["a/x:free", "openai/gpt-5"]));
        }

        #[test]
        fn a_fresh_config_gets_the_account_the_models_and_a_default() {
            let mut c = Config::default();
            let out = provision(&mut c, "sk-1", &[free("a/x:free"), free("b/y:free")]);
            assert_eq!(
                c.provider_accounts[OPENROUTER_ACCOUNT_ID]
                    .api_key
                    .as_deref(),
                Some("sk-1")
            );
            assert!(!c.provider_accounts[OPENROUTER_ACCOUNT_ID].ephemeral);
            assert_eq!(
                out.added,
                vec!["openrouter/a/x:free", "openrouter/b/y:free"]
            );
            assert!(out.removed.is_empty());
            assert_eq!(out.default_model.as_deref(), Some("openrouter/a/x:free"));
            assert_eq!(c.default_model.as_deref(), Some("openrouter/a/x:free"));
            assert_eq!(
                c.models["openrouter/a/x:free"].origin.as_deref(),
                Some(OPENROUTER_FREE_ORIGIN),
                "marked as this command's"
            );
        }

        /// The order they came in — OpenRouter's own ranking — is written down,
        /// because `[models.*]` is a map and would forget it. A second run with
        /// the ranking changed moves the ones it keeps to their new place.
        #[test]
        fn the_ranking_is_kept_and_follows_a_rerun() {
            let mut c = Config::default();
            provision(&mut c, "k", &[free("z/top:free"), free("a/next:free")]);
            let rank = |c: &Config, id: &str| c.models[id].rank;
            assert_eq!(rank(&c, "openrouter/z/top:free"), Some(1));
            assert_eq!(rank(&c, "openrouter/a/next:free"), Some(2));

            provision(&mut c, "k", &[free("a/next:free"), free("z/top:free")]);
            assert_eq!(rank(&c, "openrouter/a/next:free"), Some(1));
            assert_eq!(rank(&c, "openrouter/z/top:free"), Some(2));
        }

        /// **Run again, the set is swapped.** What it added before and is no
        /// longer current goes; what is still current stays, refreshed; what is
        /// new comes in. The key is replaced, and nothing is doubled.
        #[test]
        fn running_again_swaps_the_free_models_it_added() {
            let mut c = Config::default();
            provision(&mut c, "sk-old", &[free("a/x:free"), free("b/y:free")]);
            let mut newer = free("b/y:free");
            newer.context_length = 1_000_000;
            let out = provision(&mut c, "sk-new", &[newer, free("c/z:free")]);
            assert_eq!(out.removed, vec!["openrouter/a/x:free"]);
            assert_eq!(out.added, vec!["openrouter/c/z:free"]);
            assert!(!c.models.contains_key("openrouter/a/x:free"));
            assert_eq!(
                c.models["openrouter/b/y:free"].context_window, 1_000_000,
                "refreshed"
            );
            assert_eq!(c.models.len(), 2);
            assert_eq!(
                c.provider_accounts[OPENROUTER_ACCOUNT_ID]
                    .api_key
                    .as_deref(),
                Some("sk-new")
            );
        }

        /// **The person's own models are never touched** — one they added by
        /// hand under the same id, one on the same account, one an older build
        /// added without the mark, one on another account.
        #[test]
        fn the_persons_own_models_are_never_changed_or_removed() {
            let mut c = Config::default();
            c.models
                .insert("openrouter/a/x:free".into(), own("openrouter", "a/x:free"));
            c.models
                .insert("openrouter/mine".into(), own("openrouter", "mine"));
            c.models.insert("elsewhere/m".into(), own("elsewhere", "m"));
            let before = format!("{:?}", c.models);

            let out = provision(&mut c, "k", &[free("a/x:free"), free("d/w:free")]);
            assert!(out.removed.is_empty(), "{:?}", out.removed);
            assert_eq!(
                out.added,
                vec!["openrouter/d/w:free"],
                "the same id is not taken over"
            );
            assert_eq!(c.models["openrouter/a/x:free"].origin, None);
            assert_eq!(
                c.models["openrouter/a/x:free"].context_window, 7,
                "not refreshed"
            );

            // And again with none of them among the current set: still there.
            provision(&mut c, "k", &[free("e/v:free")]);
            for id in ["openrouter/a/x:free", "openrouter/mine", "elsewhere/m"] {
                assert!(c.models.contains_key(id), "{id} was removed");
            }
            assert!(
                !c.models.contains_key("openrouter/d/w:free"),
                "but its own one went"
            );
            let _ = before;
        }

        /// A default naming a free model this run removed moves to the first
        /// current one, and says which it replaced. A default the person chose
        /// stays.
        #[test]
        fn a_default_on_a_removed_free_model_moves_and_a_chosen_one_stays() {
            let mut c = Config::default();
            provision(&mut c, "k", &[free("a/x:free")]);
            assert_eq!(c.default_model.as_deref(), Some("openrouter/a/x:free"));
            let out = provision(&mut c, "k", &[free("b/y:free")]);
            assert_eq!(out.default_replaced.as_deref(), Some("openrouter/a/x:free"));
            assert_eq!(c.default_model.as_deref(), Some("openrouter/b/y:free"));

            c.default_model = Some("elsewhere/m".into());
            let out = provision(&mut c, "k", &[free("c/z:free")]);
            assert_eq!(out.default_replaced, None);
            assert_eq!(c.default_model.as_deref(), Some("elsewhere/m"));
            assert_eq!(out.default_model.as_deref(), Some("elsewhere/m"));
        }

        /// A fetch that found nothing empties nothing: only the key is taken.
        #[test]
        fn no_models_changes_nothing_but_the_key() {
            let mut c = Config::default();
            provision(&mut c, "old", &[free("a/x:free")]);
            let out = provision(&mut c, "new", &[]);
            assert!(out.removed.is_empty());
            assert!(c.models.contains_key("openrouter/a/x:free"));
            assert_eq!(c.default_model.as_deref(), Some("openrouter/a/x:free"));
            assert_eq!(
                c.provider_accounts[OPENROUTER_ACCOUNT_ID]
                    .api_key
                    .as_deref(),
                Some("new")
            );
        }

        /// A legacy `default_provider` that names something is a default the
        /// person chose; `default_model` would override it, so it is not set.
        #[test]
        fn a_legacy_default_is_a_default() {
            let mut c: Config = serde_json::from_value(serde_json::json!({
                "default_provider": "mine",
                "providers": { "mine": { "type": "openai", "model": "m" } }
            }))
            .unwrap();
            let out = provision(&mut c, "k", &[free("a/x:free")]);
            assert_eq!(c.default_model, None);
            assert_eq!(out.default_replaced, None);
            assert_eq!(c.default_provider, "mine");
        }

        /// A default on a free model that is still current is not moved.
        #[test]
        fn a_default_on_a_current_free_model_stays() {
            let mut c = Config::default();
            provision(&mut c, "k", &[free("a/x:free"), free("b/y:free")]);
            c.default_model = Some("openrouter/b/y:free".into());
            let out = provision(&mut c, "k", &[free("a/x:free"), free("b/y:free")]);
            assert_eq!(out.default_replaced, None);
            assert_eq!(c.default_model.as_deref(), Some("openrouter/b/y:free"));
        }
    }

    #[test]
    fn top_free_empty_when_none_free() {
        let json = r#"{"data":[{"id":"x/paid","context_length":9,"pricing":{"prompt":"0.01","completion":"0"}}]}"#;
        assert!(select_top_free_models(json, 5).unwrap().is_empty());
    }

    #[test]
    fn code_parsed_from_request_line() {
        let line = "GET /callback?code=abc123&scope=x HTTP/1.1";
        assert_eq!(
            parse_code_from_request_line(line).as_deref(),
            Some("abc123")
        );
    }

    #[test]
    fn code_percent_decoded() {
        // code=abc%2Bxyz → abc+xyz(服务端 code 含 '+' 被浏览器 percent-encode)。
        assert_eq!(
            parse_code_from_request_line("GET /callback?code=abc%2Bxyz HTTP/1.1").as_deref(),
            Some("abc+xyz")
        );
        // 非法/不完整 %XX 原样保留,不 panic。
        assert_eq!(
            parse_code_from_request_line("GET /callback?code=ab%2 HTTP/1.1").as_deref(),
            Some("ab%2")
        );
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

    /// 回归:模拟真实浏览器——TCP 握手完成(accept 返回)后隔一段延迟才发送
    /// HTTP 请求。若 accept 出的 stream 继承了 listener 的非阻塞标志(macOS/BSD),
    /// read 会在请求到达前 WouldBlock 而丢弃连接,本测试会失败/超时。
    /// 直连写立即到达的 `..._over_loopback` 测试掩盖不了这个平台差异。
    #[test]
    fn delayed_request_after_handshake_still_returns_code() {
        use std::io::Write;
        use std::sync::atomic::AtomicBool;
        let cb = start_local_callback().unwrap();
        let port = cb.port();
        let h = std::thread::spawn(move || {
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            // 握手后延迟再发请求,复现浏览器"连接已建立、请求稍后到"的时序。
            std::thread::sleep(std::time::Duration::from_millis(250));
            s.write_all(b"GET /callback?code=cafef00d HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
        });
        let cancel = AtomicBool::new(false);
        let code = cb
            .wait_for_code(std::time::Duration::from_secs(4), &cancel)
            .unwrap();
        h.join().unwrap();
        assert_eq!(code.as_deref(), Some("cafef00d"));
    }

    /// F2 回归:无 code 的请求(如浏览器预检 favicon)不应终止等待;
    /// 第二个连接带真正 code 时才返回。
    #[test]
    fn no_code_request_is_skipped_real_code_returned() {
        use std::io::Write;
        use std::sync::atomic::AtomicBool;

        let cb = start_local_callback().unwrap();
        let port = cb.port();

        // 后台线程:先发无 code 请求,短暂等待后发带 code 请求。
        let h = std::thread::spawn(move || {
            // 第一个连接:无 code(模拟 favicon 预检)。
            let mut s1 = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s1.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            drop(s1);

            // 稍等,让 wait_for_code 处理完第一个连接并 continue。
            std::thread::sleep(std::time::Duration::from_millis(100));

            // 第二个连接:带真正 code。
            let mut s2 = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s2.write_all(b"GET /callback?code=realcode42 HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
        });

        let cancel = AtomicBool::new(false);
        let code = cb
            .wait_for_code(std::time::Duration::from_secs(5), &cancel)
            .unwrap();
        h.join().unwrap();
        // 必须返回第二个连接的 code,不能因无 code 的第一个连接而提前返回 None。
        assert_eq!(code.as_deref(), Some("realcode42"));
    }
}
