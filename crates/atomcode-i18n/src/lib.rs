//! What this build says to a person, in the language they chose.
//!
//! Two tables, one locale:
//!
//! * [`product`] — what the CLI, the daemon, the setup flow and `/login` say.
//! * [`screen`] — what the full-screen terminal UI says.
//!
//! **Why one crate and not two.** The screen is an App apart and must not
//! depend on the product's config crate (`docs/adr/0022` §3), so before this
//! crate existed the screen had no table at all and said everything in Chinese
//! to everybody. The two ways out were a second table inside the screen — a
//! second `LOCALE` to remember to set, a second `{brand}` substitution to keep
//! in step — or one leaf both sides may depend on. This is that leaf: zero
//! atomcode dependencies, so nothing's layer is broken by reading it, and
//! [`runtime::set_locale`] is the only place the language is decided.
//!
//! New strings follow `docs/i18n-style.md`: a variant in `messages.rs`, an arm
//! in `en.rs`, an arm in `zh_cn.rs`. The `match` is exhaustive, so a forgotten
//! translation is a compile error rather than a sentence in the wrong language.

/// UI language selection (`Config.language`).
pub mod locale;

/// What the product says: CLI, daemon, setup, `/login`.
pub mod product;

/// What the full-screen terminal UI says.
pub mod screen;

/// The locale and the distribution's names — shared by both tables.
pub mod runtime;

pub use locale::Locale;
pub use runtime::{current_locale, set_brand, set_locale, test_lock, LocaleTestGuard};
