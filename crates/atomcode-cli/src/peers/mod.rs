//! Same-user online peers. Transport and admission belong to the CLI host;
//! accepted text enters the existing runtime as a peer note, never a user command.
#[cfg(unix)]
mod local;
#[cfg(unix)]
mod tools;
mod tui;
#[cfg(unix)]
pub(crate) use local::bind;
#[cfg(unix)]
pub use local::install;
pub use tui::{row_layer, PeerRow};
