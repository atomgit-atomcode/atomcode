//! The harness's front end: a full-screen terminal UI assembled from plugins,
//! on its way to being **the** product UI.
//!
//! # Where this stands
//!
//! `atomcode-tuix` is the UI that ships today. It is 9× the size of this crate
//! and bound to `atomcode-coding` by concrete types, not a seam — which is why
//! it is being replaced by this crate rather than bridged onto the harness.
//! The decision, the gap it has to close and the order to close it in are in
//! `docs/adr/0012`; the earlier "headless only" stance is `docs/adr/0011`,
//! superseded. Don't read tuix for what to build; build it here as rows.
//!
//! What makes this crate the one worth growing: end-to-end tests drive the
//! whole UI with no tty, no model and no human; `atomcode --tui --audit` checks
//! the screen's composition on a machine with no terminal; `--demo` prints one
//! composed frame. The headless surface is a row next to the terminal one, so
//! every feature added is testable the day it lands.
//!
//! The screen is an App of its own and drives an agent it does not build: a
//! host hands it a connection — the handle protocol and host control
//! (`docs/adr/0021`, `docs/adr/0022`). [`launch`] is how it is mounted.
//!
//! Nothing here is a monolith with extension points bolted on. The host owns
//! four things nobody else can — the surface, the event loop, layout
//! arbitration and focus — and everything visible is a row in the config tree:
//! a stream producer, a view module, a keymap, a command set.
//!
//! # The two properties this is built to hold
//!
//! **Time.** What a person sees is irreversible. A block opens `Live`, may
//! change while it is, and `settle`s once — after which its *content* is frozen
//! and only its *presentation* (folded, hidden, rewrapped) can change. Ordering
//! is append-only. The type system enforces it: nothing hands out `&mut` to a
//! settled block.
//!
//! **Space.** A frame is composed *for a realm*: the region tree says where a
//! module goes, realm visibility says which modules exist there. Those two axes
//! are orthogonal, and the second one is the same `visible_from` the service
//! table and the event bus use — so a delegated child agent cannot paint on its
//! parent's screen.
//!
//! See `docs/tui-composability.md` for the argument and `docs/adr/0004`–`0008`
//! for the decisions.

pub mod ansi;
pub mod ask;
pub mod attach;
pub mod block;
pub mod caps;
pub mod command;
pub mod commands;
pub mod conformance;
pub mod content;
pub mod diff;
pub mod el;
pub mod frame;
pub mod host;
pub mod image_cache;

pub mod i18n;
pub mod keymap;
pub mod launch;
pub mod layout;
pub mod markdown;
pub mod mcp;
pub mod menu;
pub mod module;
pub mod modules;
pub mod moment;
pub mod overlay;
pub mod places;
pub mod plugin;
pub mod plugins;
pub mod providers;
pub mod qr;
pub mod raster;
pub mod region;
pub mod resume;
pub mod rewind;
pub mod rows;
pub mod secret;
pub mod settings;
pub mod setup;
pub mod surface;
pub mod text;
pub mod theme;
pub mod tools;
pub mod widget;
pub mod width;
pub mod wizard;

pub use block::{Block, BlockId, Content, ContentHash, Coord, Slot, Stream, StreamWriter};
pub use frame::{Color, Frame, Line, Placed, Rect, Span, Style};
pub use host::{default_layout, Host, Presentation};
pub use keymap::{Action, Keymap, Keys};
pub use module::{Height, Modules, Mounted, Producer, View, ViewObject};
pub use moment::{Moment, Timestamp, Viewport};
pub use region::{Constraint, Dir, Region};
pub use surface::{Headless, Input, Key, KeyPress, Mods, Surface, Terminal};

/// Which language this crate's own tests assert in.
///
/// Every assertion in here was written against the Chinese wording, because
/// that is what the screen said when they were written — they mean "in Chinese,
/// this row reads …", which is a property worth keeping. So the test binary
/// says once, here, which language it is asserting in, rather than 900 tests
/// each restating it.
///
/// The other language is not left unchecked. `tests/language.rs` draws the same
/// surfaces in English and asserts they came out different, and the tables'
/// `match` is exhaustive, so a missing translation is a compile error rather
/// than a quiet fallback to the other language.
#[cfg(test)]
#[ctor::ctor]
fn _tests_assert_in_chinese() {
    crate::i18n::set_locale(crate::i18n::Locale::ZhCn);
}
