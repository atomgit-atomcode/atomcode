//! Telemetry configuration and 4-level opt-out resolution.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const DEFAULT_ENDPOINT: &str = "https://acs.atomgit.com/api/v1/events";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CliOverride {
    pub disabled: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub state: TelemetryState,
    pub endpoint: String,
    pub atomcode_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub enum TelemetryState {
    Enabled,
    Disabled(&'static str),
}

impl TelemetryState {
    pub fn is_enabled(&self) -> bool {
        matches!(self, TelemetryState::Enabled)
    }
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            TelemetryState::Disabled(r) => Some(r),
            _ => None,
        }
    }
}

/// Resolve the 5-level telemetry opt-out: offline → env×2 → cli → config.
///
/// `offline` is resolved ONCE at startup. Under `offline_mode="auto"` the verdict is
/// optimistic-online at telemetry-init time; a later network-failure flip does NOT
/// re-resolve telemetry, so `auto` does NOT disable telemetry — only a forced `on` /
/// `ATOMCODE_OFFLINE=on` does.
pub fn resolve(
    cfg: &TelemetryConfig,
    cli: &CliOverride,
    atomcode_dir: PathBuf,
    env: &impl EnvLookup,
    offline: bool,
) -> ResolvedConfig {
    let state = if offline {
        TelemetryState::Disabled("offline")
    } else if env.var("ATOMCODE_TELEMETRY").as_deref() == Some("0") {
        TelemetryState::Disabled("env:ATOMCODE_TELEMETRY=0")
    } else if env.var("DO_NOT_TRACK").as_deref() == Some("1") {
        TelemetryState::Disabled("env:DO_NOT_TRACK=1")
    } else if cli.disabled {
        TelemetryState::Disabled("cli:--no-telemetry")
    } else if matches!(cfg.enabled, Some(false)) {
        TelemetryState::Disabled("config")
    } else {
        TelemetryState::Enabled
    };

    let endpoint = env
        .var("ATOMCODE_TELEMETRY_ENDPOINT")
        .or_else(|| cfg.endpoint.clone())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());

    ResolvedConfig {
        state,
        endpoint,
        atomcode_dir,
    }
}

pub trait EnvLookup {
    fn var(&self, key: &str) -> Option<String>;
}

pub struct ProcessEnv;
impl EnvLookup for ProcessEnv {
    fn var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MapEnv(HashMap<&'static str, &'static str>);
    impl EnvLookup for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).map(|s| s.to_string())
        }
    }
    fn env(kv: &[(&'static str, &'static str)]) -> MapEnv {
        MapEnv(kv.iter().copied().collect())
    }
    fn dir() -> PathBuf {
        PathBuf::from("/tmp/.atomcode-test")
    }

    #[test]
    fn default_is_enabled() {
        let r = resolve(
            &TelemetryConfig::default(),
            &CliOverride::default(),
            dir(),
            &env(&[]),
            false,
        );
        assert!(r.state.is_enabled());
        assert_eq!(r.endpoint, DEFAULT_ENDPOINT);
    }

    #[test]
    fn env_wins_over_config() {
        let cfg = TelemetryConfig {
            enabled: Some(true),
            endpoint: None,
        };
        let r = resolve(
            &cfg,
            &CliOverride::default(),
            dir(),
            &env(&[("ATOMCODE_TELEMETRY", "0")]),
            false,
        );
        assert_eq!(r.state.reason(), Some("env:ATOMCODE_TELEMETRY=0"));
    }

    #[test]
    fn do_not_track_wins_over_cli() {
        let r = resolve(
            &TelemetryConfig::default(),
            &CliOverride { disabled: true },
            dir(),
            &env(&[("DO_NOT_TRACK", "1")]),
            false,
        );
        assert_eq!(r.state.reason(), Some("env:DO_NOT_TRACK=1"));
    }

    #[test]
    fn cli_wins_over_config() {
        let cfg = TelemetryConfig {
            enabled: Some(true),
            endpoint: None,
        };
        let r = resolve(
            &cfg,
            &CliOverride { disabled: true },
            dir(),
            &env(&[]),
            false,
        );
        assert_eq!(r.state.reason(), Some("cli:--no-telemetry"));
    }

    #[test]
    fn config_false_disables() {
        let cfg = TelemetryConfig {
            enabled: Some(false),
            endpoint: None,
        };
        let r = resolve(&cfg, &CliOverride::default(), dir(), &env(&[]), false);
        assert_eq!(r.state.reason(), Some("config"));
    }

    #[test]
    fn endpoint_env_override() {
        let r = resolve(
            &TelemetryConfig::default(),
            &CliOverride::default(),
            dir(),
            &env(&[("ATOMCODE_TELEMETRY_ENDPOINT", "https://test.example/v1")]),
            false,
        );
        assert_eq!(r.endpoint, "https://test.example/v1");
    }

    #[test]
    fn offline_disables_telemetry() {
        let r = resolve(
            &TelemetryConfig {
                enabled: Some(true),
                endpoint: None,
            },
            &CliOverride::default(),
            dir(),
            &env(&[]),
            /* offline: */ true,
        );
        assert_eq!(r.state.reason(), Some("offline"));
    }
}
