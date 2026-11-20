//! Tool-instance health, bounded failover and cancellation. No global session owner.
use super::backends::SearchError;
use super::*;
use std::future::Future;
use tokio::time::{timeout, Instant};

const POOL: [SearchProvider; 5] = [
    SearchProvider::Exa,
    SearchProvider::Parallel,
    SearchProvider::Tavily,
    SearchProvider::Firecrawl,
    SearchProvider::TinyFish,
];

#[derive(Default)]
pub(super) struct Health {
    preferred: Option<SearchProvider>,
    cooldowns: HashMap<SearchProvider, Instant>,
}
impl WebSearchTool {
    pub(super) async fn route<F, Fut>(
        &self,
        query: &str,
        ctx: &ToolContext,
        mut attempt: F,
    ) -> ToolResult
    where
        F: FnMut(SearchProvider) -> Fut,
        Fut: Future<Output = Result<String, SearchError>>,
    {
        if ctx.cancel.is_cancelled() {
            return err("web_search: cancelled");
        }
        let automatic = self.provider == SearchProvider::Auto;
        let now = Instant::now();
        let candidates = if automatic {
            let mut health = self.health.lock().unwrap_or_else(|e| e.into_inner());
            health.cooldowns.retain(|_, until| *until > now);
            let mut pool = POOL.to_vec();
            if let Some(preferred) = health.preferred {
                if let Some(at) = pool.iter().position(|id| *id == preferred) {
                    pool.swap(0, at);
                }
            }
            pool
        } else {
            vec![self.provider]
        };
        let deadline = now + Duration::from_secs(if automatic { 45 } else { REQUEST_TIMEOUT_SECS });
        let mut failures = Vec::new();
        let mut all_transport = true;
        let mut attempted = false;
        for provider in candidates {
            if automatic
                && self
                    .health
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .cooldowns
                    .get(&provider)
                    .is_some_and(|until| *until > Instant::now())
            {
                failures.push(format!("{}: cooling down", provider.name()));
                all_transport = false;
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                failures.push("total search timeout reached".into());
                break;
            }
            let budget = remaining.min(Duration::from_secs(if automatic {
                8
            } else {
                REQUEST_TIMEOUT_SECS
            }));
            attempted = true;
            let result = tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => return err("web_search: cancelled"),
                result = timeout(budget, attempt(provider)) => result.unwrap_or_else(|_| Err(SearchError::timeout())),
            };
            match result {
                Ok(text) => {
                    if automatic {
                        let mut health = self.health.lock().unwrap_or_else(|e| e.into_inner());
                        health.preferred = Some(provider);
                        health.cooldowns.remove(&provider);
                    }
                    return ok(format!(
                        "Search results for \"{query}\" (via {}):\n\n{}",
                        provider.name(),
                        self.redact(text)
                    ));
                }
                Err(error) => {
                    all_transport &= error.transport;
                    if automatic {
                        let now = Instant::now();
                        let until = now
                            .checked_add(error.cooldown)
                            .unwrap_or(now + Duration::from_secs(365 * 86400));
                        self.health
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .cooldowns
                            .insert(provider, until);
                    }
                    // Redact BEFORE truncating, otherwise the cut can expose a
                    // partial key that a complete-key replacement cannot find.
                    let detail: String = self
                        .redact(error.message)
                        .chars()
                        .take(ERROR_BODY_MAX_CHARS)
                        .collect();
                    failures.push(format!("{}: {detail}", provider.name()));
                }
            }
        }
        // A blocked search provider is not proof that every public network is
        // unreachable; never flip offline while a replacement succeeded.
        if attempted && all_transport {
            note_network_unreachable();
        }
        fail(format!(
            "web_search: no available search provider. {}",
            failures.join("; ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn context() -> ToolContext {
        ToolContext {
            working_dir: Default::default(),
            cancel: Default::default(),
            progress: Default::default(),
            requester: None,
        }
    }
    fn tool() -> WebSearchTool {
        // Environment-independent fixtures: no real HTTP or credentials.
        WebSearchTool {
            provider: SearchProvider::Auto,
            exa_api_key: None,
            keys: Default::default(),
            health: Default::default(),
        }
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn rate_limits_switch_once_and_keep_the_successful_replacement() {
        let tool = tool();
        let mut seen = Vec::new();
        let result = tool
            .route("query", &context(), |provider| {
                seen.push(provider);
                std::future::ready(if provider == SearchProvider::Exa {
                    Err(SearchError {
                        message: "HTTP 429".into(),
                        cooldown: Duration::from_secs(120),
                        transport: false,
                    })
                } else {
                    Ok("results".into())
                })
            })
            .await;
        assert!(!result.is_error);
        assert_eq!(seen, [SearchProvider::Exa, SearchProvider::Parallel]);
        tokio::time::advance(Duration::from_secs(121)).await;
        seen.clear();
        tool.clone()
            .route("next", &context(), |provider| {
                seen.push(provider);
                std::future::ready(Ok("results".into()))
            })
            .await;
        assert_eq!(seen, [SearchProvider::Parallel]);
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn failures_are_bounded_cool_down_and_redact_even_body_echoes() {
        let mut tool = tool();
        tool.exa_api_key = Some("secret/key".into());
        tool.keys
            .insert(SearchProvider::Tavily, "second-secret".into());
        let calls = AtomicUsize::new(0);
        let run = || {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Err(SearchError::protocol(
                "secret/key secret%2Fkey second-secret",
            )))
        };
        let result = tool.route("q", &context(), |_| run()).await;
        assert!(result.is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert!(!result.content.contains("secret"));
        assert!(!format!("{tool:?}").contains("secret"));
        tool.route("q", &context(), |_| run()).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            5,
            "no call while every provider is cooling down"
        );
        tokio::time::advance(Duration::from_secs(61)).await;
        tool.route("q", &context(), |_| run()).await;
        assert_eq!(calls.load(Ordering::SeqCst), 10);
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn fixed_provider_never_fails_over_and_empty_results_are_success() {
        let mut tool = tool();
        tool.provider = SearchProvider::Tavily;
        let mut seen = Vec::new();
        let result = tool
            .route("q", &context(), |id| {
                seen.push(id);
                std::future::ready(Err(SearchError::protocol("HTTP 503")))
            })
            .await;
        assert!(result.is_error);
        assert_eq!(seen, [SearchProvider::Tavily]);
        tool.provider = SearchProvider::Auto;
        seen.clear();
        assert!(
            !tool
                .route("q", &context(), |id| {
                    seen.push(id);
                    std::future::ready(Ok("No search results found.".into()))
                })
                .await
                .is_error
        );
        assert_eq!(seen.len(), 1);
    }
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn timeout_tries_each_backend_once_and_cancel_does_not_poison_health() {
        let tool = tool();
        let ctx = context();
        let calls = AtomicUsize::new(0);
        let started = Instant::now();
        let result = tool
            .route("q", &ctx, |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                std::future::pending()
            })
            .await;
        assert!(result.is_error);
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert!(started.elapsed() <= Duration::from_secs(45));
        let tool = tool.clone();
        tool.health.lock().unwrap().cooldowns.clear();
        let cancel = ctx.cancel.clone();
        let result = tool
            .route("q", &ctx, |_| {
                cancel.cancel();
                std::future::pending()
            })
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("cancelled"));
        assert!(tool.health.lock().unwrap().cooldowns.is_empty());
    }
}
