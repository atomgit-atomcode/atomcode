use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

const MODE_ENV: &str = "ATOMCODE_PROXY_MODE";

const ENV_HTTP_PROXY: &[&str] = &["HTTP_PROXY", "http_proxy"];
const ENV_HTTPS_PROXY: &[&str] = &["HTTPS_PROXY", "https_proxy"];
const ENV_ALL_PROXY: &[&str] = &["ALL_PROXY", "all_proxy"];
const ENV_NO_PROXY: &[&str] = &["NO_PROXY", "no_proxy"];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    FollowSystem,
    DefaultProxy,
    NoProxy,
}

impl Default for ProxyMode {
    fn default() -> Self {
        Self::NoProxy
    }
}

impl ProxyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FollowSystem => "follow_system",
            Self::DefaultProxy => "default_proxy",
            Self::NoProxy => "no_proxy",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::FollowSystem => "follow_system",
            Self::DefaultProxy => "default_proxy",
            Self::NoProxy => "no_proxy",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyConfig {
    #[serde(default)]
    pub mode: ProxyMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub https: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_proxy: Option<String>,
}

impl ProxyConfig {
    pub fn capture_from_env() -> Self {
        let snapshot = startup_env();
        Self {
            mode: ProxyMode::DefaultProxy,
            http: snapshot.http.clone(),
            https: snapshot.https.clone(),
            all: snapshot.all.clone(),
            no_proxy: snapshot.no_proxy.clone(),
        }
    }

    pub fn summary(&self) -> String {
        match self.mode {
            ProxyMode::FollowSystem => "follow_system".to_string(),
            ProxyMode::NoProxy => "no_proxy".to_string(),
            ProxyMode::DefaultProxy => {
                let count = [
                    self.http.as_ref(),
                    self.https.as_ref(),
                    self.all.as_ref(),
                    self.no_proxy.as_ref(),
                ]
                .into_iter()
                .flatten()
                .count();
                if count == 0 {
                    "default_proxy (no pinned env captured)".to_string()
                } else {
                    format!("default_proxy ({} pinned vars)", count)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProxyFileConfig {
    #[serde(default)]
    network: ProxyNetworkSection,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProxyNetworkSection {
    #[serde(default)]
    proxy: ProxyConfig,
}

#[derive(Debug, Clone, Default)]
struct ProxyEnvSnapshot {
    http: Option<String>,
    https: Option<String>,
    all: Option<String>,
    no_proxy: Option<String>,
}

static STARTUP_ENV: OnceLock<ProxyEnvSnapshot> = OnceLock::new();

fn startup_env() -> &'static ProxyEnvSnapshot {
    STARTUP_ENV.get_or_init(|| ProxyEnvSnapshot {
        http: first_env_value(ENV_HTTP_PROXY),
        https: first_env_value(ENV_HTTPS_PROXY),
        all: first_env_value(ENV_ALL_PROXY),
        no_proxy: first_env_value(ENV_NO_PROXY),
    })
}

fn first_env_value(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| env::var(key).ok().filter(|value| !value.trim().is_empty()))
}

fn set_env_keys(keys: &[&str], value: &Option<String>) {
    match value {
        Some(v) if !v.trim().is_empty() => {
            for key in keys {
                env::set_var(key, v);
            }
        }
        _ => {
            for key in keys {
                env::remove_var(key);
            }
        }
    }
}

fn config_path() -> PathBuf {
    if let Some(home) = env::var("ATOMCODE_HOME").ok().filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join("config.toml");
    }
    let home = crate::tool::real_home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".atomcode").join("config.toml")
}

fn load_proxy_config_from_disk() -> ProxyConfig {
    let path = config_path();
    let Ok(content) = std::fs::read_to_string(path) else {
        return ProxyConfig::default();
    };
    toml::from_str::<ProxyFileConfig>(&content)
        .map(|cfg| cfg.network.proxy)
        .unwrap_or_default()
}

pub fn install_from_default_path_or_default() {
    let cfg = load_proxy_config_from_disk();
    apply_process_proxy_config(&cfg);
}

fn ensure_runtime_initialized() {
    if env::var_os(MODE_ENV).is_none() {
        install_from_default_path_or_default();
    }
}

pub fn apply_process_proxy_config(cfg: &ProxyConfig) {
    let _ = startup_env();
    env::set_var(MODE_ENV, cfg.mode.as_str());
    match cfg.mode {
        ProxyMode::FollowSystem => {
            let snapshot = startup_env();
            set_env_keys(ENV_HTTP_PROXY, &snapshot.http);
            set_env_keys(ENV_HTTPS_PROXY, &snapshot.https);
            set_env_keys(ENV_ALL_PROXY, &snapshot.all);
            set_env_keys(ENV_NO_PROXY, &snapshot.no_proxy);
        }
        ProxyMode::DefaultProxy => {
            set_env_keys(ENV_HTTP_PROXY, &cfg.http);
            set_env_keys(ENV_HTTPS_PROXY, &cfg.https);
            set_env_keys(ENV_ALL_PROXY, &cfg.all);
            set_env_keys(ENV_NO_PROXY, &cfg.no_proxy);
        }
        ProxyMode::NoProxy => {
            set_env_keys(ENV_HTTP_PROXY, &None);
            set_env_keys(ENV_HTTPS_PROXY, &None);
            set_env_keys(ENV_ALL_PROXY, &None);
            set_env_keys(ENV_NO_PROXY, &None);
        }
    }
}

pub fn apply_async_proxy_policy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    ensure_runtime_initialized();
    if env::var(MODE_ENV).ok().as_deref() == Some(ProxyMode::NoProxy.as_str()) {
        builder.no_proxy()
    } else {
        builder
    }
}

pub fn apply_blocking_proxy_policy(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    ensure_runtime_initialized();
    if env::var(MODE_ENV).ok().as_deref() == Some(ProxyMode::NoProxy.as_str()) {
        builder.no_proxy()
    } else {
        builder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_mode_default_is_no_proxy() {
        assert_eq!(ProxyMode::default(), ProxyMode::NoProxy);
    }

    #[test]
    fn proxy_summary_reflects_mode() {
        let cfg = ProxyConfig::default();
        assert_eq!(cfg.summary(), "no_proxy");

        let follow = ProxyConfig {
            mode: ProxyMode::FollowSystem,
            ..Default::default()
        };
        assert_eq!(follow.summary(), "follow_system");
    }

    #[test]
    fn proxy_file_config_defaults_to_no_proxy() {
        let parsed: ProxyFileConfig = toml::from_str("").expect("parse");
        assert_eq!(parsed.network.proxy.mode, ProxyMode::NoProxy);
    }
}
