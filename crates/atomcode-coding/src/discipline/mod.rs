//! Coding-specific *discipline*: the behaviours that make this a CODING agent
//! rather than a generic tool-runner.
//!
//! One ships: edit-then-verify self-correction. The judgement lives here, apart
//! from the shape it is delivered in — the `verify-cadence` row in
//! [`crate::on_harness`] asks [`unverified_edit`] and sends [`NUDGE`].

mod verify;

pub use verify::{unverified_edit, NudgedEdit, NUDGE};
