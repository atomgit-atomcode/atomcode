//! Opaque host invocation identity for negotiated resource leases. Never taken
//! from model tool arguments and never a session persistence authority.
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpScope(String);
impl McpScope {
    /// The host supplies its real execution identity and authoritative epoch.
    /// Length framing and hashing keep metadata bounded without exposing either.
    pub fn from_identity(identity: &str, epoch: u64) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"atomcode-mcp-scope-v1\0");
        digest.update((identity.len() as u64).to_be_bytes());
        digest.update(identity.as_bytes());
        digest.update(epoch.to_be_bytes());
        Self(format!("atomcode-{:x}", digest.finalize()))
    }
    pub fn owner(&self) -> &str {
        &self.0
    }
}

#[derive(Default)]
pub(super) struct ScopeState {
    pub owner: Option<McpScope>,
    pub attempted: Option<McpScope>,
}
