//! What the full-screen terminal UI says.
//!
//! Separate table from [`crate::product`], same locale. The split is by who
//! says it: a line drawn by `atomcode-tui` is here, a line the CLI or the
//! daemon prints is there. Neither crate can see the other's table, and neither
//! needs to — [`crate::runtime`] holds the one language switch.
//!
//! **What does not belong here.** Fixture text that exists to prove the
//! renderer can draw it (`conformance.rs`'s CJK samples, `--audit`'s demo
//! input) stays a literal: it is the *subject* of the drawing, not a sentence
//! to a person, and translating it would delete the property it was written to
//! check. `gates/tui-i18n.sh` exempts those two files by name for that reason.

mod en;
mod messages;
mod zh_cn;

/// The product's table, reachable from screen code.
///
/// **A sentence this build already says is not written twice.** The approval
/// answers, the provider tabs, "how long ago", the todo header — tuix says all
/// of them, so the screen reaches for that entry instead of restating it, and
/// the two front ends cannot drift into two wordings for one thing. At the call
/// site it reads `product::t(product::Msg::ApprovalAllowOnce)`, which also says
/// *why* that line is not in the screen's table.
/// `gates/i18n-no-double-wording.sh` fails on a literal that appears in both.
pub use crate::product;

pub use crate::locale::Locale;
pub use messages::Msg;

// The one locale and the distribution's names, reached through this table too,
// so a screen file needs exactly one `use`.
pub use crate::runtime::{current_locale, set_locale, test_lock, LocaleTestGuard};

use std::borrow::Cow;

/// Translate a screen message using the current global locale.
pub fn t(msg: Msg<'_>) -> Cow<'static, str> {
    t_with(current_locale(), msg)
}

/// Look up against an explicit locale — what a test asserts with, and what a
/// caller rendering for someone else's session would use.
pub fn t_with(locale: Locale, msg: Msg<'_>) -> Cow<'static, str> {
    let raw = match locale {
        Locale::En => en::en(msg),
        Locale::ZhCn => zh_cn::zh_cn(msg),
    };
    crate::runtime::substitute_placeholders(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/keys` 的两列表格（经典界面同款式）：两种语言各自成立，英文表里不许
    /// 混入中文。翻 locale 的测试必须持 `test_lock` —— `set_locale` 是进程
    /// 全局的，不锁会毒到并行的其他判据。
    #[test]
    fn keys_table_and_help_header_in_both_locales() {
        let _g = test_lock();

        set_locale(Locale::ZhCn);
        let keys = t(Msg::KeysHelp);
        let zh_header = keys
            .lines()
            .find(|l| !l.trim().is_empty())
            .expect("zh header line");
        assert_eq!(zh_header.trim_end(), "  键盘快捷键", "zh header: {keys}");
        for section in [
            "── 输入 ──",
            "── 回合进行中 ──",
            "── 空闲时 ──",
            "── 翻看与显示 ──",
            "── 模式与注入 ──",
        ] {
            assert!(keys.contains(section), "zh 缺分节 {section}:{keys}");
        }
        for key in [
            "Ctrl+O / Alt+R",
            "Ctrl+T",
            "Ctrl+L",
            "Ctrl+G / /mouse",
            "Ctrl+R",
            "Shift+Tab",
            "/showinject [名字]",
        ] {
            assert!(keys.contains(key), "zh 缺键位 {key}:{keys}");
        }

        set_locale(Locale::En);
        let keys = t(Msg::KeysHelp);
        let en_header = keys
            .lines()
            .find(|l| !l.trim().is_empty())
            .expect("en header line");
        assert_eq!(
            en_header.trim_end(),
            "  Keyboard shortcuts",
            "en header: {keys}"
        );
        for section in [
            "── Input ──",
            "── During a turn ──",
            "── Idle ──",
            "── Reading & display ──",
            "── Modes & injections ──",
        ] {
            assert!(
                keys.contains(section),
                "en missing section {section}: {keys}"
            );
        }
        assert!(
            !keys.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c)),
            "the English table must stay English: {keys}"
        );
    }

    /// A message names a key in words, never as `⏎`/`↵`: these strings reach
    /// the screen without passing through the key legend
    /// (`atomcode_tui::widget::keys`), and on a terminal without Unicode the
    /// downgrade table turns both glyphs into `<` — the left arrow, not
    /// Enter. The same rule as `product`'s
    /// `a_message_names_a_key_in_words_not_as_a_glyph`, for the tables the
    /// row-assembled screen reads.
    #[test]
    fn a_message_names_a_key_in_words_not_as_a_glyph() {
        for (table, source) in [
            ("en", include_str!("en.rs")),
            ("zh_cn", include_str!("zh_cn.rs")),
        ] {
            for glyph in ['\u{23CE}', '\u{21B5}'] {
                assert!(
                    !source.contains(glyph),
                    "screen/{table}.rs names a key as {glyph:?}: say the word"
                );
            }
        }
    }
}

#[cfg(test)]
mod provider_message_tests {
    use super::*;
    #[test]
    fn provider_stop_and_retry_are_localized_without_a_global_locale_change() {
        for (locale, stopped, denied, retry_word) in [
            (Locale::ZhCn, "模型服务错误", "模型服务拒绝了请求", "重试"),
            (
                Locale::En,
                "Model service error",
                "Model service denied the request",
                "retry",
            ),
        ] {
            assert!(t_with(locale, Msg::StopProviderError).contains(stopped));
            let reason = t_with(locale, Msg::ProviderAccessDenied);
            assert_eq!(reason, denied);
            let reason = t_with(
                locale,
                Msg::ProviderErrorStatus {
                    reason: &reason,
                    status: 403,
                },
            );
            let text = t_with(
                locale,
                Msg::TranscriptProviderRetry {
                    reason: &reason,
                    seconds: 6,
                    attempt: 2,
                    max: 2,
                },
            );
            assert!(text.contains(retry_word));
            assert!(text.contains("403") && text.contains("2/2"));
            assert!(!text.contains("litellm"));
            assert!(!t_with(locale, Msg::ProviderCheckAccess).is_empty());
            assert!(!t_with(locale, Msg::ProviderErrorDetails).is_empty());
        }
    }
}
