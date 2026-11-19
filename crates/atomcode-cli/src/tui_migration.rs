//! The host side of the migration port: it scans the disk and hands the screen
//! a view it can lay out.

use async_trait::async_trait;
use atomcode_tui::migration::{
    AssetStatusView, Migration, MigrationAssetView, MigrationReportView, MigrationSourceView,
};
use std::path::PathBuf;

pub struct DiskMigration {
    home: PathBuf,
    project: PathBuf,
    product: atomcode_capabilities::ProductDirs,
    /// The skill roots the RUNTIME uses, resolved by the host.
    ///
    /// **The host must supply them**: the screen cannot resolve them itself —
    /// it may not depend on `atomcode-coding` (the layering gate forbids it),
    /// and the resolution rule lives there. `None` means "the host did not
    /// say" and the standard list is used; `Some(dirs)` means exactly these.
    /// An empty list would fall back to the standard list, and then the
    /// screen would disagree with the CLI about which same-named skill wins.
    skill_dirs: Option<Vec<PathBuf>>,
    /// Where OUR configuration lives — the screen's re-check compares against
    /// it, so an account that was imported shows as "already imported" in the
    /// screen too, not just in the CLI.
    config_path: PathBuf,
}

impl DiskMigration {
    pub fn new(
        home: PathBuf,
        project: PathBuf,
        product: atomcode_capabilities::ProductDirs,
        config_path: PathBuf,
        skill_dirs: Option<Vec<PathBuf>>,
    ) -> Self {
        Self {
            home,
            project,
            product,
            config_path,
            skill_dirs,
        }
    }
}

#[async_trait]
impl Migration for DiskMigration {
    async fn report(&self) -> Result<MigrationReportView, String> {
        let ctx = atomcode_capabilities::migration::ScanContext {
            os_home: self.home.clone(),
            project: self.project.clone(),
            product: self.product.clone(),
            skill_dirs: self.skill_dirs.clone(),
            opencode_config_dir: PathBuf::new(),
            opencode_data_dir: PathBuf::new(),
        };
        let sources = atomcode_capabilities::migration::builtin_sources();
        let mut report = atomcode_capabilities::migration::scan(&sources, &ctx);
        // The same re-check the CLI does — the screen must not say "can be
        // brought over" about an account that is already in the config.
        let drafts: Vec<_> = sources
            .iter()
            .flat_map(|source| source.provider_drafts(&ctx))
            .collect();
        // `crate::` — this file IS the `atomcode` library, so `atomcode::`
        // does not resolve here.
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
