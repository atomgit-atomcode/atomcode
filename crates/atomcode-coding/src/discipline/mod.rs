//! Coding-specific *discipline*: the behaviors that make this a CODING agent rather
//! than a generic tool-runner. Each is layered onto the neutral kernel loop through a
//! [`LifecycleHooks`](atomcode_kernel::hook::LifecycleHooks) seam — no kernel change.
//!
//! MVP ships one: [`VerifyCadenceHook`] (edit-then-verify self-correction). Followons
//! (auto-diagnosis, build-failure streak tracking, etc.) land as additional hooks.

mod verify;

pub use verify::VerifyCadenceHook;
/// The judgement, separate from the shape it is delivered in.
///
/// `VerifyCadenceHook` is one shape (the kernel's `offer_continuation`); the
/// `verify-cadence` row in [`crate::on_harness`] is the other (`agent/request`
/// plus the inbox). Both ask [`unverified_edit`] the same question and send the
/// same [`NUDGE`], so the discipline cannot drift between the two assemblies.
pub use verify::{unverified_edit, NudgedEdit, NUDGE};
