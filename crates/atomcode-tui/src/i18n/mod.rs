//! Screen-local import surface for the localization tables.
//!
//! The same shape `atomcode-tuix/src/i18n/mod.rs` has, for the same reason: a
//! front end writes `crate::i18n::{t, Msg}` and does not name the crate the
//! table lives in, so moving the table does not touch 500 call sites.
//!
//! The screen's sentences are `atomcode_i18n::screen`; the product's are
//! `atomcode_i18n::product`, which tuix and the CLI read. **One locale behind
//! both** — this crate may not depend on `atomcode-config` (`docs/adr/0022` §3),
//! and before the table was a leaf that meant the screen had no table at all
//! and said everything in Chinese to everybody.
//!
//! Adding a line: a variant in `atomcode-i18n/src/screen/messages.rs`, an arm
//! in `en.rs`, an arm in `zh_cn.rs`. The `match` is exhaustive — a missing
//! translation does not compile. Style: `docs/i18n-style.md`.
pub use atomcode_i18n::screen::*;
