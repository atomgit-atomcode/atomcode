//! Where the product keeps its data — handed in, never looked up.
//!
//! Every capability that persists (skills, plugins, memory, MCP config, sessions,
//! setup state, the credential guard) needs two things: the user-level tree
//! (`~/.atomcode`, or wherever the person moved it) and, per project, the dir
//! under the working directory (`<project>/.atomcode`). This crate used to spell
//! both itself and read `$ATOMCODE_HOME` from the environment wherever it
//! needed one. Two downstreams paid for that:
//!
//! - a fork renaming its tree edited ~240 files, and re-edits them in the merge
//!   commit every time it takes upstream;
//! - a program importing these crates could only relocate them by setting
//!   `ATOMCODE_HOME` before every entry point — and the entry point that forgot
//!   read its login from the wrong tree and told a logged-in person they were not.
//!
//! So the name is the host's: it builds one `ProductDirs` (for this product,
//! `ProductDirs::new(Config::config_dir(), distribution::PROJECT_DIR_NAME)`) and
//! passes it down. Nothing in this crate spells the product directory
//! (`gates/product-dir.sh` holds it to that). Forgetting to pass it is a compile
//! error rather than a silent read from somewhere else.

use std::path::{Path, PathBuf};

/// The user-level tree and the per-project dir name. Cheap to clone; hand it to
/// whatever persists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProductDirs {
    user: PathBuf,
    project_dir_name: String,
    home_dir_name: String,
    /// Where sessions live, when the host wants them somewhere other than
    /// `<user>/sessions`.
    ///
    /// The default is right for a desktop. Android points sessions at a directory
    /// an externally started daemon can also read, so switching to it keeps one
    /// history — while `config.toml` and its keys stay in the app's private tree.
    ///
    /// This used to be a process-global `OnceLock` in the session module, which let
    /// one host's setting move the sessions root of every runtime in the same
    /// process. It belongs to the dirs the host hands out, like every other path.
    sessions_root: Option<PathBuf>,
}

impl ProductDirs {
    /// `user` is the tree itself (not the home it sits in); `project_dir_name`
    /// is the dir created under each working directory.
    pub fn new(user: impl Into<PathBuf>, project_dir_name: impl Into<String>) -> Self {
        let project_dir_name = project_dir_name.into();
        Self {
            user: user.into(),
            home_dir_name: project_dir_name.clone(),
            project_dir_name,
            sessions_root: None,
        }
    }

    /// Pin where sessions live. `None` keeps `<user>/sessions`.
    pub fn with_sessions_root(mut self, root: Option<PathBuf>) -> Self {
        self.sessions_root = root;
        self
    }

    /// Where session history lives: the host's choice, or `<user>/sessions`.
    pub fn sessions_root(&self) -> PathBuf {
        self.sessions_root
            .clone()
            .unwrap_or_else(|| self.user.join("sessions"))
    }

    /// The user tree's default name under a person's home, when it differs from
    /// the project dir's name (by default they are the same). Only matched on:
    /// it is how a model spells the tree from habit (`~/<name>/auth.toml`), which
    /// the credential guard must still catch wherever the tree actually is.
    pub fn with_home_dir_name(mut self, name: impl Into<String>) -> Self {
        self.home_dir_name = name.into();
        self
    }

    /// The user-level tree.
    pub fn user(&self) -> &Path {
        &self.user
    }

    /// The per-project dir under `root`.
    pub fn project(&self, root: &Path) -> PathBuf {
        root.join(&self.project_dir_name)
    }

    /// The per-project dir's bare name, for the places that match on it rather
    /// than join it: walk skip lists, `.gitignore` lines.
    pub fn project_dir_name(&self) -> &str {
        &self.project_dir_name
    }

    /// See [`Self::with_home_dir_name`].
    pub fn home_dir_name(&self) -> &str {
        &self.home_dir_name
    }
}

/// For this crate's own unit tests: a user tree in a per-process temp dir and a
/// project dir named `.ours` — deliberately not our real name, so a test that
/// still spells `.atomcode` somewhere fails instead of passing by coincidence.
#[cfg(test)]
pub(crate) fn test_dirs() -> ProductDirs {
    static TREE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let tree = TREE.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "product-dirs-test-tree-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("create the test user tree");
        dir
    });
    ProductDirs::new(tree.clone(), ".ours")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_dir_is_the_given_name_under_the_root() {
        let dirs = ProductDirs::new("/elsewhere/tree", ".fork");
        assert_eq!(dirs.user(), Path::new("/elsewhere/tree"));
        assert_eq!(dirs.project(Path::new("/w")), PathBuf::from("/w/.fork"));
        assert_eq!(dirs.project_dir_name(), ".fork");
    }
}
