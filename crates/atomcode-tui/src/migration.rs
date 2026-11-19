//! The migration port: what the screen asks the host about another agent's
//! assets.
//!
//! The screen owns the list; the host owns the disk. Same shape as
//! [`crate::setup`] — the screen asks, the host answers with a view type the
//! screen can lay out.

use async_trait::async_trait;

/// What the host found, in the shape the screen draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReportView {
    pub sources: Vec<MigrationSourceView>,
}

/// One external agent's assets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationSourceView {
    pub source_id: String,
    pub display_name: String,
    pub assets: Vec<MigrationAssetView>,
}

/// One asset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationAssetView {
    pub name: String,
    /// The status, as the four-state vocabulary. The screen renders it with
    /// the SAME shared product message the CLI uses, so the two cannot say
    /// the same state two ways.
    pub status: AssetStatusView,
    pub detail: String,
}

/// The status of one asset, as the screen sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetStatusView {
    Active,
    Imported,
    Actionable,
    Blocked,
}

/// The word a status shows as.
///
/// The **product** table, not the screen table: the CLI says the same state
/// with the same words, and one thing must not be said two ways.
///
/// `pub(crate)` because `commands.rs` — a sibling module — calls it.
pub(crate) fn status_word(status: AssetStatusView) -> String {
    use crate::i18n::product::{t, Msg};
    match status {
        AssetStatusView::Active => t(Msg::MigrationStatusActive).into_owned(),
        AssetStatusView::Imported => t(Msg::MigrationStatusImported).into_owned(),
        AssetStatusView::Actionable => t(Msg::MigrationStatusActionable).into_owned(),
        AssetStatusView::Blocked => t(Msg::MigrationStatusBlocked).into_owned(),
    }
}

/// Read the assets another agent left behind.
#[async_trait]
pub trait Migration: Send + Sync {
    /// The report. Read-only.
    async fn report(&self) -> Result<MigrationReportView, String>;
}
