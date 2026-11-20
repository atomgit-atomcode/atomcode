//! What a scan found: one row per asset, each carrying a status.
//!
//! Four statuses, not two. "Compatible" and "needs conversion" cannot say the
//! one thing a person most needs to know before deciding: whether the asset
//! can come over at all. A lost session history and a skill directory that
//! only needs copying are both "not native", but only one of them is
//! recoverable.

use serde::Serialize;
use std::path::PathBuf;

/// What kind of asset a row is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetKind {
    Instructions,
    Skills,
    Commands,
    McpServers,
    Permissions,
    Hooks,
    ProviderConfig,
    Agents,
    Sessions,
    Plugins,
}

/// Where an asset stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetStatus {
    /// AtomCode reads this exact path today. Nothing to do.
    Active,
    /// Its equivalent is already in OUR configuration — a previous import
    /// brought it over. This is the re-check half of the loop: without it,
    /// a second run would say "can be brought over" about something that is
    /// already there.
    Imported,
    /// There is a known way to bring it over.
    Actionable,
    /// It cannot come over (lossy, or this release does not do it).
    Blocked,
}

/// Which level of the tree an asset lives at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AssetScope {
    /// Under the person's home directory.
    User,
    /// Under the current project.
    Project,
}

/// One asset the scan found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetReport {
    pub kind: AssetKind,
    /// The asset's own name: a skill name, `settings.json`, a session file.
    pub name: String,
    /// Where it is on disk now.
    pub path: PathBuf,
    pub scope: AssetScope,
    pub status: AssetStatus,
    /// One sentence for a person: why this status, and what to do about it.
    ///
    /// Localized here so both front ends show the same words — see the design
    /// document's "文案只出一处". When the asset can be brought over, this
    /// sentence **names the exact path to copy it to**, so "copy it to the
    /// user tree" is not a thing the person has to work out.
    pub detail: String,
}

/// Everything one external agent had.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceReport {
    pub source_id: &'static str,
    pub display_name: &'static str,
    pub assets: Vec<AssetReport>,
}

/// Everything every external agent had.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct MigrationReport {
    pub sources: Vec<SourceReport>,
}

impl MigrationReport {
    /// How many assets the whole scan found.
    pub fn total_assets(&self) -> usize {
        self.sources.iter().map(|s| s.assets.len()).sum()
    }

    /// How many assets are in one status.
    pub fn assets_with_status(&self, status: AssetStatus) -> usize {
        self.sources
            .iter()
            .flat_map(|s| s.assets.iter())
            .filter(|a| a.status == status)
            .count()
    }
}
