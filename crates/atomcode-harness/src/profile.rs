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
    /// A layer that addresses rows an earlier layer already inserted, applied
    /// after the named bundles and before this profile's own patch.
    ///
    /// It exists because `Op::Insert` REPLACES a row with the same id, so a
    /// layer that restates rows a bundle owns silently reverts whatever the
    /// layers below did to them. A profile that is a product's edit of a
    /// shipped assembly therefore states that edit here — where the user's home
    /// patch, this profile's patch and every `--patch` overlay still come after
    /// it — rather than restating rows in a bundle stacked ahead of them.
    #[serde(default)]
    pub adjust: Option<String>,
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
        // `base` was one bundle before it was split into the machine and the
        // product decisions. A profile on disk that names it is a deployment's
        // file, not ours, so the name keeps meaning what it meant: both halves,
        // in the order `bundle::base()` composes them.
        bundles.insert(
            "base".to_string(),
            format!("{}\n{}", crate::bundle::INFRA, crate::bundle::DEFAULTS),
        );
        let mut profiles = BTreeMap::new();
        for (name, bundles_list, patch, description) in crate::bundle::PROFILES {
            profiles.insert(
                (*name).to_string(),
                Profile {
                    name: (*name).to_string(),
                    bundles: bundles_list.iter().map(|b| (*b).to_string()).collect(),
                    patch: patch.map(|p| p.to_string()),
                    adjust: None,
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

    /// Add bundles this build does not ship — the catalog another crate's
    /// product contributes, so its profiles (and a user's) can name them.
    ///
    /// Owned strings rather than `&'static str`, because a product's bundle is
    /// usually composed from parts that only exist at runtime (`rows::SCREEN`
    /// beside a few edits) and a duplicated fact diverges.
    pub fn with_bundles(mut self, bundles: &[(String, String)]) -> Self {
        for (name, source) in bundles {
            self.bundles.insert(name.clone(), source.clone());
        }
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

    /// Register a profile this build does not ship.
    ///
    /// Same act as dropping a file in `<home>/profiles/`, minus the file: it is
    /// how a product ships an assembly that names its own bundles without
    /// teaching this crate what a product is. A shipped or on-disk name given
    /// here is overridden, which is deliberate — the caller registering it is
    /// closer to the person than the shipped catalog is.
    pub fn with_profile(
        mut self,
        name: &str,
        bundles: Vec<String>,
        adjust: Option<String>,
        description: &str,
    ) -> Self {
        self.profiles.insert(
            name.to_string(),
            Profile {
                name: name.to_string(),
                bundles,
                patch: None,
                adjust,
                description: description.to_string(),
            },
        );
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
        if let Some(adjust) = &profile.adjust {
            layers.push(Layer::from_toml(adjust)?);
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
        out.push_str(&format!(
            "  2. this profile's own edits{}\n",
            if profile.adjust.is_some() {
                ""
            } else {
                " (none)"
            }
        ));
        if profile.patch.is_some() {
            out.push_str("  3. the profile's own patch\n");
        }
        let home = self.home_patch_path();
        out.push_str(&format!(
            "  4. {} {}\n",
            home.display(),
            if home.exists() {
                "(present)"
            } else {
                "(absent)"
            }
        ));
        out.push_str("  5. any --patch overlay, in the order given\n");
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
