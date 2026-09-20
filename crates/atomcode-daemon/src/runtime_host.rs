use std::sync::Arc;

use async_trait::async_trait;
use atomcode_coding::cc_hooks::HookConfig;
use atomcode_coding::{
    AccountUsage, DayUse, ModelUse, PluginHookSource, RateLimitWindow, RateLimitWindowSource,
};

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
    let working_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    gather_plugin_skill_dirs_for(&working_dir)
}

pub fn gather_plugin_skill_dirs_for(
    working_dir: &std::path::Path,
) -> Vec<(std::path::PathBuf, String)> {
    atomcode_capabilities::plugin::loader::installed_plugin_skill_dirs(working_dir)
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
                .map(window_from)
                .collect())
        })
        .await
        .map_err(|error| error.to_string())?
    }

    /// What the account has spent, from the same service the windows come from.
    ///
    /// Shaped into the neutral type here, where the service's own vocabulary
    /// still is: the per-model maps become a list biggest-first, and the daily
    /// rows keep their order. A screen drawing a chart should not have to know
    /// that the service answers in hash maps.
    async fn fetch_usage(&self) -> Result<Option<AccountUsage>, String> {
        tokio::task::spawn_blocking(move || -> Result<Option<AccountUsage>, String> {
            let client = atomcode_codingplan::client::Client::from_stored_auth()
                .map_err(|error| error.to_string())?;
            let usage = client.usage().map_err(|error| error.to_string())?;
            let mut models: Vec<ModelUse> = usage
                .model_tokens
                .iter()
                .map(|(name, tokens)| ModelUse {
                    name: name.clone(),
                    tokens: *tokens,
                    requests: usage.model_counts.get(name).copied().unwrap_or(0),
                })
                .collect();
            models.sort_by(|a, b| b.tokens.cmp(&a.tokens).then_with(|| a.name.cmp(&b.name)));
            Ok(Some(AccountUsage {
                from: usage.start_date,
                to: usage.end_date,
                models,
                daily: usage
                    .rows
                    .into_iter()
                    .map(|row| DayUse {
                        date: row.date,
                        tokens: row.total_tokens,
                        requests: row.total_counts,
                    })
                    .collect(),
                total_tokens: usage.total_tokens,
                total_requests: usage.total_counts,
            }))
        })
        .await
        .map_err(|error| error.to_string())?
    }
}

/// One account-service window, in the runtime's own words.
///
/// A named function rather than a closure in the fetch, so what it carries can
/// be judged without a network: this mapping quietly dropped `calls_used` and
/// `usage_percent` until 2026-09-20, and the cost was a screen that could say
/// when a window resets and never how much of it was gone. A field-by-field
/// map is exactly the shape that loses a field without anything going red.
fn window_from(window: atomcode_codingplan::types::RateLimitWindow) -> RateLimitWindow {
    RateLimitWindow {
        window_size_seconds: window.window_size_seconds,
        quota_exhausted: window.quota_exhausted,
        reset_at_display: window.reset_at_display,
        seconds_until_reset: window.seconds_until_reset,
        reset_label: window.reset_label,
        call_limit: window.call_limit,
        calls_used: window.calls_used,
        usage_percent: window.usage_percent,
    }
}

pub fn coding_plan_rate_limit_source() -> Arc<dyn RateLimitWindowSource> {
    Arc::new(CodingPlanRateLimitSource)
}

pub fn coding_provider_factory() -> Arc<dyn atomcode_coding::CodingProviderFactory> {
    atomcode_coding::atomgit_provider_factory(atomcode_auth::ATOMCODE_USER_AGENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything the account service says about a window reaches the runtime.
    ///
    /// Written because this mapping lost two fields and nothing noticed: the
    /// screen drew a reset time and no usage, and every test stayed green
    /// because no test looked at the mapping. Asserted field by field, which is
    /// the only shape that catches the next one going missing.
    #[test]
    fn a_window_crosses_with_everything_the_service_said_about_it() {
        let upstream = atomcode_codingplan::types::RateLimitWindow {
            window_size_seconds: 18_000,
            quota_exhausted: true,
            reset_at_display: "14:30".into(),
            seconds_until_reset: 3600,
            reset_label: "5 小时".into(),
            call_limit: 1000,
            calls_used: 420,
            usage_percent: 42.0,
            rule_index: 0,
            show_enable: 1,
            window_hours: 5,
            reset_at: "2026-09-20T14:30:00Z".into(),
            usage_status_desc: "42% used".into(),
        };
        let crossed = window_from(upstream);
        assert_eq!(crossed.window_size_seconds, 18_000);
        assert!(crossed.quota_exhausted);
        assert_eq!(crossed.reset_at_display, "14:30");
        assert_eq!(crossed.seconds_until_reset, 3600);
        assert_eq!(crossed.reset_label, "5 小时");
        assert_eq!(crossed.call_limit, 1000);
        // The two that were being dropped.
        assert_eq!(crossed.calls_used, 420);
        assert_eq!(crossed.usage_percent, 42.0);
    }
}
