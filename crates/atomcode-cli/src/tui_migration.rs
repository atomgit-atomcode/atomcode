//! The host side of the migration port: it scans the disk and hands the screen
//! a view it can lay out.

use async_trait::async_trait;
use atomcode_tui::migration::{
    AssetStatusView, Migration, MigrationAssetView, MigrationReportView, MigrationSourceView,
};
use std::path::{Path, PathBuf};

pub struct DiskMigration {
    home: PathBuf,
    product: atomcode_capabilities::ProductDirs,
    config_path: PathBuf,
}

impl DiskMigration {
    pub fn new(
        home: PathBuf,
        product: atomcode_capabilities::ProductDirs,
        config_path: PathBuf,
    ) -> Self {
        Self {
            home,
            product,
            config_path,
        }
    }
}

#[async_trait]
impl Migration for DiskMigration {
    async fn report(&self, project: &Path) -> Result<MigrationReportView, String> {
        let ctx = crate::import_cmd::scan_context(
            &self.home,
            project,
            self.product.clone(),
            &self.config_path,
        );
        let sources = atomcode_capabilities::migration::builtin_sources();
        let (mut report, drafts) =
            atomcode_capabilities::migration::scan_with_provider_drafts(&sources, &ctx);
        crate::import_cmd::mark_already_imported(&mut report, &drafts, &self.config_path);
        Ok(MigrationReportView {
            sources: report
                .sources
                .into_iter()
                .map(|source| MigrationSourceView {
                    source_id: source.source_id.to_string(),
                    display_name: source.display_name.to_string(),
                    assets: source
                        .assets
                        .into_iter()
                        .map(|asset| MigrationAssetView {
                            name: asset.name,
                            status: match asset.status {
                                atomcode_capabilities::migration::AssetStatus::Active => {
                                    AssetStatusView::Active
                                }
                                atomcode_capabilities::migration::AssetStatus::Imported => {
                                    AssetStatusView::Imported
                                }
                                atomcode_capabilities::migration::AssetStatus::Actionable => {
                                    AssetStatusView::Actionable
                                }
                                atomcode_capabilities::migration::AssetStatus::Blocked => {
                                    AssetStatusView::Blocked
                                }
                            },
                            detail: asset.detail,
                        })
                        .collect(),
                })
                .collect(),
        })
    }
}
