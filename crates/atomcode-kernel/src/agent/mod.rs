//! The agent: the turn engine, and — beside it, in files of their own — the
//! contract a front end speaks to an agent through.
//!
//! `engine` is what runs turns (`Agent`, `AgentBuilder`, `AgentHandle`, …). It is
//! re-exported whole, so `atomcode_kernel::agent::Agent` keeps its path. The
//! contract types live in sibling files so a reader can tell, by file, what is
//! protocol and what is implementation (`docs/adr/0021` §6).

mod engine;

pub use engine::*;
