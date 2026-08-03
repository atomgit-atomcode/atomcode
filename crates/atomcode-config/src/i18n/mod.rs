mod en;
mod messages;
mod zh_cn;

pub use crate::locale::Locale;
pub use messages::Msg;

use std::borrow::Cow;
use std::sync::RwLock;

static LOCALE: RwLock<Locale> = RwLock::new(Locale::En);

/// Translate a message using the current global locale.
///
/// Returns a `Cow<'static, str>` — static for literal translations,
/// owned for interpolated ones.
pub fn t(msg: Msg<'_>) -> Cow<'static, str> {
    t_with(current_locale(), msg)
}

/// Look up against an explicit locale.
pub fn t_with(locale: Locale, msg: Msg<'_>) -> Cow<'static, str> {
    match locale {
        Locale::En => en::en(msg),
        Locale::ZhCn => zh_cn::zh_cn(msg),
    }
}

/// Return the current global locale. Falls back to `Locale::En` if
/// the RwLock is poisoned.
pub fn current_locale() -> Locale {
    LOCALE.read().map(|g| *g).unwrap_or(Locale::En)
}

/// Switch the global locale used by [`t`]. Silently no-ops if the
/// RwLock is poisoned.
pub fn set_locale(locale: Locale) {
    if let Ok(mut g) = LOCALE.write() {
        *g = locale;
    }
}

/// Format a raw token count into a compact, scannable string for the
/// inter-turn divider. Large totals (e.g. `3672812`) are hard to read at a
/// glance, so we collapse them with `K` / `M` suffixes:
///   `< 1_000`        → `942`        (verbatim)
///   `>= 1_000`       → `3.67K`      (two decimals)
///   `>= 1_000_000`   → `3.67M`      (two decimals)
/// The caller appends the localised `tokens` word, so this returns only the
/// numeric part. Unit-agnostic across locales — the digits read the same.
pub fn fmt_tokens(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.2}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format the localized marker shown after a committed compaction.
pub fn format_compaction_mark(
    removed_messages: usize,
    estimated_tokens_before: usize,
    estimated_tokens_after: usize,
) -> String {
    if removed_messages > 0 {
        let before = fmt_compaction_tokens(estimated_tokens_before);
        let after = fmt_compaction_tokens(estimated_tokens_after);
        t(Msg::CompactMarkDrain {
            messages: removed_messages,
            before: &before,
            after: &after,
        })
        .into_owned()
    } else {
        let saved =
            fmt_compaction_tokens(estimated_tokens_before.saturating_sub(estimated_tokens_after));
        t(Msg::CompactMarkStub { saved: &saved }).into_owned()
    }
}

/// Format the localized acknowledgement for a user-requested compaction that
/// left the conversation unchanged.
pub fn format_compaction_noop(
    estimated_tokens_before: usize,
    estimated_tokens_after: usize,
    summary_would_grow: bool,
) -> String {
    if summary_would_grow {
        let before = fmt_compaction_tokens(estimated_tokens_before);
        let after = fmt_compaction_tokens(estimated_tokens_after);
        t(Msg::CompactNothingNoSavings {
            before: &before,
            after: &after,
        })
        .into_owned()
    } else {
        t(Msg::CompactNothingShort).into_owned()
    }
}

/// Format the localized acknowledgement for an accepted compaction that was
/// interrupted by runtime replacement or shutdown.
pub fn format_compaction_interrupted() -> String {
    t(Msg::CompactInterrupted).into_owned()
}

fn fmt_compaction_tokens(tokens: usize) -> String {
    if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Determine the initial locale from (in priority order):
/// CLI `--lang` flag, config file `language` field, environment
/// variables `LC_ALL` / `LC_MESSAGES` / `LANG`.
pub fn resolve_initial_locale(cli_lang: Option<&str>, config_lang: Option<Locale>) -> Locale {
    resolve_initial_locale_with_env(cli_lang, config_lang, &|k| std::env::var(k).ok())
}

#[doc(hidden)]
pub fn resolve_initial_locale_with_env(
    cli_lang: Option<&str>,
    config_lang: Option<Locale>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Locale {
    if let Some(s) = cli_lang {
        if let Ok(loc) = s.parse::<Locale>() {
            return loc;
        }
    }
    if let Some(loc) = config_lang {
        return loc;
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(val) = env(key) {
            if !val.is_empty() {
                return classify_env_locale(&val);
            }
        }
    }
    Locale::En
}

fn classify_env_locale(value: &str) -> Locale {
    let lower = value.to_ascii_lowercase();
    // All Chinese variants (zh_CN, zh_TW, zh_HK, …) map to ZhCn.
    // zh_TW / zh_HK intentionally fall back — no separate Traditional variant yet.
    if lower == "zh"
        || lower.starts_with("zh_")
        || lower.starts_with("zh-")
        || lower.starts_with("zh.")
    {
        Locale::ZhCn
    } else {
        Locale::En
    }
}

/// Serialization lock for tests that mutate the global locale.
/// Prevents test races when multiple tests call `set_locale`, AND
/// restores the original locale on guard drop so a test that flips
/// to `ZhCn` doesn't leak into the next test that assumes the
/// default `En`.
///
/// Exposed unconditionally (not `#[cfg(test)]`-gated) because tests in
/// downstream crates (atomcode-tuix, etc.) need to take this lock too,
/// and `cfg(test)` only applies to the crate currently being tested.
/// The lock is a `OnceLock` so it costs nothing at runtime until first
/// use.
///
/// Return value is a custom guard that:
///   1. Owns the underlying `MutexGuard<'static, ()>` so the lock is
///      released when it drops.
///   2. Captures `current_locale()` at construction.
///   3. Restores that captured locale in its own `Drop` (runs BEFORE
///      the inner MutexGuard's Drop, since fields drop in declaration
///      order — so the next test sees the restored locale AND the
///      lock is still held while restoration happens).
pub fn test_lock() -> LocaleTestGuard {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    // Recover from a poisoned mutex (a previous test panicked while
    // holding the guard). The locale value the panicking test wrote
    // is irrelevant — we restore from `current_locale()` next, and
    // each test sets its own desired locale immediately after taking
    // the lock. Without this, one panicking test would cascade and
    // fail every subsequent locale-touching test with PoisonError.
    let guard = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let original = current_locale();
    LocaleTestGuard {
        original,
        _guard: guard,
    }
}

/// RAII guard returned by `test_lock()`. Holds the serialisation
/// mutex AND restores the locale that was current at lock-acquire
/// time. Field declaration order matters: `original` (with its
/// `Drop` impl below) drops before `_guard`, so the locale is
/// restored while the lock is still held — the next waiter never
/// sees a transient mixed state.
pub struct LocaleTestGuard {
    original: Locale,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Drop for LocaleTestGuard {
    fn drop(&mut self) {
        set_locale(self.original);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_tokens_scales_with_magnitude() {
        // < 1_000 → verbatim, no suffix.
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(942), "942");
        assert_eq!(fmt_tokens(999), "999");
        // >= 1_000 → K with two decimals.
        assert_eq!(fmt_tokens(1_000), "1.00K");
        assert_eq!(fmt_tokens(1_696), "1.70K");
        assert_eq!(fmt_tokens(999_999), "1000.00K");
        // >= 1_000_000 → M with two decimals.
        assert_eq!(fmt_tokens(1_000_000), "1.00M");
        assert_eq!(fmt_tokens(3_672_812), "3.67M");
    }

    #[test]
    fn t_with_returns_english_for_en() {
        let s = t_with(Locale::En, Msg::WelcomeBannerLine1);
        assert!(s.starts_with("Welcome to AtomCode"));
    }

    #[test]
    fn t_with_returns_chinese_for_zh_cn() {
        let s = t_with(Locale::ZhCn, Msg::WelcomeBannerLine1);
        assert!(s.starts_with("欢迎使用 AtomCode"));
    }

    #[test]
    fn set_locale_flips_global() {
        let _g = test_lock();
        set_locale(Locale::ZhCn);
        assert_eq!(current_locale(), Locale::ZhCn);
        let s = t(Msg::WelcomeBannerLine1);
        assert!(s.starts_with("欢迎使用"));

        set_locale(Locale::En);
        assert_eq!(current_locale(), Locale::En);
        let s = t(Msg::WelcomeBannerLine1);
        assert!(s.starts_with("Welcome to AtomCode"));
    }

    #[test]
    fn err_unsupported_locale_includes_input() {
        let s = t_with(Locale::En, Msg::ErrUnsupportedLocale { input: "fr" });
        assert!(s.contains("fr"));
        let s = t_with(Locale::ZhCn, Msg::ErrUnsupportedLocale { input: "fr" });
        assert!(s.contains("fr"));
    }

    fn has_cjk(s: &str) -> bool {
        s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
    }

    #[test]
    fn turn_summary_appends_cached_pct_only_when_present() {
        let with = t_with(
            Locale::En,
            Msg::TurnSummary {
                done: "Dialed in",
                turn_count: 30,
                tool_call_count: 32,
                duration: "435.3s",
                total_tokens: 152_000,
                cached_pct: Some(97),
            },
        );
        assert!(with.contains("152.00K tokens · 97% cached"), "got: {with}");
        let without = t_with(
            Locale::En,
            Msg::TurnSummary {
                done: "Dialed in",
                turn_count: 30,
                tool_call_count: 32,
                duration: "435.3s",
                total_tokens: 152_000,
                cached_pct: None,
            },
        );
        assert!(
            without.trim_end().ends_with("152.00K tokens"),
            "got: {without}"
        );
        assert!(
            !without.contains("cached"),
            "no annotation when None: {without}"
        );
    }

    #[test]
    fn gateway_auth_unavailable_is_localized_and_keeps_url() {
        let url = "https://llm-api.atomgit.com/v1";
        let en = t_with(Locale::En, Msg::GatewayAuthUnavailable { base_url: url });
        assert!(en.contains(url), "EN must echo the base_url: {en}");
        assert!(en.to_lowercase().contains("gateway"), "EN keyword: {en}");
        let zh = t_with(Locale::ZhCn, Msg::GatewayAuthUnavailable { base_url: url });
        assert!(zh.contains(url), "ZH must echo the base_url: {zh}");
        assert!(has_cjk(&zh), "ZH must actually be Chinese: {zh}");
    }

    #[test]
    fn provider_init_frame_keeps_detail_both_locales() {
        let en = t_with(Locale::En, Msg::ProviderInitFailed { detail: "DETAIL_X" });
        assert!(en.contains("DETAIL_X"));
        let zh = t_with(Locale::ZhCn, Msg::ProviderInitFailed { detail: "DETAIL_X" });
        assert!(zh.contains("DETAIL_X"));
        assert!(has_cjk(&zh), "ZH frame must be Chinese: {zh}");
    }

    #[test]
    fn plugin_manager_empty_hints_advertise_esc() {
        // Regression: every plugin-manager screen advertises Esc-to-go-back in
        // its hint, EXCEPT these empty-state hints once did not — so an empty
        // list (e.g. /plugin → Installed with 0 plugins) looked frozen with no
        // visible way out. Keep the Esc affordance on the empty states too.
        fn has_esc(s: &str) -> bool {
            s.to_lowercase().contains("esc")
        }
        for (en, zh) in [
            (
                t_with(Locale::En, Msg::PluginMgrEmptyInstalled),
                t_with(Locale::ZhCn, Msg::PluginMgrEmptyInstalled),
            ),
            (
                t_with(Locale::En, Msg::PluginMgrEmptyMarketplaces),
                t_with(Locale::ZhCn, Msg::PluginMgrEmptyMarketplaces),
            ),
            (
                t_with(Locale::En, Msg::PluginMgrEmptyPlugins),
                t_with(Locale::ZhCn, Msg::PluginMgrEmptyPlugins),
            ),
        ] {
            assert!(has_esc(&en), "EN empty hint missing esc: {en}");
            assert!(has_esc(&zh), "ZH empty hint missing esc: {zh}");
        }
    }

    #[test]
    fn cli_flag_wins_over_everything() {
        let env = |_: &str| Some("zh_CN.UTF-8".to_string());
        assert_eq!(
            resolve_initial_locale_with_env(Some("en"), Some(Locale::ZhCn), &env),
            Locale::En
        );
    }

    #[test]
    fn config_beats_env() {
        let env = |_: &str| Some("zh_CN.UTF-8".to_string());
        assert_eq!(
            resolve_initial_locale_with_env(None, Some(Locale::En), &env),
            Locale::En
        );
    }

    #[test]
    fn env_zh_cn_resolves_to_zh_cn() {
        let env = |k: &str| {
            if k == "LANG" {
                Some("zh_CN.UTF-8".into())
            } else {
                None
            }
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::ZhCn
        );
    }

    #[test]
    fn env_zh_tw_maps_to_zh_cn() {
        let env = |k: &str| {
            if k == "LANG" {
                Some("zh_TW".into())
            } else {
                None
            }
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::ZhCn
        );
    }

    #[test]
    fn env_c_or_english_resolves_to_en() {
        let mk = |val: &'static str| {
            move |k: &str| {
                if k == "LANG" {
                    Some(val.to_string())
                } else {
                    None
                }
            }
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &mk("C")),
            Locale::En
        );
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &mk("en_US.UTF-8")),
            Locale::En
        );
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &mk("")),
            Locale::En
        );
    }

    #[test]
    fn env_no_locale_vars_resolves_to_en() {
        let env = |_: &str| None;
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::En
        );
    }

    #[test]
    fn lc_all_overrides_lc_messages_and_lang() {
        let env = |k: &str| match k {
            "LC_ALL" => Some("zh_CN.UTF-8".into()),
            "LANG" => Some("en_US.UTF-8".into()),
            _ => None,
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::ZhCn
        );
    }

    #[test]
    fn lc_messages_overrides_lang() {
        let env = |k: &str| match k {
            "LC_MESSAGES" => Some("zh_CN.UTF-8".into()),
            "LANG" => Some("en_US.UTF-8".into()),
            _ => None,
        };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::ZhCn
        );
    }

    #[test]
    fn compact_mark_drain_renders_numbers_and_arrow() {
        // Locale-invariant assertion (numbers + the → arrow appear in both en & zh).
        let s = crate::i18n::t(crate::i18n::Msg::CompactMarkDrain {
            messages: 12,
            before: "48.2K",
            after: "9.1K",
        });
        assert!(s.contains("12"), "message count missing: {s}");
        assert!(
            s.contains("48.2K") && s.contains("9.1K"),
            "token figures missing: {s}"
        );
        assert!(s.contains('→'), "before→after arrow missing: {s}");
        assert!(s.contains('~'), "estimate marker missing: {s}");
        assert!(s.contains("tok"), "token unit missing: {s}");
    }

    #[test]
    fn compact_mark_stub_renders_saved_without_arrow() {
        let s = crate::i18n::t(crate::i18n::Msg::CompactMarkStub { saved: "6.0K" });
        assert!(s.contains("6.0K"), "saved figure missing: {s}");
        assert!(
            !s.contains('→'),
            "stub marker shows a single figure, no arrow: {s}"
        );
        assert!(s.contains("tok"), "token unit missing: {s}");
    }

    #[test]
    fn format_compaction_mark_renders_drain_estimates() {
        let s = format_compaction_mark(129, 42_900, 11_103);

        assert!(s.contains("129") && s.contains("42.9K") && s.contains("11.1K"));
    }

    #[test]
    fn format_compaction_mark_renders_stub_savings() {
        let s = format_compaction_mark(0, 42_900, 34_320);

        assert!(s.contains("8.6K") && !s.contains('→'));
    }

    #[test]
    fn format_compaction_noop_distinguishes_net_loss() {
        let s = format_compaction_noop(5_000, 7_500, true);

        assert!(s.contains("5.0K") && s.contains("7.5K") && s.contains('→'));
    }

    #[test]
    fn format_compaction_interrupted_is_not_a_noop_message() {
        let s = format_compaction_interrupted();

        assert!(s.contains("interrupt") || s.contains("中断"));
        assert!(!s.contains("nothing to compact") && !s.contains("无需压缩"));
    }

    #[test]
    fn cli_flag_unparseable_falls_through() {
        let env = |_: &str| None;
        assert_eq!(
            resolve_initial_locale_with_env(Some("fr"), Some(Locale::ZhCn), &env),
            Locale::ZhCn
        );
        assert_eq!(
            resolve_initial_locale_with_env(Some("fr"), None, &env),
            Locale::En
        );
    }

    #[test]
    fn todo_panel_labels_render() {
        // Default locale (En); exact copy is locale-dependent, assert non-empty + digit.
        assert!(!t(Msg::TodoPanelTitle).is_empty());
        assert!(t(Msg::TodoPanelCompleted { n: 3 }).contains('3'));
        assert!(t(Msg::TodoPanelMore { n: 2 }).contains('2'));
    }

    #[test]
    fn welcome_tip_descriptions_present_both_langs() {
        // Check a representative subset of the new welcome-tips variants in both locales.
        macro_rules! check {
            ($variant:expr) => {{
                let en = t_with(Locale::En, $variant);
                assert!(!en.is_empty(), "EN empty for {}", stringify!($variant));
                let zh = t_with(Locale::ZhCn, $variant);
                assert!(!zh.is_empty(), "ZH empty for {}", stringify!($variant));
            }};
        }
        check!(Msg::WelcomeTipsHeading);
        check!(Msg::WelcomeTipLogin);
        check!(Msg::WelcomeTipGoal);
        check!(Msg::WelcomeTipLoop);
        check!(Msg::WelcomeTipSession);
        check!(Msg::WelcomeTipInit);
    }

    #[test]
    fn model_copy_explains_default_and_current_session_scope() {
        let en_desc = t_with(Locale::En, Msg::CmdDescModel);
        let zh_desc = t_with(Locale::ZhCn, Msg::CmdDescModel);
        assert!(en_desc.contains("default") && en_desc.contains("this session"));
        assert!(zh_desc.contains("默认") && zh_desc.contains("当前会话"));

        let en_switched = t_with(
            Locale::En,
            Msg::ModelSwitchedAndDefault {
                provider: "provider",
                model: "model",
            },
        );
        let zh_switched = t_with(
            Locale::ZhCn,
            Msg::ModelSwitchedAndDefault {
                provider: "provider",
                model: "model",
            },
        );
        assert!(en_switched.contains("default for new sessions"));
        assert!(zh_switched.contains("新会话默认"));

        let ephemeral = t_with(
            Locale::En,
            Msg::ModelSwitched {
                provider: "provider",
                model: "model",
            },
        );
        assert!(ephemeral.contains("this session"));
        assert!(!ephemeral.contains("default"));
    }

    #[test]
    fn provider_panel_copy_is_localized_in_both_languages() {
        let en_tabs = (
            t_with(Locale::En, Msg::ProviderPanelTabAccounts),
            t_with(Locale::En, Msg::ProviderPanelTabModels),
        );
        let zh_tabs = (
            t_with(Locale::ZhCn, Msg::ProviderPanelTabAccounts),
            t_with(Locale::ZhCn, Msg::ProviderPanelTabModels),
        );
        assert_eq!(en_tabs.0, "Accounts");
        assert_eq!(en_tabs.1, "Models");
        assert_eq!(zh_tabs.0, "账号");
        assert_eq!(zh_tabs.1, "模型");

        let en_hint = t_with(Locale::En, Msg::ProviderPanelAccountsHint);
        let zh_hint = t_with(Locale::ZhCn, Msg::ProviderPanelAccountsHint);
        assert!(en_hint.contains("add") && en_hint.contains("delete"));
        assert!(zh_hint.contains("添加") && zh_hint.contains("删除"));
        assert!(en_hint.contains("Ctrl+Dx2"));
        assert!(zh_hint.contains("Ctrl+Dx2"));
        assert!(en_hint.contains("Ctrl+A") && en_hint.contains("Ctrl+E"));
        assert!(zh_hint.contains("Ctrl+A") && zh_hint.contains("Ctrl+E"));
        assert_eq!(
            t_with(Locale::En, Msg::ProviderPanelAddModelRow),
            "+ Add model"
        );
        assert_eq!(
            t_with(Locale::ZhCn, Msg::ProviderPanelAddModelRow),
            "＋ 添加模型"
        );

        assert_eq!(
            t_with(Locale::En, Msg::ProviderPanelEmptyModels),
            "(No models yet — press Ctrl+A to add one)"
        );
        assert_eq!(
            t_with(Locale::ZhCn, Msg::ProviderPanelEmptyModels),
            "（尚无模型 — 按 Ctrl+A 添加）"
        );

        assert_eq!(
            t_with(
                Locale::En,
                Msg::ProviderPanelModelSaved {
                    model: "deepseek-chat"
                }
            ),
            "Saved model \"deepseek-chat\"."
        );
        assert_eq!(
            t_with(
                Locale::ZhCn,
                Msg::ProviderPanelModelSaved {
                    model: "deepseek-chat"
                }
            ),
            "已保存模型“deepseek-chat”。"
        );

        let en_row = t_with(Locale::En, Msg::ProviderPanelModelCount { count: 3 });
        let zh_row = t_with(Locale::ZhCn, Msg::ProviderPanelModelCount { count: 3 });
        assert_eq!(en_row, "3 models");
        assert_eq!(zh_row, "3 个模型");
    }

    #[test]
    fn usage_modal_i18n_present_both_langs() {
        macro_rules! check {
            ($variant:expr) => {{
                let en = t_with(Locale::En, $variant);
                assert!(!en.is_empty(), "EN empty for {}", stringify!($variant));
                let zh = t_with(Locale::ZhCn, $variant);
                assert!(!zh.is_empty(), "ZH empty for {}", stringify!($variant));
            }};
        }
        check!(Msg::UsageTabCurrent);
        check!(Msg::UsageTabOverview);
        check!(Msg::UsageTabModels);
        check!(Msg::UsageCurrentTitle);
        check!(Msg::UsageResetsIn { hms: "01:23:45" });
        check!(Msg::UsageWindowUnavailable);
        check!(Msg::UsageStatFavorite);
        check!(Msg::UsageStatTotal);
        check!(Msg::UsageStatRequests);
        check!(Msg::UsageStatActiveDays);
        check!(Msg::UsageStatMostActive);
        check!(Msg::UsageStatLongestStreak);
        check!(Msg::UsageStatCurrentStreak);
        check!(Msg::UsageHeatLess);
        check!(Msg::UsageHeatMore);
        check!(Msg::UsageModelsTitle);
        check!(Msg::UsageNoData);
        check!(Msg::UsageFooterHint);
        check!(Msg::UsageFetchFailed { error: "timeout" });
        check!(Msg::UsagePlanTitle);
        check!(Msg::UsagePlanActive);
        check!(Msg::UsagePlanExpired);
        check!(Msg::UsagePlanClaimedExpires {
            claimed: "2026-06-01",
            expires: "2026-07-01"
        });
        check!(Msg::UsagePlanRemaining {
            remaining: 5,
            total: 30
        });
        check!(Msg::UsageCopied);
    }

    #[test]
    fn network_connect_hint_present_both_langs() {
        let _g = test_lock();
        let en = t_with(Locale::En, Msg::NetworkConnectHint);
        let zh = t_with(Locale::ZhCn, Msg::NetworkConnectHint);
        assert!(!en.trim().is_empty(), "en hint must be non-empty");
        assert!(!zh.trim().is_empty(), "zh hint must be non-empty");
        // Mentions the actionable knobs so the hint is useful.
        assert!(
            en.contains("/proxy") && en.contains("HTTPS_PROXY"),
            "en: {en}"
        );
        assert!(
            zh.contains("/proxy") && zh.contains("HTTPS_PROXY"),
            "zh: {zh}"
        );
    }
}
