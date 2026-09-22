//! What the two tables share: which language is in force, and the two names a
//! distribution writes into its own sentences.
//!
//! There is **one** locale for the process, not one per table. A screen that
//! kept its own would have to be told twice — and the day one of the two calls
//! is forgotten, the status bar is in English while the welcome block is in
//! Chinese, which is the failure this crate exists to make impossible. The
//! product's table and the screen's table read the same static; `/language`
//! sets it once (`atomcode_cli::tui_settings::apply_language`).

use std::borrow::Cow;
use std::sync::RwLock;

use crate::locale::Locale;

static LOCALE: RwLock<Locale> = RwLock::new(Locale::En);

/// Cached brand name shown in i18n strings via the `{brand}` placeholder.
/// Settled from `Config::ui::brand_name` at startup by [`set_brand`];
/// falls back to `"AtomCode"` (upstream default) until then.
///
/// `RwLock` (not `OnceLock`) so the authoritative `Config` load can override
/// the lightweight pre-scan value used for clap `--help` rendering. The
/// pre-scan reads only the default config path; a `--config <custom>` or
/// `--seed-config` first-run must still surface the real brand, so the
/// later authoritative `set_brand` call wins. Writes are serial and rare
/// (two per launch), reads are the hot path through `substitute_placeholders`.
static BRAND: RwLock<String> = RwLock::new(String::new());

/// Cached OAuth provider display name shown via the `{oauth}` placeholder.
/// Same write-twice / read-hot pattern as [`BRAND`].
static OAUTH: RwLock<String> = RwLock::new(String::new());

/// Settle the brand/OAuth display names from the loaded config. Called twice
/// from `main`: once with a pre-scan value before clap `--help` (so localised
/// help shows the distribution's brand), then again with the authoritative
/// `Config` value after load — the second call OVERWRITES the first, so
/// `--config <custom>` / `--seed-config` paths surface the real brand, not
/// the default-path pre-scan.
///
/// A mid-session `/reload` DOES flip the brand (last write wins). This is
/// intentional — the pre-scan is best-effort, the authoritative load must
/// be able to correct it, and `/reload` is an explicit user action.
pub fn set_brand(brand: &str, oauth: &str) {
    if let Ok(mut guard) = BRAND.write() {
        *guard = brand.to_string();
    }
    if let Ok(mut guard) = OAUTH.write() {
        *guard = oauth.to_string();
    }
}

fn brand() -> String {
    BRAND
        .read()
        .map(|g| g.clone())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "AtomCode".to_string())
}

fn oauth() -> String {
    OAUTH
        .read()
        .map(|g| g.clone())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "AtomGit OAuth".to_string())
}

/// Replace `{brand}` and `{oauth}` placeholders in a rendered i18n string.
/// Static when no placeholder is present (the common case), so `Cow` stays
/// borrowed and no allocation happens on the hot path.
///
/// Public so sibling crates (tuix `BUILTIN_COMMANDS` static fallback descs)
/// can run the same substitution on their own non-`Msg` strings, keeping the
/// fallback path consistent with the `t_with()` render path.
pub fn substitute_placeholders<'a>(raw: Cow<'a, str>) -> Cow<'a, str> {
    if !raw.contains('{') {
        return raw;
    }
    let owned = raw
        .replace("{brand}", &brand())
        .replace("{oauth}", &oauth());
    Cow::Owned(owned)
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

    /// The env fallback only runs when neither flag nor file said anything —
    /// and every Chinese variant lands on the one table there is.
    #[test]
    fn the_environment_is_read_only_when_nothing_else_answered() {
        let env = |want: &'static str| move |key: &str| (key == "LANG").then(|| want.to_string());
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env("zh_CN.UTF-8")),
            Locale::ZhCn
        );
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env("zh_TW.UTF-8")),
            Locale::ZhCn
        );
        assert_eq!(
            resolve_initial_locale_with_env(None, None, &env("en_US.UTF-8")),
            Locale::En
        );
        // The file wins over the environment, and the flag over the file.
        assert_eq!(
            resolve_initial_locale_with_env(None, Some(Locale::En), &env("zh_CN.UTF-8")),
            Locale::En
        );
        assert_eq!(
            resolve_initial_locale_with_env(Some("zh"), Some(Locale::En), &env("en_US.UTF-8")),
            Locale::ZhCn
        );
    }
}
