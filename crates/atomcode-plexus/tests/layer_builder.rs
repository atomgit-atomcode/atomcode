//! The Rust way of writing a layer means the same thing as the TOML way.
//!
//! That is the whole contract of the builder: it is a second spelling, not a
//! second semantics. Every criterion here is the same edit written twice, with
//! the trees compared — because the thing worth guaranteeing is not that the
//! methods run, it is that a host which stops formatting TOML strings does not
//! quietly get a different tree.

use atomcode_plexus::{ConfigTree, Entry, Layer};

/// The mounted rows, each with its plugin and its config as a VALUE.
///
/// Not the `dump()` string, and the reason is worth keeping: whether a config's
/// keys come out sorted or in struct order depends on whether anything in the
/// build turned on `serde_json/preserve_order` — and Cargo unifies features
/// across a workspace, so that depends on which OTHER crates are in the same
/// `cargo nextest` invocation. Comparing rendered text made these criteria pass
/// alone and fail next to `atomcode`. Comparing values compares what the rows
/// actually receive, which is the thing this file is about.
fn tree(layers: impl IntoIterator<Item = Layer>) -> Vec<(String, String, serde_json::Value)> {
    ConfigTree::from_layers(layers)
        .expect("stacks")
        .active()
        .map(|e| (e.id.clone(), e.name.clone(), e.config.clone()))
        .collect()
}

#[derive(serde::Serialize)]
struct FsRow {
    root: String,
    read_only: bool,
}

/// An insert written both ways lands the same row, including `id` defaulting to
/// the plugin's name — the rule TOML applies when a row omits `id`.
#[test]
fn an_insert_means_the_same_written_either_way() {
    let from_toml = Layer::from_toml(
        r#"
[[insert]]
name = "fs-local"
config = { root = "/tmp", read_only = true }
"#,
    )
    .unwrap();
    let built = Layer::new().insert(
        Entry::named("fs-local")
            .with(FsRow {
                root: "/tmp".into(),
                read_only: true,
            })
            .unwrap(),
    );
    assert_eq!(tree([from_toml]), tree([built]));
}

/// …and so do patch, swap, disable and remove, stacked on the same base.
#[test]
fn every_edit_means_the_same_written_either_way() {
    let base = || {
        Layer::from_toml(
            r#"
[[insert]]
name = "fs-local"
config = { root = "/a", read_only = false }

[[insert]]
name = "llm"

[[insert]]
name = "telemetry"

[[insert]]
name = "trace"
"#,
        )
        .unwrap()
    };
    let from_toml = Layer::from_toml(
        r#"
[[patch]]
id = "fs-local"
config = { root = "/b", read_only = true }

[[patch]]
id = "llm"
name = "llm-replay"

[[patch]]
id = "trace"
disabled = true

[[remove]]
id = "telemetry"
"#,
    )
    .unwrap();
    let built = Layer::new()
        .patch(
            "fs-local",
            FsRow {
                root: "/b".into(),
                read_only: true,
            },
        )
        .unwrap()
        .swap("llm", "llm-replay")
        .disable("trace")
        .remove("telemetry");
    assert_eq!(tree([base(), from_toml]), tree([base(), built]));
}

/// A non-finite float becomes `null` here rather than an error — pinned because
/// it is surprising, and because it is the one place this path is WEAKER than
/// formatting TOML by hand.
///
/// `f32::NAN` has no JSON spelling, and `serde_json` maps it to `null` instead
/// of failing. So a host that switches from `format!` to this builder trades a
/// loud failure (`NaN` is not TOML, and the layer will not parse) for a quiet
/// one (the field arrives as `null`, and the plugin sees whatever its schema
/// makes of that — `None`, or a deserialize error naming the row).
///
/// Neither is good, and the fix belongs upstream of both: a config that reaches
/// here carrying NaN was already wrong when the person wrote it. This exists so
/// nobody reads the builder as validation it does not do.
#[test]
fn a_non_finite_number_becomes_null_rather_than_an_error() {
    #[derive(serde::Serialize)]
    struct Options {
        temperature: f32,
    }
    let entry = Entry::named("chat-options")
        .with(Options {
            temperature: f32::NAN,
        })
        .expect("serde_json maps NaN to null instead of failing");
    assert_eq!(
        entry.config["temperature"],
        serde_json::Value::Null,
        "a NaN that reaches the tree arrives as null, not as the number"
    );
}

/// Consecutive inserts stay one op, so a built layer round-trips through its own
/// serialization the way a parsed one does.
#[test]
fn a_built_layer_serializes_back_to_what_it_means() {
    let built = Layer::new()
        .insert(Entry::named("a"))
        .insert(Entry::named("b"))
        .insert(Entry::with_id("b-2", "b").disabled());
    let json = serde_json::to_string(&built).unwrap();
    let back: Layer = serde_json::from_str(&json).unwrap();
    assert_eq!(tree([built]), tree([back]));
}
