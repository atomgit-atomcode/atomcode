//! Presentation only: preserve original errors and retry policy unchanged.
use crate::block::{hash_of, Content, ContentHash, RenderCtx};
use crate::content::NoticeBlock;
use crate::frame::Line;
use crate::i18n::{t, Msg};
use std::sync::Arc;

fn authentication_expired(error: &str) -> bool {
    use atomcode_i18n::product::{t_with, Locale, Msg};
    // The provider's terminal auth error has no HTTP prefix. Match only its
    // known headline, including persisted errors from the other UI language.
    let error = error
        .trim()
        .strip_prefix("provider error: ")
        .unwrap_or(error.trim());
    [Locale::En, Locale::ZhCn]
        .into_iter()
        .any(|locale| error == t_with(locale, Msg::ChatAuthExpired))
}

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
    if authentication_expired(error) {
        return atomcode_i18n::product::t(atomcode_i18n::product::Msg::ChatAuthExpired)
            .into_owned();
    }
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
        None if partial_reply_preserved(error) => Msg::ProviderPartialReply,
        None if lower.contains("without sending tls close_notify") => Msg::ProviderTlsInterrupted,
        // i18n-exempt: protocol marker, classified not shown
        None if lower.contains("connection timed out") || error.contains("等待首字节超过") => {
            Msg::ProviderConnectionTimeout
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

fn partial_reply_preserved(error: &str) -> bool {
    let error = error
        .trim()
        .strip_prefix("provider error: ")
        .unwrap_or(error.trim());
    // i18n-exempt: protocol marker, classified not shown
    error.starts_with("响应中断:为避免重复输出或工具执行,未自动重放;已保留可安全保存的部分回复。")
}

pub(crate) fn advice(error: &str) -> String {
    if authentication_expired(error) {
        return reason(error);
    }
    let action = match status(error) {
        Some(401) => Msg::ProviderCheckCredentials,
        Some(403) => Msg::ProviderCheckAccess,
        None if partial_reply_preserved(error) => Msg::ProviderContinueReply,
        None if reason(error) == t(Msg::ProviderConnectionTimeout)
            || reason(error) == t(Msg::ProviderTlsInterrupted) =>
        {
            Msg::ProviderCheckConnection
        }
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
    fn transport_summaries_preserve_the_phase_and_recovery_action() {
        let tls = "open failed: peer closed connection without sending TLS close_notify";
        assert_eq!(reason(tls), t(Msg::ProviderTlsInterrupted));
        let timeout = "open failed: Connection timed out (os error 110)";
        assert_eq!(reason(timeout), t(Msg::ProviderConnectionTimeout));
        let partial = "响应中断:为避免重复输出或工具执行,未自动重放;已保留可安全保存的部分回复。详情: connection timed out; without sending TLS close_notify";
        assert_eq!(reason(partial), t(Msg::ProviderPartialReply));
        assert!(advice(partial).contains(&*t(Msg::ProviderContinueReply)));
        assert!(!advice(tls).contains(&*t(Msg::ProviderContinueReply)));
        assert!(!advice(timeout).contains("/login"));
        assert_eq!(
            reason("HTTP 401: without sending TLS close_notify"),
            t(Msg::ProviderErrorStatus {
                reason: &t(Msg::ProviderAuthenticationFailed),
                status: 401
            })
        );
        let block = DiagnosticBlock {
            summary: Arc::new(NoticeBlock {
                detail: advice(partial),
            }),
            detail: partial.into(),
        };
        assert!(plain(block.summary_lines(&RenderCtx::bare(100)))
            .contains(&*t(Msg::ProviderContinueReply)));
        assert_eq!(block.detail, partial);
    }

    #[test]
    fn expired_login_keeps_its_action_in_the_folded_summary() {
        use atomcode_i18n::product::{t_with, Locale, Msg as ProductMsg};
        for locale in [Locale::En, Locale::ZhCn] {
            let raw = t_with(locale, ProductMsg::ChatAuthExpired);
            for error in [raw.to_string(), format!("provider error: {raw}")] {
                assert!(reason(&error).contains("/login"));
                assert_eq!(advice(&error), reason(&error));
                let block = DiagnosticBlock {
                    summary: Arc::new(NoticeBlock {
                        detail: advice(&error),
                    }),
                    detail: error.clone(),
                };
                assert!(plain(block.summary_lines(&RenderCtx::bare(80))).contains("/login"));
                assert_eq!(block.detail, error);
            }
        }
        // An arbitrary provider key rejection must not suggest product login.
        assert!(!advice("HTTP 401: invalid API key").contains("/login"));
        assert!(!authentication_expired("server says please /login"));
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
