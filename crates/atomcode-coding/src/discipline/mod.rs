//! Coding-specific *discipline*: the behaviors that make this a CODING agent rather
//! than a generic tool-runner. Each is layered onto the neutral kernel loop through a
//! [`LifecycleHooks`](atomcode_kernel::hook::LifecycleHooks) seam — no kernel change.
//!
//! MVP ships one: [`VerifyCadenceHook`] (edit-then-verify self-correction). Followons
//! (auto-diagnosis, build-failure streak tracking, etc.) land as additional hooks.

mod verify;

pub use verify::VerifyCadenceHook;
