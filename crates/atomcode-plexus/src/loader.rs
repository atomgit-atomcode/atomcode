//! The config tree: an ordered list of rows, and the layers that patch it.
//!
//! A running harness is not a hardcoded assembly — it is a list of rows, each
//! `{id, name, config}`, stacked from layers. A **bundle** contributes rows; a
//! **profile** stacks bundles and then the user's own patch. Every layer after
//! the first can address any earlier row by id and replace its config, disable
//! it, or insert new rows beside it.
//!
//! That is the property the whole design exists for: the model adapter, the
//! approval policy, even the agent loop are rows, so a user's patch file can
//! replace any of them without a fork and without the code that consumes them
//! knowing anything changed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{PlexusError, Result};

/// One mounted plugin instance.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    /// Stable address for patches. Defaults to `name` when a row omits it.
    pub id: String,
    /// The plugin to instantiate.
    pub name: String,
    /// Mounted but inert. Kept in the tree (rather than removed) so a later
    /// layer can re-enable it and so `--dump-config` shows what was suppressed.
    #[serde(default)]
    pub disabled: bool,
    /// Opaque to the runtime; the plugin's own schema.
    #[serde(default)]
    pub config: Value,
}

impl Entry {
    /// A row whose address is its plugin's name — the common case, and what
    /// TOML does when a row omits `id`.
    pub fn named(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            id: name.clone(),
            name,
            disabled: false,
            config: Value::Null,
        }
    }

    /// A second row of the same plugin, which needs its own address.
    pub fn with_id(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            disabled: false,
            config: Value::Null,
        }
    }

    /// The plugin's own config, from its own type rather than hand-built JSON.
    ///
    /// Fails for a type that has no JSON shape at all, such as a map with
    /// non-string keys. It does NOT validate values: `serde_json` maps a
    /// non-finite float to `null` rather than failing, so a `f32::NAN` arrives
    /// as an absent value instead of an error. Pinned by
    /// `a_non_finite_number_becomes_null_rather_than_an_error`; do not read this
    /// as validation.
    pub fn with(mut self, config: impl Serialize) -> Result<Self> {
        self.config = serde_json::to_value(config)
            .map_err(|e| PlexusError::Config(format!("row `{}`: {e}", self.id)))?;
        Ok(self)
    }

    /// Mounted but inert.
    pub fn disabled(mut self) -> Self {
        self.disabled = true;
        self
    }
}

/// One edit to the tree.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// Append rows.
    Insert(Vec<Entry>),
    /// Address a row by id and change what this op names.
    ///
    /// The three fields below are independent: whichever ones are `None` leave
    /// that aspect of the row alone.
    ///
    /// **`config` is REPLACED WHOLESALE, never deep-merged** (cordis semantics:
    /// a half-merged config is a config nobody can reason about). This is the
    /// part that surprises people, so it is worth being blunt: a patch that
    /// carries `config` and omits a key the row already had does NOT keep the
    /// old value — the key is gone, and the plugin reads its serde default.
    /// `--dump-config` then shows that default with no sign that anyone had
    /// chosen otherwise.
    ///
    /// Three real bugs came from reading this the other way. To change one key
    /// and keep the rest, carry the rest explicitly.
    /// `atomcode-coding`'s `no_row_silently_loses_a_configured_field` is a test
    /// of exactly this, and a good model for anyone patching from a host.
    Patch {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disabled: Option<bool>,
        /// Swap the implementation while keeping the row's address — how you
        /// replace a seam's provider from a patch file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Drop a row entirely.
    Remove { id: String },
}

/// An ordered set of edits — a bundle, or a user's patch file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Layer {
    #[serde(default)]
    pub ops: Vec<Op>,
}

impl Layer {
    /// An empty layer, to be built up with the methods below.
    ///
    /// The same edits [`from_toml`](Self::from_toml) parses, written in Rust —
    /// for a host that computes its layer rather than shipping a file.
    ///
    /// Nothing here changes what a layer MEANS; the criteria in
    /// `tests/layer_builder.rs` are the same edits written both ways with the
    /// trees compared. What it removes is the step where a host formats TOML
    /// into a string and hopes it parses: a value whose `Debug` is not TOML
    /// (`f32::NAN` prints `NaN`, a control character prints `\u{7f}`) makes the
    /// layer unparseable at a point far from whoever set it.
    ///
    /// It is not validation. See [`Entry::with`] for what a bad value does
    /// here instead.
    ///
    /// ```
    /// # use atomcode_plexus::{Entry, Layer};
    /// # #[derive(serde::Serialize)]
    /// # struct FsRow { root: String }
    /// # fn demo() -> atomcode_plexus::Result<Layer> {
    /// Ok(Layer::new()
    ///     .insert(Entry::named("fs-local").with(FsRow { root: "/tmp".into() })?)
    ///     .swap("llm", "llm-replay")
    ///     .disable("telemetry"))
    /// # }
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a row.
    pub fn insert(mut self, entry: Entry) -> Self {
        match self.ops.last_mut() {
            // Keep consecutive inserts in one op, the shape `from_toml` builds.
            Some(Op::Insert(entries)) => entries.push(entry),
            _ => self.ops.push(Op::Insert(vec![entry])),
        }
        self
    }

    /// Replace a row's config — **wholesale**, so carry every key you mean to
    /// keep. See [`Op::Patch`].
    pub fn patch(mut self, id: impl Into<String>, config: impl Serialize) -> Result<Self> {
        let id = id.into();
        let config = serde_json::to_value(config)
            .map_err(|e| PlexusError::Config(format!("patch `{id}`: {e}")))?;
        self.ops.push(Op::Patch {
            id,
            config: Some(config),
            disabled: None,
            name: None,
        });
        Ok(self)
    }

    /// Swap which plugin serves a row, keeping its address and config.
    pub fn swap(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.ops.push(Op::Patch {
            id: id.into(),
            config: None,
            disabled: None,
            name: Some(name.into()),
        });
        self
    }

    /// Turn a row off without removing it, so a later layer can turn it back on.
    pub fn disable(self, id: impl Into<String>) -> Self {
        self.set_disabled(id, true)
    }

    /// Turn a disabled row back on.
    pub fn enable(self, id: impl Into<String>) -> Self {
        self.set_disabled(id, false)
    }

    fn set_disabled(mut self, id: impl Into<String>, disabled: bool) -> Self {
        self.ops.push(Op::Patch {
            id: id.into(),
            config: None,
            disabled: Some(disabled),
            name: None,
        });
        self
    }

    /// Drop a row entirely.
    pub fn remove(mut self, id: impl Into<String>) -> Self {
        self.ops.push(Op::Remove { id: id.into() });
        self
    }

    /// Parse a layer from TOML.
    ///
    /// ```toml
    /// [[insert]]
    /// id = "llm-openai-compat"
    /// name = "llm-openai-compat"
    /// config = { model = "deepseek-chat" }
    ///
    /// [[patch]]
    /// id = "approval"
    /// config = { mode = "auto" }
    ///
    /// [[remove]]
    /// id = "telemetry"
    /// ```
    pub fn from_toml(src: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            insert: Vec<RawEntry>,
            #[serde(default)]
            patch: Vec<RawPatch>,
            #[serde(default)]
            remove: Vec<RawRemove>,
        }
        #[derive(Deserialize)]
        struct RawEntry {
            id: Option<String>,
            name: String,
            #[serde(default)]
            disabled: bool,
            config: Option<toml::Value>,
        }
        #[derive(Deserialize)]
        struct RawPatch {
            id: String,
            config: Option<toml::Value>,
            disabled: Option<bool>,
            name: Option<String>,
        }
        #[derive(Deserialize)]
        struct RawRemove {
            id: String,
        }

        let raw: Raw = toml::from_str(src).map_err(|e| PlexusError::Config(e.to_string()))?;
        let mut ops = Vec::new();
        if !raw.insert.is_empty() {
            let entries = raw
                .insert
                .into_iter()
                .map(|e| {
                    Ok(Entry {
                        id: e.id.unwrap_or_else(|| e.name.clone()),
                        name: e.name,
                        disabled: e.disabled,
                        config: e
                            .config
                            .map(to_json)
                            .transpose()?
                            .unwrap_or_else(|| Value::Object(Default::default())),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            ops.push(Op::Insert(entries));
        }
        for p in raw.patch {
            ops.push(Op::Patch {
                id: p.id,
                config: p.config.map(to_json).transpose()?,
                disabled: p.disabled,
                name: p.name,
            });
        }
        for r in raw.remove {
            ops.push(Op::Remove { id: r.id });
        }
        Ok(Self { ops })
    }
}

fn to_json(value: toml::Value) -> Result<Value> {
    // An empty table is the natural "no config" default and must stay an object,
    // not become null, so a plugin can always read `config["x"]` without a guard.
    serde_json::to_value(value).map_err(|e| PlexusError::Config(e.to_string()))
}

/// The resolved list of rows a runtime mounts.
#[derive(Clone, Debug, Default)]
pub struct ConfigTree {
    pub entries: Vec<Entry>,
}

impl ConfigTree {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Stack layers in order. Later layers win, per row.
    pub fn from_layers(layers: impl IntoIterator<Item = Layer>) -> Result<Self> {
        let mut tree = Self::empty();
        for layer in layers {
            tree.apply(&layer)?;
        }
        Ok(tree)
    }

    pub fn apply(&mut self, layer: &Layer) -> Result<()> {
        for op in &layer.ops {
            match op {
                Op::Insert(entries) => {
                    for entry in entries {
                        if let Some(existing) = self.entries.iter_mut().find(|e| e.id == entry.id) {
                            // Re-inserting an id is how a later bundle restates a
                            // row it owns; treat it as a full replacement.
                            *existing = entry.clone();
                        } else {
                            self.entries.push(entry.clone());
                        }
                    }
                }
                Op::Patch {
                    id,
                    config,
                    disabled,
                    name,
                } => {
                    let Some(row) = self.entries.iter_mut().find(|e| &e.id == id) else {
                        return Err(PlexusError::Config(format!(
                            "patch targets row `{id}`, which no earlier layer inserted"
                        )));
                    };
                    if let Some(config) = config {
                        row.config = config.clone();
                    }
                    if let Some(disabled) = disabled {
                        row.disabled = *disabled;
                    }
                    if let Some(name) = name {
                        row.name = name.clone();
                    }
                }
                Op::Remove { id } => {
                    self.entries.retain(|e| &e.id != id);
                }
            }
        }
        Ok(())
    }

    pub fn active(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| !e.disabled)
    }

    /// `--dump-config`: the tree as it will actually be mounted. Every line here
    /// is a row a user's own patch can address.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for entry in &self.entries {
            let mark = if entry.disabled { " (disabled)" } else { "" };
            let plugin = if entry.id == entry.name {
                String::new()
            } else {
                format!("  <- {}", entry.name)
            };
            out.push_str(&format!("- {}{}{}\n", entry.id, mark, plugin));
            if !matches!(&entry.config, Value::Null)
                && entry.config.as_object().is_none_or(|o| !o.is_empty())
            {
                let rendered = serde_json::to_string_pretty(&entry.config)
                    .unwrap_or_else(|_| entry.config.to_string());
                for line in rendered.lines() {
                    out.push_str("    ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out
    }
}
