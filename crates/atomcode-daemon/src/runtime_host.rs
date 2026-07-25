use std::sync::Arc;

use async_trait::async_trait;
use atomcode_coding::cc_hooks::HookConfig;
use atomcode_coding::{PluginHookSource, RateLimitWindow, RateLimitWindowSource};

#[derive(Debug, Default)]
pub struct InstalledPluginHookSource;

impl PluginHookSource for InstalledPluginHookSource {
    fn load(&self) -> Result<Vec<HookConfig>, String> {
        atomcode_capabilities::plugin::hook_trust::ensure_migrated();
        Ok(
            atomcode_capabilities::plugin::loader::installed_plugin_cc_hooks()
                .into_iter()
                .filter_map(|hook| {
                    HookConfig::from_plugin_spec(
                        &hook.event,
                        hook.matcher,
                        hook.command,
                        hook.timeout_secs,
                        hook.plugin_root,
                    )
                })
                .collect(),
        )
    }
}

pub fn installed_plugin_hook_source() -> Arc<dyn PluginHookSource> {
    Arc::new(InstalledPluginHookSource)
}

pub fn gather_plugin_skill_dirs() -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    for assets in atomcode_capabilities::plugin::loader::iter_installed_plugin_assets() {
        for sd in assets.skills_dirs() {
            if sd.exists() {
                out.push((sd, assets.plugin.clone()));
            }
        }
    }
    out
}

#[derive(Debug, Default)]
pub struct CodingPlanRateLimitSource;

#[async_trait]
impl RateLimitWindowSource for CodingPlanRateLimitSource {
    fn applies_to(&self, base_url: &str) -> bool {
        atomcode_capabilities::provider::is_atomgit_gateway(base_url)
    }

    async fn fetch_windows(&self) -> Result<Vec<RateLimitWindow>, String> {
        tokio::task::spawn_blocking(|| {
            let client = atomcode_codingplan::Client::from_stored_auth()
                .map_err(|error| error.to_string())?;
            let status = client.status_v2().map_err(|error| error.to_string())?;
            Ok(status
                .rate_limit_windows
                .into_iter()
                .map(|window| RateLimitWindow {
                    window_size_seconds: window.window_size_seconds,
                    quota_exhausted: window.quota_exhausted,
                    reset_at_display: window.reset_at_display,
                    seconds_until_reset: window.seconds_until_reset,
                    reset_label: window.reset_label,
                })
                .collect())
        })
        .await
        .map_err(|error| error.to_string())?
    }
}

pub fn coding_plan_rate_limit_source() -> Arc<dyn RateLimitWindowSource> {
    Arc::new(CodingPlanRateLimitSource)
}

pub fn coding_provider_factory() -> Arc<dyn atomcode_coding::CodingProviderFactory> {
    atomcode_coding::atomgit_provider_factory(atomcode_auth::ATOMCODE_USER_AGENT)
}
