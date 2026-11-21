//! `product-dirs`: where the product keeps its data, said once for the tree.
//!
//! Every row that persists or guards something — skills, memory, MCP, sessions,
//! plugins, the credential gates, the walkers that skip our own project dir —
//! reads the same [`ProductDirs`] from this row's seam. They used to resolve
//! `$ATOMCODE_HOME` each for itself, with a literal `.atomcode` fallback, so a
//! program embedding this harness could only relocate them by exporting the
//! variable before every entry point; the one entry point that forgot read its
//! login from the wrong tree and told a logged-in person they were not.
//!
//! Now a row that needs the dirs and does not find them fails to mount
//! ([`crate::product_dirs`]), the same way a row that needs the model does
//! without the `llm` seam — never a quiet read from somewhere else.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::ProductDirs;
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

use crate::seams::ProductDirsSvc;

#[derive(Debug, Deserialize, Default)]
struct ProductDirsRow {
    /// The user-level tree. Absent: this build's default (`$ATOMCODE_HOME`,
    /// else `~/<distribution::HOME_DIR_NAME>`) — the harness launcher acting as
    /// the host. A program that embeds the harness writes its own.
    #[serde(default)]
    user: Option<String>,
    /// The per-project dir's name. Absent: `distribution::PROJECT_DIR_NAME`.
    #[serde(default)]
    project_dir_name: Option<String>,
    /// The tree's default name under a home, when it differs from the project
    /// dir's. Absent: `distribution::HOME_DIR_NAME`.
    #[serde(default)]
    home_dir_name: Option<String>,
    /// Where session history lives, when the host wants it somewhere other than
    /// `<user>/sessions`. Absent: that default.
    ///
    /// This is a *choice*, not a resolved path, so it has to survive the trip
    /// through this row: a host that points sessions at a shared directory (the
    /// Android build does, so an externally started daemon and the app read one
    /// history) would otherwise have the rows fall back to `<user>/sessions`
    /// and the catalogue and the session contents would live in two trees.
    #[serde(default)]
    sessions_root: Option<String>,
}

/// The `product-dirs` row. First in the tree: every row that reads its seam
/// comes after it.
pub struct ProductDirsPlugin;

#[async_trait]
impl Plugin for ProductDirsPlugin {
    fn name(&self) -> &'static str {
        "product-dirs"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["product-dirs"]
    }
    fn description(&self) -> &'static str {
        "where the product keeps its data: the user tree and the per-project dir"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ProductDirsRow = if config.is_null() {
            ProductDirsRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let dirs = resolve(row);
        let _ = ctx
            .provide::<ProductDirsSvc>(Arc::new(dirs))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn resolve(row: ProductDirsRow) -> ProductDirs {
    use atomcode_config::distribution::{HOME_DIR_NAME, PROJECT_DIR_NAME};
    let user = row
        .user
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(crate::home);
    ProductDirs::new(
        user,
        row.project_dir_name
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| PROJECT_DIR_NAME.to_string()),
    )
    .with_home_dir_name(
        row.home_dir_name
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| HOME_DIR_NAME.to_string()),
    )
    .with_sessions_root(
        row.sessions_root
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_row_says_is_what_every_row_gets() {
        let dirs = resolve(ProductDirsRow {
            user: Some("/elsewhere/fork-tree".into()),
            project_dir_name: Some(".fork".into()),
            home_dir_name: Some(".fork".into()),
            sessions_root: None,
        });
        assert_eq!(dirs.user(), std::path::Path::new("/elsewhere/fork-tree"));
        assert_eq!(dirs.project_dir_name(), ".fork");
        assert_eq!(dirs.home_dir_name(), ".fork");
    }

    /// The host's choice about where sessions live has to survive this row.
    ///
    /// It used to be dropped here: the row carried only the user tree, so every
    /// row that reads the dirs fell back to `<user>/sessions` while the daemon
    /// (which resolves the dirs from the environment) used the host's path. The
    /// catalogue and the session contents then lived in two trees — the exact
    /// split this row exists to prevent.
    #[test]
    fn the_hosts_choice_about_sessions_survives_the_row() {
        let shared = "/storage/emulated/0/AtomCode/.sessions";
        let dirs = resolve(ProductDirsRow {
            user: Some("/data/user/0/com.atomcode.app/files/atomcode".into()),
            project_dir_name: None,
            home_dir_name: None,
            sessions_root: Some(shared.into()),
        });
        assert_eq!(
            dirs.sessions_root(),
            std::path::PathBuf::from(shared),
            "the row's choice is the sessions root"
        );
        assert_eq!(
            dirs.sessions_root_override(),
            Some(std::path::Path::new(shared))
        );

        // No choice: the default stands, and the override stays `None` so a
        // component can tell "not chosen" from "chosen to be the default".
        let plain = resolve(ProductDirsRow {
            user: Some("/data/user/0/com.atomcode.app/files/atomcode".into()),
            project_dir_name: None,
            home_dir_name: None,
            sessions_root: None,
        });
        assert_eq!(
            plain.sessions_root(),
            std::path::PathBuf::from("/data/user/0/com.atomcode.app/files/atomcode/sessions")
        );
        assert!(plain.sessions_root_override().is_none());
    }
}
