//! Transitional re-export of gateway signing primitives now owned by `atomcode-auth`.
//!
//! Core provider utilities keep this compatibility path; CodingRuntime depends on
//! `atomcode-auth` or the capabilities provider adapter directly.

pub use atomcode_auth::gateway_crypto::*;
