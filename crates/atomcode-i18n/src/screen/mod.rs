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
