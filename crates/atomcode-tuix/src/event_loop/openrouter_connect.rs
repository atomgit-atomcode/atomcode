//! OpenRouter 一键接入:后台连接任务 + 事件 + Config 装配(幂等纯函数)。

use atomcode_auth::openrouter::FreeModel;
use atomcode_config::config::provider::{
    default_context_window_for, ModelProfileConfig, ProviderAccountConfig,
};
use atomcode_config::config::provider_preset::preset_or_compatible;
use atomcode_config::config::Config;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::mpsc;

const OPENROUTER_ACCOUNT_ID: &str = "openrouter";

/// 接入模式:空 arg → OAuth PKCE 浏览器流,非空 → 直接使用已有 key。
/// 由 `/openrouter` 命令处理器构造并传给 `spawn_openrouter_connect`。
pub enum ConnectMode {
    Oauth,
    ProvidedKey(String),
}

/// 解析 `/openrouter [arg]` 的参数:trim 后为空则走 OAuth,否则为 ProvidedKey。
/// 由 `/openrouter` 命令处理器调用。
pub fn parse_connect_mode(arg: &str) -> ConnectMode {
    let t = arg.trim();
    if t.is_empty() {
        ConnectMode::Oauth
    } else {
        ConnectMode::ProvidedKey(t.to_string())
    }
}

/// 后台连接任务发给主循环 select! 臂的事件。
pub enum OpenRouterConnectEvent {
    Ready {
        api_key: String,
        models: Vec<FreeModel>,
    },
    Failed(String),
}

#[allow(dead_code)]
pub struct ProvisionOutcome {
    pub account_id: String,
    pub added: Vec<String>,
    pub default_model: String,
}

#[allow(dead_code)]
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
        let result: Result<(String, Vec<or::FreeModel>), String> = (|| {
            let key = match mode {
                ConnectMode::ProvidedKey(k) => k,
                ConnectMode::Oauth => {
                    let pkce = or::generate_pkce();
                    let cb = or::start_local_callback().map_err(|e| format!("{e:#}"))?;
                    let callback_url = format!("http://localhost:{}/callback", cb.port());
                    let auth_url = or::build_auth_url(Some(&callback_url), &pkce.challenge);
                    let _ = atomcode_auth::oauth::open_browser(&auth_url);
                    // 等最长 3 分钟;cancel 由 ESC 置位。
                    let code = cb
                        .wait_for_code(std::time::Duration::from_secs(180), &cancel)
                        .map_err(|e| format!("{e:#}"))?
                        .ok_or_else(|| "已取消或超时".to_string())?;
                    or::exchange_code_for_key(&code, &pkce.verifier)
                        .map_err(|e| format!("{e:#}"))?
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

/// 幂等装配:account 固定 id,存在则更新 key;模型 selection `openrouter/<model>`,
/// 存在则跳过。全部持久(ephemeral=false)。
pub fn provision_openrouter(
    config: &mut Config,
    api_key: &str,
    models: &[FreeModel],
) -> ProvisionOutcome {
    let preset = preset_or_compatible(OPENROUTER_ACCOUNT_ID);
    let provider_type_wire = preset.provider_type.wire().to_string();

    // upsert 账号(仅更新 key,保留其它字段)。
    config
        .provider_accounts
        .entry(OPENROUTER_ACCOUNT_ID.to_string())
        .and_modify(|a| a.api_key = Some(api_key.to_string()))
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

/// CodingPlan 当前窗口是否耗尽。usage_percent 以百分比计(0..=100+)。
pub fn quota_exhausted(usage: &atomcode_codingplan::types::UsageInfo) -> bool {
    usage.usage_percent >= 100.0
}

/// 用户是否已有 CodingPlan 权益(据 config 里的账号判定)。
///
/// 检测两类配置写法:
/// - 旧 schema `[providers.AtomGit*]`:key 名匹配 CodingPlan 前缀规则。
/// - 新 schema `[provider_accounts.*]`:base_url 指向 CodingPlan LLM 网关。
pub fn has_codingplan(config: &Config) -> bool {
    use atomcode_config::config::is_codingplan_provider_name;
    use atomcode_config::endpoints::is_codingplan_llm_gateway;

    // 旧 schema:provider 名直接是 AtomGit / AtomGit-* 等。
    if config
        .providers
        .keys()
        .any(|k| is_codingplan_provider_name(k))
    {
        return true;
    }
    // 新 schema:账号的 base_url 指向 CodingPlan 网关。
    config.provider_accounts.values().any(|a| {
        a.base_url
            .as_deref()
            .is_some_and(is_codingplan_llm_gateway)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_auth::openrouter::FreeModel;
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

    fn models() -> Vec<FreeModel> {
        vec![
            FreeModel {
                id: "vendor/big:free".into(),
                name: Some("Big".into()),
                context_length: 128000,
            },
            FreeModel {
                id: "vendor/small:free".into(),
                name: None,
                context_length: 8000,
            },
        ]
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
        assert!(matches!(parse_connect_mode(""), ConnectMode::Oauth));
        assert!(matches!(parse_connect_mode("   "), ConnectMode::Oauth));
        match parse_connect_mode("  sk-or-v1-abc  ") {
            ConnectMode::ProvidedKey(k) => assert_eq!(k, "sk-or-v1-abc"),
            _ => panic!("expected ProvidedKey"),
        }
    }

    #[test]
    fn fresh_config_gets_account_models_and_default() {
        let mut c = Config::default();
        let out = provision_openrouter(&mut c, "sk-or-v1-x", &models());
        assert_eq!(out.account_id, "openrouter");
        assert!(c.provider_accounts.contains_key("openrouter"));
        assert_eq!(
            c.provider_accounts["openrouter"].api_key.as_deref(),
            Some("sk-or-v1-x")
        );
        assert!(!c.provider_accounts["openrouter"].ephemeral);
        assert!(c.models.contains_key("openrouter/vendor/big:free"));
        assert!(c.models.contains_key("openrouter/vendor/small:free"));
        assert_eq!(out.default_model, "openrouter/vendor/big:free");
        assert_eq!(
            c.default_model.as_deref(),
            Some("openrouter/vendor/big:free")
        );
    }

    #[test]
    fn existing_account_key_updated_not_duplicated() {
        let mut c = Config::default();
        provision_openrouter(&mut c, "sk-or-v1-OLD", &models());
        let out = provision_openrouter(&mut c, "sk-or-v1-NEW", &models());
        // 仍只有一个 openrouter 账号,key 被更新,模型不翻倍。
        assert_eq!(
            c.provider_accounts["openrouter"].api_key.as_deref(),
            Some("sk-or-v1-NEW")
        );
        assert_eq!(
            c.models.keys().filter(|k| k.starts_with("openrouter/")).count(),
            2
        );
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
