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
