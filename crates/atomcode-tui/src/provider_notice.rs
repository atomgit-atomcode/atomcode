//! Presentation only: preserve original errors and retry policy unchanged.
use crate::block::{hash_of, Content, ContentHash, RenderCtx};
use crate::content::NoticeBlock;
use crate::frame::Line;
use crate::i18n::{t, Msg};
use std::sync::Arc;

fn status(error: &str) -> Option<u16> {
    // Read an explicit outer status, never digits in model names or stack traces.
    let error = error
        .trim()
        .strip_prefix("provider error: ")
        .unwrap_or(error.trim());
    let code = error
        .strip_prefix("HTTP ")
        .or_else(|| error.strip_prefix('['))?;
    let digits = code.get(..3)?;
    if !matches!(code.as_bytes().get(3), Some(b']' | b':' | b' ')) {
        return None;
    }
    digits
        .parse::<u16>()
        .ok()
        .filter(|n| (400..600).contains(n))
}

pub(crate) fn reason(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    let code = status(error);
    let msg = match code {
        Some(401) => Msg::ProviderAuthenticationFailed,
        Some(403) => Msg::ProviderAccessDenied,
        Some(429) => Msg::ProviderRateLimitError,
        Some(503) => Msg::ProviderUnavailable,
        Some(500 | 502 | 504) => {
            if lower.contains("overloaded") {
                Msg::ProviderBusy
            } else {
                Msg::ProviderUnavailable
            }
        }
        None if lower.contains("overloaded") => Msg::ProviderBusy,
        None if [
            "connection refused",
            "connection reset",
            "dns error",
            "failed to lookup",
            "error sending request",
        ]
        .iter()
        .any(|s| lower.contains(s)) =>
        {
            Msg::ProviderConnectionError
        }
        _ => Msg::ProviderUnknownError,
    };
    let reason = t(msg).into_owned();
    match code {
        Some(status) => t(Msg::ProviderErrorStatus {
            reason: &reason,
            status,
        })
        .into_owned(),
        None => reason,
    }
}

pub(crate) fn advice(error: &str) -> String {
    let action = match status(error) {
        Some(401) => Msg::ProviderCheckCredentials,
        Some(403) => Msg::ProviderCheckAccess,
        _ if reason(error) == t(Msg::ProviderConnectionError) => Msg::ProviderCheckConnection,
        Some(429 | 500 | 502 | 503 | 504) => Msg::ProviderTryAgain,
        None if error.to_ascii_lowercase().contains("overloaded") => Msg::ProviderTryAgain,
        _ => Msg::ProviderCheckRequest,
    };
    let mut text = format!("{} {}", reason(error), t(action));
    let lower = error.to_ascii_lowercase();
    if lower.contains("no fallback model group found") {
        text.push(' ');
        text.push_str(&t(Msg::ProviderNoFallback));
    }
    text
}

/// The summary is localized on replay; the full diagnostic stays byte-for-byte intact.
#[derive(Debug)]
pub(crate) struct DiagnosticBlock {
    pub summary: Arc<dyn Content>,
    pub detail: String,
}
impl Content for DiagnosticBlock {
    fn kind(&self) -> &'static str {
        "provider_diagnostic"
    }
    fn content_hash(&self) -> ContentHash {
        hash_of(&[
            "provider_diagnostic",
            &self.summary.content_hash().to_string(),
            &self.detail,
        ])
    }
    fn summary_lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let mut lines = self.summary.lines(ctx);
        if self.summary.kind() == "turn_end" {
            lines.extend(
                NoticeBlock {
                    detail: t(Msg::ProviderErrorDetails).into_owned(),
                }
                .lines(ctx),
            );
        }
        lines
    }
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let mut lines = self.summary_lines(ctx);
        lines.extend(
            NoticeBlock {
                detail: self.detail.clone(),
            }
            .lines(ctx),
        );
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Presentation, Showing};

    fn plain(lines: Vec<Line>) -> String {
        lines.iter().map(Line::plain).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn service_errors_have_actions_without_claiming_a_403_is_quota_exhaustion() {
        let denied = "HTTP 403: [403] litellm.APIError: overloaded; No fallback model group found for original model_group=lumen-alpha";
        let text = advice(denied);
        assert!(text.contains(&*t(Msg::ProviderAccessDenied)));
        assert!(text.contains(&*t(Msg::ProviderCheckAccess)));
        assert!(text.contains(&*t(Msg::ProviderNoFallback)));
        assert!(!text.contains("lumen-alpha"));
        assert!(!text.contains(&*t(Msg::ProviderBusy)));
        let busy = advice("provider error: [500] litellm: Our servers are currently overloaded");
        assert!(busy.contains(&*t(Msg::ProviderBusy)));
        assert!(busy.contains(&*t(Msg::ProviderTryAgain)));
        assert!(advice("HTTP 503: unavailable").contains(&*t(Msg::ProviderUnavailable)));
        assert!(advice("HTTP 401: denied").contains(&*t(Msg::ProviderCheckCredentials)));
        assert_eq!(
            status("model-503 returned an unknown error with nested [403]"),
            None
        );
        assert_eq!(status("HTTP 5030: unknown"), None);
    }

    #[test]
    fn diagnostics_are_compact_by_default_and_retain_the_full_original_error() {
        let raw = "HTTP 503: litellm.ServiceUnavailableError; model_group=lumen-alpha; ".repeat(20);
        let block = DiagnosticBlock {
            summary: Arc::new(NoticeBlock {
                detail: reason(&raw),
            }),
            detail: raw.clone(),
        };
        let ctx = RenderCtx::bare(80);
        assert!(Presentation::default_folds().is_folded(block.kind()));
        let summary = plain(block.summary_lines(&ctx));
        assert!(!summary.contains("litellm"));
        assert!(summary.contains("503"));
        assert!(block.summary_lines(&ctx).len() < 5);
        assert_eq!(block.detail, raw);
        assert!(plain(block.lines(&ctx)).contains("lumen-alpha"));
        let mut presentation = Presentation::default_folds();
        presentation.show_as(block.kind(), Showing::Open);
        assert!(!presentation.is_folded(block.kind()));
    }
}
