//! Proxy configuration types + process-env policy (`[network.proxy]`).
//!
//! This reqwest-free leaf owns config types and std/toml environment policy.
//! Crates that own HTTP clients apply the resulting process policy to their own
//! reqwest builders.

use std::env;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

pub const MODE_ENV: &str = "ATOMCODE_PROXY_MODE";

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
    // Respect the environment's proxy by default (matches curl / reqwest-native
    // behavior). A `NoProxy` default silently stripped `https_proxy` and forced
    // `.no_proxy()` on every client, breaking every corporate-proxy user out of
    // the box (they'd time out reaching acs.atomgit.com etc.). Users who want to
    // ignore a system proxy can pick `no_proxy` via `/proxy`.
    fn default() -> Self {
        Self::FollowSystem
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

/// Pure core of [`config_path`], split out so the resolution is testable without mutating
/// process env — the same shape as `distribution::default_home`.
fn config_path_from(home_env: Option<String>, real_home: Option<PathBuf>) -> PathBuf {
    if let Some(home) = home_env.filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join("config.toml");
    }
    real_home
        .unwrap_or_else(|| PathBuf::from("."))
        .join(crate::distribution::HOME_DIR_NAME)
        .join("config.toml")
}

/// Mirrors `Config::config_dir()` + `default_path()`, resolved locally so this leaf keeps its
/// "reqwest-free, config-type-free" shape. Both halves must come from `distribution` — spelling
/// the env var and the directory out here meant a rebuilt distribution read its proxy policy
/// out of the *other* build's config file, i.e. silently ignored its own `[network.proxy]`.
fn config_path() -> PathBuf {
    config_path_from(
        env::var(crate::distribution::HOME_ENV).ok(),
        crate::util::real_home_dir(),
    )
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

/// Ensure the process proxy env has been initialized from disk once. Public so the
/// HTTP-owning crates can call it before building clients.
pub fn ensure_runtime_initialized() {
    if env::var_os(MODE_ENV).is_none() {
        install_from_default_path_or_default();
    }
}

/// Resolve the effective `FollowSystem` env values: an explicit env-var proxy
/// always wins; the OS system proxy only fills a field the env left empty. A
/// user-set `ALL_PROXY` counts as an explicit proxy for every scheme (reqwest
/// prioritizes scheme-specific vars over `ALL_PROXY`, so the system proxy must
/// not fill `http`/`https` and override it).
fn follow_system_env(
    snapshot: &ProxyEnvSnapshot,
    sys: &crate::system_proxy::SystemProxy,
) -> ProxyEnvSnapshot {
    let env_covers_all = snapshot.all.is_some();
    let fill = |scheme_env: &Option<String>, sys_val: &Option<String>| -> Option<String> {
        scheme_env.clone().or_else(|| {
            if env_covers_all {
                None
            } else {
                sys_val.clone()
            }
        })
    };
    ProxyEnvSnapshot {
        http: fill(&snapshot.http, &sys.http),
        https: fill(&snapshot.https, &sys.https),
        all: snapshot.all.clone(),
        no_proxy: snapshot.no_proxy.clone().or_else(|| sys.no_proxy.clone()),
    }
}

pub fn apply_process_proxy_config(cfg: &ProxyConfig) {
    let _ = startup_env();
    env::set_var(MODE_ENV, cfg.mode.as_str());
    match cfg.mode {
        ProxyMode::FollowSystem => {
            let snapshot = startup_env();
            // Explicit env proxy wins; the OS system proxy fills any gap so a
            // browser-configured (system) proxy is honored out of the box.
            let resolved = follow_system_env(snapshot, &crate::system_proxy::resolve());
            set_env_keys(ENV_HTTP_PROXY, &resolved.http);
            set_env_keys(ENV_HTTPS_PROXY, &resolved.https);
            set_env_keys(ENV_ALL_PROXY, &resolved.all);
            set_env_keys(ENV_NO_PROXY, &resolved.no_proxy);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_config_is_read_from_this_distribution_not_a_hardcoded_one() {
        // This leaf resolves its own path instead of calling Config::default_path(), so it is
        // the easiest place for a distribution rename to be missed — and a miss is silent: the
        // build reads the OTHER build's [network.proxy] and quietly ignores its own.
        let path = config_path_from(None, Some(PathBuf::from("/Users/foo")));
        assert_eq!(
            path,
            PathBuf::from("/Users/foo")
                .join(crate::distribution::HOME_DIR_NAME)
                .join("config.toml")
        );
        assert!(path.starts_with("/Users/foo"));
    }

    #[test]
    fn proxy_config_path_prefers_the_home_env_verbatim() {
        let path = config_path_from(
            Some("/tmp/explicit".into()),
            Some(PathBuf::from("/Users/foo")),
        );
        assert_eq!(path, PathBuf::from("/tmp/explicit/config.toml"));
    }

    #[test]
    fn proxy_config_path_ignores_an_empty_home_env() {
        // An exported-but-empty var means "unset", not "root at the filesystem root".
        let path = config_path_from(Some(String::new()), Some(PathBuf::from("/Users/foo")));
        assert!(path.starts_with("/Users/foo"));
    }

    #[test]
    fn follow_system_env_prefers_env_then_system() {
        use crate::system_proxy::SystemProxy;
        // Env has HTTPS only; system supplies HTTP + bypass.
        let snap = ProxyEnvSnapshot {
            http: None,
            https: Some("http://env-https:9".into()),
            all: None,
            no_proxy: None,
        };
        let sys = SystemProxy {
            http: Some("http://sys-http:8".into()),
            https: Some("http://sys-https:8".into()),
            no_proxy: Some("*.corp".into()),
        };
        let out = follow_system_env(&snap, &sys);
        // env https wins; system http fills the gap; system no_proxy fills the gap.
        assert_eq!(out.https.as_deref(), Some("http://env-https:9"));
        assert_eq!(out.http.as_deref(), Some("http://sys-http:8"));
        assert_eq!(out.no_proxy.as_deref(), Some("*.corp"));
    }

    #[test]
    fn follow_system_env_all_empty_stays_empty() {
        use crate::system_proxy::SystemProxy;
        let out = follow_system_env(&ProxyEnvSnapshot::default(), &SystemProxy::default());
        assert!(out.http.is_none() && out.https.is_none() && out.no_proxy.is_none());
    }

    #[test]
    fn follow_system_env_explicit_all_proxy_beats_system() {
        use crate::system_proxy::SystemProxy;
        // User set only ALL_PROXY; the OS also has http/https proxies. ALL_PROXY is
        // authoritative for every scheme, so the system proxy must NOT fill
        // http/https (which reqwest would otherwise prioritize over ALL_PROXY).
        let snap = ProxyEnvSnapshot {
            http: None,
            https: None,
            all: Some("http://env-all:7".into()),
            no_proxy: None,
        };
        let sys = SystemProxy {
            http: Some("http://sys-http:8".into()),
            https: Some("http://sys-https:8".into()),
            no_proxy: None,
        };
        let out = follow_system_env(&snap, &sys);
        assert!(
            out.http.is_none(),
            "system must not fill http when ALL_PROXY set: {:?}",
            out.http
        );
        assert!(
            out.https.is_none(),
            "system must not fill https when ALL_PROXY set: {:?}",
            out.https
        );
        assert_eq!(out.all.as_deref(), Some("http://env-all:7"));
    }

    #[test]
    fn proxy_mode_default_is_follow_system() {
        // Default respects the environment proxy so corporate-proxy users work
        // out of the box (see the Default impl rationale).
        assert_eq!(ProxyMode::default(), ProxyMode::FollowSystem);
    }

    #[test]
    fn proxy_summary_reflects_mode() {
        let cfg = ProxyConfig::default();
        assert_eq!(cfg.summary(), "follow_system");

        let no_proxy = ProxyConfig {
            mode: ProxyMode::NoProxy,
            ..Default::default()
        };
        assert_eq!(no_proxy.summary(), "no_proxy");
    }

    #[test]
    fn proxy_file_config_defaults_to_follow_system() {
        let parsed: ProxyFileConfig = toml::from_str("").expect("parse");
        assert_eq!(parsed.network.proxy.mode, ProxyMode::FollowSystem);
    }
}
