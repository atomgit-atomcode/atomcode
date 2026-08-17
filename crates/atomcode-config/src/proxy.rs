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

fn config_path() -> PathBuf {
    if let Some(home) = env::var("ATOMCODE_HOME").ok().filter(|s| !s.is_empty()) {
        return PathBuf::from(home).join("config.toml");
    }
    let home = crate::util::real_home_dir().unwrap_or_else(|| PathBuf::from("."));
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

/// Hosts that must never be reached through a proxy, whatever the user or the OS asked for.
///
/// A proxy resolves `127.0.0.1` against ITSELF, so proxying loopback either fails outright or —
/// worse — silently reaches a different machine's service on that port. Every other HTTP stack
/// special-cases this (curl, Go's `httpproxy`, Docker); reqwest does not, and macOS's own
/// exception list does not include loopback unless the user adds it. Without this, anyone with
/// a system proxy switched on cannot reach a local model server (Ollama, LM Studio, vLLM) at
/// all — the request goes to the proxy and comes back 403.
const LOOPBACK_BYPASS: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// Add [`LOOPBACK_BYPASS`] to whatever bypass list the user or the OS supplied, keeping their
/// entries and not repeating one they already listed.
fn with_loopback_bypass(no_proxy: &Option<String>) -> Option<String> {
    let mut entries: Vec<String> = no_proxy
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    for host in LOOPBACK_BYPASS {
        if !entries.iter().any(|e| e.eq_ignore_ascii_case(host)) {
            entries.push((*host).to_owned());
        }
    }
    Some(entries.join(","))
}

/// The bypass list to install for a resolved snapshot: the loopback hosts folded in when some
/// proxy is actually in play, and the caller's list untouched when none is — with no proxy
/// there is nothing to bypass, and conjuring a `NO_PROXY` out of nothing would only confuse
/// anyone reading the process environment.
fn bypass_for(resolved: &ProxyEnvSnapshot) -> Option<String> {
    let any_proxy = resolved.http.is_some() || resolved.https.is_some() || resolved.all.is_some();
    if any_proxy {
        with_loopback_bypass(&resolved.no_proxy)
    } else {
        resolved.no_proxy.clone()
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
            set_env_keys(ENV_NO_PROXY, &bypass_for(&resolved));
        }
        ProxyMode::DefaultProxy => {
            let pinned = ProxyEnvSnapshot {
                http: cfg.http.clone(),
                https: cfg.https.clone(),
                all: cfg.all.clone(),
                no_proxy: cfg.no_proxy.clone(),
            };
            set_env_keys(ENV_HTTP_PROXY, &cfg.http);
            set_env_keys(ENV_HTTPS_PROXY, &cfg.https);
            set_env_keys(ENV_ALL_PROXY, &cfg.all);
            set_env_keys(ENV_NO_PROXY, &bypass_for(&pinned));
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

    fn snapshot_with_proxy(no_proxy: Option<&str>) -> ProxyEnvSnapshot {
        ProxyEnvSnapshot {
            http: Some("http://corp:8080".into()),
            https: Some("http://corp:8080".into()),
            all: None,
            no_proxy: no_proxy.map(str::to_owned),
        }
    }

    fn entries(value: &Option<String>) -> Vec<String> {
        value
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn loopback_is_bypassed_whenever_a_proxy_is_in_play() {
        // The whole point: a proxy resolves 127.0.0.1 against itself, so a local model server
        // is unreachable unless loopback is exempted. macOS's exception list does not include
        // it and reqwest does not special-case it, so nobody else will do this for us.
        let out = entries(&bypass_for(&snapshot_with_proxy(None)));
        for host in LOOPBACK_BYPASS {
            assert!(out.iter().any(|e| e == host), "{host} must be bypassed");
        }
    }

    #[test]
    fn loopback_bypass_keeps_what_the_user_already_listed() {
        let out = entries(&bypass_for(&snapshot_with_proxy(Some(
            "*.corp, internal.example",
        ))));
        assert!(out.iter().any(|e| e == "*.corp"));
        assert!(out.iter().any(|e| e == "internal.example"));
        assert!(out.iter().any(|e| e == "127.0.0.1"));
    }

    #[test]
    fn loopback_bypass_does_not_repeat_an_entry_the_user_supplied() {
        let out = entries(&bypass_for(&snapshot_with_proxy(Some("localhost,*.corp"))));
        assert_eq!(
            out.iter().filter(|e| e.as_str() == "localhost").count(),
            1,
            "got {out:?}"
        );
    }

    #[test]
    fn no_bypass_is_invented_when_there_is_no_proxy_to_bypass() {
        // With nothing proxied, a NO_PROXY appearing from nowhere would just mislead anyone
        // reading the process environment.
        let none = ProxyEnvSnapshot::default();
        assert_eq!(bypass_for(&none), None);
    }
}
