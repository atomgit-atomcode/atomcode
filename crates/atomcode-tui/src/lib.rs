//! A full-screen terminal UI, assembled from plugins.
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
pub mod block;
pub mod command;
pub mod commands;
pub mod conformance;
pub mod content;
pub mod frame;
pub mod host;
pub mod keymap;
pub mod layout;
pub mod layout_tool;
pub mod markdown;
pub mod module;
pub mod modules;
pub mod moment;
pub mod overlay;
pub mod plugin;
pub mod region;
pub mod surface;
pub mod width;

pub use block::{Block, BlockId, Content, ContentHash, Coord, Slot, Stream, StreamWriter};
pub use frame::{Color, Frame, Line, Placed, Rect, Span, Style};
pub use host::{default_layout, Host, Presentation};
pub use keymap::{Action, Keymap, Keys};
pub use module::{Height, Modules, Mounted, Producer, View, ViewObject};
pub use moment::{Moment, Timestamp, Viewport};
pub use region::{Constraint, Dir, Region};
pub use surface::{Headless, Input, Key, KeyPress, Mods, Surface, Terminal};
