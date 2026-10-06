//! OpenRouter 一键接入:后台连接任务 + 事件。写进配置走
//! `atomcode_auth::openrouter::provision`,与新界面同一份。

use atomcode_auth::openrouter::FreeModel;
use atomcode_config::config::Config;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

/// 接入模式。由 `/openrouter` 命令处理器构造并传给 `spawn_openrouter_connect`。
pub enum ConnectMode {
    /// OAuth PKCE 浏览器流。
    Oauth,
    /// 人给的 key。
    ProvidedKey(String),
    /// 上次授权保存下来的 key:OpenRouter 还认就用,不认才走浏览器。
    Saved(String),
}

/// 解析 `/openrouter [arg]` 的参数:空 → 有保存的 key 就用它(`Saved`),没有走 OAuth;
/// `login` → 不管存没存过都走 OAuth(换账号);其余 → ProvidedKey。
/// 由 `/openrouter` 命令处理器调用,`saved` 是配置里 OpenRouter 账号现有的 key。
pub fn parse_connect_mode(arg: &str, saved: Option<String>) -> ConnectMode {
    match arg.trim() {
        "" => saved.map_or(ConnectMode::Oauth, ConnectMode::Saved),
        word if word.eq_ignore_ascii_case("login") => ConnectMode::Oauth,
        given => ConnectMode::ProvidedKey(given.to_string()),
    }
}

/// 后台连接任务发给主循环 select! 臂的事件。
pub enum OpenRouterConnectEvent {
    /// OAuth 授权 URL 已打开浏览器,回传供用户在浏览器未自动打开时手动访问。
    AwaitingBrowser {
        auth_url: String,
    },
    /// 过程中值得说一句的事(用了保存的 key、保存的 key 已失效)。
    Note(String),
    Ready {
        api_key: String,
        models: Vec<FreeModel>,
        /// Every model id OpenRouter lists, for `provision_with_listed` to take
        /// out the ones it no longer offers.
        listed: std::collections::HashSet<String>,
    },
    Failed(String),
}

const FREE_MODEL_LIMIT: usize = 5;

/// 后台线程:取 key(OAuth 或直传)+ 发现 top5 免费模型 → 发事件 + 唤醒循环。
/// 网络操作全在此线程,装配+存盘+reload 在主循环 select! 臂。
/// 由 `/openrouter` 命令处理器调用。
pub fn spawn_openrouter_connect(
    mode: ConnectMode,
    event_tx: mpsc::UnboundedSender<OpenRouterConnectEvent>,
    wake_tx: mpsc::Sender<()>,
    cancel: Arc<AtomicBool>,
) {
    use atomcode_auth::openrouter as or;
    std::thread::spawn(move || {
        let result: Result<(String, or::FreeCatalog), String> = (|| {
            let oauth = || -> Result<String, String> {
                let pkce = or::generate_pkce();
                let cb = or::start_local_callback().map_err(|e| format!("{e:#}"))?;
                let callback_url = format!("http://127.0.0.1:{}/callback", cb.port());
                let auth_url = or::build_auth_url(Some(&callback_url), &pkce.challenge);
                let _ = atomcode_auth::oauth::open_browser(&auth_url);
                // 把授权 URL 回传主循环显给用户:浏览器没自动打开(headless /
                // 无 DISPLAY / SSH)时用户仍能手动复制访问,而不是干等超时。
                let _ = event_tx.send(OpenRouterConnectEvent::AwaitingBrowser { auth_url });
                let _ = wake_tx.blocking_send(());
                // 等最长 3 分钟;cancel 由 ESC 置位。
                let code = cb
                    .wait_for_code(std::time::Duration::from_secs(180), &cancel)
                    .map_err(|e| format!("{e:#}"))?
                    .ok_or_else(|| "已取消或超时".to_string())?;
                or::exchange_code_for_key(&code, &pkce.verifier).map_err(|e| format!("{e:#}"))
            };
            let note = |text: &str| {
                let _ = event_tx.send(OpenRouterConnectEvent::Note(text.to_string()));
                let _ = wake_tx.blocking_send(());
            };
            let key = match mode {
                ConnectMode::ProvidedKey(k) => k,
                ConnectMode::Oauth => oauth()?,
                // 授权过一次就不再每次去浏览器:OpenRouter 还认就用;明确不认
                // (被吊销、过期)才重新授权。查不了(断网、代理)不算不认。
                ConnectMode::Saved(k) => {
                    match or::check_key(&k) {
                        or::KeyCheck::Rejected => {
                            note("上次保存的 key 已经失效,重新授权一次。");
                            oauth()?
                        }
                        or::KeyCheck::Valid => {
                            note("用的是上次授权保存的 key,不用再去浏览器(要换账号:/openrouter login)。");
                            k
                        }
                        or::KeyCheck::Unknown(_) => {
                            note("没能确认上次保存的 key 还有没有效(网络或代理?),先照用;后面失败的话用 /openrouter login 重新授权。");
                            k
                        }
                    }
                }
            };
            // ESC 之后这里不能再往下:取消了还发 Ready,主循环照样装配——再敲一次
            // /openrouter 就是两个线程各装配一遍。原本只有浏览器那一步看 cancel。
            let cancelled = || cancel.load(std::sync::atomic::Ordering::Relaxed);
            if cancelled() {
                return Err("已取消".to_string());
            }
            let catalog =
                or::fetch_free_catalog(&key, FREE_MODEL_LIMIT).map_err(|e| format!("{e:#}"))?;
            if cancelled() {
                return Err("已取消".to_string());
            }
            if catalog.free.is_empty() {
                return Err("OpenRouter 未返回可用免费模型".to_string());
            }
            Ok((key, catalog))
        })();

        let event = match result {
            Ok((api_key, catalog)) => OpenRouterConnectEvent::Ready {
                api_key,
                models: catalog.free,
                listed: catalog.listed,
            },
            Err(reason) => OpenRouterConnectEvent::Failed(reason),
        };
        let _ = event_tx.send(event);
        let _ = wake_tx.blocking_send(());
    });
}

/// CodingPlan 当前窗口是否耗尽。usage_percent 以百分比计(0..=100+)。
pub fn quota_exhausted(usage: &atomcode_codingplan::types::UsageInfo) -> bool {
    usage.usage_percent >= 100.0
}

/// 用户是否已有 CodingPlan 权益(据 config 里的账号判定)。
///
/// 检测两类配置写法:
/// - 旧 schema `[providers.AtomGit*]`:key 名匹配 CodingPlan 前缀规则。
/// - 新 schema `[provider_accounts.*]`:base_url 指向 CodingPlan LLM 网关,
///   或 provider 字段指向 atomgit preset(preset 默认 base_url 即网关)。
pub fn has_codingplan(config: &Config) -> bool {
    use atomcode_config::config::is_codingplan_provider_name;
    use atomcode_config::config::provider_preset::preset_or_compatible;
    use atomcode_config::endpoints::is_codingplan_llm_gateway;

    // 旧 schema:provider 名是 AtomGit / AtomGit-* 等,或(自定义命名但)
    // base_url 指向 CodingPlan 网关(与新 schema / account_is_codingplan_managed
    // 判定口径一致,避免"名字非 AtomGit 但确是 CodingPlan"被漏判致误弹 nudge)。
    if config.providers.iter().any(|(k, p)| {
        is_codingplan_provider_name(k)
            || p.base_url.as_deref().is_some_and(is_codingplan_llm_gateway)
    }) {
        return true;
    }
    // 新 schema:账号的 base_url 指向 CodingPlan 网关。
    // 若账号未显式写 base_url,则按 preset 的默认值回落(与
    // Config::account_is_codingplan_managed 保持一致)。
    config.provider_accounts.values().any(|a| {
        let preset = preset_or_compatible(&a.provider);
        a.base_url
            .as_deref()
            .or(preset.default_base_url)
            .is_some_and(is_codingplan_llm_gateway)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_config::config::Config;

    #[test]
    fn has_codingplan_detects_atomgit_account() {
        // 空配置:无 CodingPlan 账号。
        let empty = Config::default();
        assert!(!has_codingplan(&empty));

        // Legacy 旧 schema:key "AtomGit" 匹配 is_codingplan_provider_name。
        let legacy: Config = toml::from_str(
            r#"
[providers.AtomGit]
type = "openai"
model = "chatglm"
base_url = "https://llm-api.atomgit.com/v1"
"#,
        )
        .expect("valid legacy config");
        assert!(has_codingplan(&legacy));

        // 新 schema provider_accounts:base_url 指向 CodingPlan 网关。
        let new_schema: Config = toml::from_str(
            r#"
[provider_accounts.AtomGit]
provider = "openai"
base_url = "https://llm-api.atomgit.com/v1"
"#,
        )
        .expect("valid new-schema config");
        assert!(has_codingplan(&new_schema));
    }

    /// 新 schema 账号 provider="atomgit" 但未写 base_url:应通过 preset 默认值
    /// `https://llm-api.atomgit.com/v1` 回落判定为有 CodingPlan。
    #[test]
    fn has_codingplan_atomgit_preset_no_base_url() {
        let cfg: Config = toml::from_str(
            r#"
[provider_accounts.my-atomgit]
provider = "atomgit"
"#,
        )
        .expect("valid config");
        // 此账号无显式 base_url,应依 preset 回落判定为 true。
        assert!(
            has_codingplan(&cfg),
            "atomgit preset 账号(无 base_url)应被识别为 CodingPlan"
        );
    }

    #[test]
    fn quota_predicate_fires_at_full_usage() {
        use atomcode_codingplan::types::UsageInfo;
        let mut u: UsageInfo = serde_json::from_str("{}").unwrap();
        u.usage_percent = 100.0;
        assert!(quota_exhausted(&u));
        u.usage_percent = 87.0;
        assert!(!quota_exhausted(&u));
    }

    #[test]
    fn arg_parsing_selects_mode() {
        assert!(matches!(parse_connect_mode("", None), ConnectMode::Oauth));
        assert!(matches!(
            parse_connect_mode("   ", None),
            ConnectMode::Oauth
        ));
        // 授权过:空 arg 用保存的 key;login 照样走浏览器。
        assert!(matches!(
            parse_connect_mode("", Some("sk-saved".into())),
            ConnectMode::Saved(k) if k == "sk-saved"
        ));
        assert!(matches!(
            parse_connect_mode("login", Some("sk-saved".into())),
            ConnectMode::Oauth
        ));
        match parse_connect_mode("  sk-or-v1-abc  ", Some("sk-saved".into())) {
            ConnectMode::ProvidedKey(k) => assert_eq!(k, "sk-or-v1-abc"),
            _ => panic!("expected ProvidedKey"),
        }
    }
}
