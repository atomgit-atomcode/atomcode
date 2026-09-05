//! Profiles: named assemblies, and the layer order that resolves them.
//!
//! A **bundle** contributes rows. A **profile** names an ordered list of bundles
//! plus a patch of its own. Resolution stacks them in one fixed order, and every
//! layer can address any row an earlier layer inserted:
//!
//! ```text
//! bundles, in the order the profile lists them
//!   -> the profile's own patch
//!     -> the home patch ($ATOMCODE_HOME/harness.patch.toml)
//!       -> any --patch overlay, in the order given
//! ```
//!
//! The user's layer comes last on purpose. Everything a profile decides — which
//! front end, which approval policy, which execution world — is something the
//! person running it can overrule without forking anything, which is the whole
//! claim of a config-tree architecture.
//!
//! Profiles are data, so a deployment adds one by dropping a file in
//! `$ATOMCODE_HOME/profiles/<name>.toml` rather than by changing this crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use atomcode_plexus::{ConfigTree, Layer, PlexusError, Result};
use serde::Deserialize;

/// One named assembly.
#[derive(Clone, Debug, Deserialize)]
pub struct Profile {
    #[serde(skip)]
    pub name: String,
    /// Bundles to stack, in order.
    pub bundles: Vec<String>,
    /// The profile's own patch layer, as TOML.
    #[serde(default)]
    pub patch: Option<String>,
    /// One line for `--list-profiles`.
    #[serde(default)]
    pub description: String,
}

/// The catalog of bundles and profiles this build knows, plus whatever the
/// harness home adds.
pub struct Profiles {
    bundles: BTreeMap<String, String>,
    profiles: BTreeMap<String, Profile>,
    home: PathBuf,
}

impl Profiles {
    /// The shipped catalog, before anything on disk is consulted.
    pub fn builtin() -> Self {
        let mut bundles = BTreeMap::new();
        for (name, source) in crate::bundle::BUNDLES {
            bundles.insert((*name).to_string(), (*source).to_string());
        }
        let mut profiles = BTreeMap::new();
        for (name, bundles_list, patch, description) in crate::bundle::PROFILES {
            profiles.insert(
                (*name).to_string(),
                Profile {
                    name: (*name).to_string(),
                    bundles: bundles_list.iter().map(|b| (*b).to_string()).collect(),
                    patch: patch.map(|p| p.to_string()),
                    description: (*description).to_string(),
                },
            );
        }
        Self {
            bundles,
            profiles,
            home: crate::home(),
        }
    }

    /// Point at a different harness home. For a test, or an embedder keeping
    /// its profiles somewhere other than `$ATOMCODE_HOME`.
    pub fn rooted_at(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = home.into();
        self
    }

    /// Load profiles from `<home>/profiles/*.toml` on top of the shipped ones.
    /// A file may redefine a shipped name — that is how a deployment changes
    /// what `--profile repl` means without touching this crate.
    pub fn with_home(mut self) -> Self {
        let dir = self.home.join("profiles");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return self;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "toml") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match toml::from_str::<Profile>(&text) {
                Ok(mut profile) => {
                    profile.name = name.to_string();
                    self.profiles.insert(name.to_string(), profile);
                }
                // A malformed profile is reported and skipped rather than
                // failing every other profile in the directory.
                Err(e) => eprintln!("profile `{name}` is malformed: {e}"),
            }
        }
        self
    }

    pub fn names(&self) -> Vec<&str> {
        self.profiles.keys().map(String::as_str).collect()
    }

    pub fn get(&self, name: &str) -> Option<&Profile> {
        self.profiles.get(name)
    }

    pub fn bundle_names(&self) -> Vec<&str> {
        self.bundles.keys().map(String::as_str).collect()
    }

    /// The home patch, applied after the profile's own and before any overlay.
    pub fn home_patch_path(&self) -> PathBuf {
        self.home.join("harness.patch.toml")
    }

    /// Resolve `name` into a mountable tree, with `overlays` applied last.
    pub fn resolve(&self, name: &str, overlays: &[&str]) -> Result<ConfigTree> {
        let profile = self.profiles.get(name).ok_or_else(|| {
            PlexusError::Config(format!(
                "unknown profile `{name}`; known: {}",
                self.names().join(", ")
            ))
        })?;

        let mut layers = Vec::new();
        for bundle in &profile.bundles {
            let source = self.bundles.get(bundle).ok_or_else(|| {
                PlexusError::Config(format!(
                    "profile `{name}` lists bundle `{bundle}`, which does not exist; known: {}",
                    self.bundle_names().join(", ")
                ))
            })?;
            layers.push(Layer::from_toml(source)?);
        }
        if let Some(patch) = &profile.patch {
            layers.push(Layer::from_toml(patch)?);
        }
        if let Some(home) = read_optional(&self.home_patch_path())? {
            layers.push(Layer::from_toml(&home)?);
        }
        for overlay in overlays {
            layers.push(Layer::from_toml(overlay)?);
        }
        ConfigTree::from_layers(layers)
    }

    /// How a profile would be assembled, for `--dump-config` and diagnostics.
    pub fn explain(&self, name: &str) -> String {
        let Some(profile) = self.profiles.get(name) else {
            return format!("unknown profile `{name}`");
        };
        let mut out = format!("profile `{name}` — {}\n", profile.description);
        out.push_str("layers, in order:\n");
        for bundle in &profile.bundles {
            out.push_str(&format!("  1. bundle `{bundle}`\n"));
        }
        if profile.patch.is_some() {
            out.push_str("  2. the profile's own patch\n");
        }
        let home = self.home_patch_path();
        out.push_str(&format!(
            "  3. {} {}\n",
            home.display(),
            if home.exists() {
                "(present)"
            } else {
                "(absent)"
            }
        ));
        out.push_str("  4. any --patch overlay, in the order given\n");
        out
    }
}

impl Default for Profiles {
    fn default() -> Self {
        Self::builtin()
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(PlexusError::Config(format!("{}: {e}", path.display()))),
    }
}
