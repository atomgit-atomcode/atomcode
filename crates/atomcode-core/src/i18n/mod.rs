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

/// Determine the initial locale from (in priority order):
/// CLI `--lang` flag, config file `language` field, environment
/// variables `LC_ALL` / `LC_MESSAGES` / `LANG`.
pub fn resolve_initial_locale(
    cli_lang: Option<&str>,
    config_lang: Option<Locale>,
) -> Locale {
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
        assert!(without.trim_end().ends_with("152.00K tokens"), "got: {without}");
        assert!(!without.contains("cached"), "no annotation when None: {without}");
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
        let env =
            |k: &str| if k == "LANG" { Some("zh_CN.UTF-8".into()) } else { None };
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env),
            Locale::ZhCn
        );
    }

    #[test]
    fn env_zh_tw_maps_to_zh_cn() {
        let env = |k: &str| if k == "LANG" { Some("zh_TW".into()) } else { None };
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
        assert!(s.contains("48.2K") && s.contains("9.1K"), "token figures missing: {s}");
        assert!(s.contains('→'), "before→after arrow missing: {s}");
        assert!(s.contains('~'), "estimate marker missing: {s}");
        assert!(s.contains("tok"), "token unit missing: {s}");
    }

    #[test]
    fn compact_mark_stub_renders_saved_without_arrow() {
        let s = crate::i18n::t(crate::i18n::Msg::CompactMarkStub { saved: "6.0K" });
        assert!(s.contains("6.0K"), "saved figure missing: {s}");
        assert!(!s.contains('→'), "stub marker shows a single figure, no arrow: {s}");
        assert!(s.contains("tok"), "token unit missing: {s}");
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
}
