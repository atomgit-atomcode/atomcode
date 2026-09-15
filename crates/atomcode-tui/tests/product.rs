//! The product, judged.
//!
//! `atui` is a coding agent with a screen in front of it, and the whole point of
//! `product.rs` is that this sentence is stated **once** — the row list lives in
//! `atomcode-coding`, and this crate stacks it rather than restating it. That
//! arrangement has one failure mode, and it is the one this file exists to
//! catch: `atomcode-coding` grows a row, nobody adds it here, and the product
//! ships a capability it names but does not mount. A person finds out one
//! support question at a time; `ast_grep`, the code graph, `open_file` and the
//! subagent were each found that way before.
//!
//! So the assertions are about *inclusion*, not about equality: every row
//! coding names must be addressable in this product's tree, and a row this
//! product adds (a panel, a surface) is none of coding's business.

use atomcode_coding::on_harness::{coding_overlay, Presence};
use atomcode_plexus::{App, Layer, Op};
use atomcode_tui::product;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn scratch(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("atui-product-{}-{tag}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Every row id a layer addresses. Read out of the parsed layer rather than
/// grepped out of the TOML, so a row written with a different `id` is still
/// counted the way the loader counts it.
fn ids_named_by(src: &str) -> Vec<String> {
    let layer = Layer::from_toml(src).expect("layer parses");
    let mut out = Vec::new();
    for op in &layer.ops {
        match op {
            Op::Insert(entries) => out.extend(entries.iter().map(|e| e.id.clone())),
            Op::Patch { id, .. } => out.push(id.clone()),
            Op::Remove { id } => out.push(id.clone()),
        }
    }
    out
}

/// The two things `atui` overlays for a test: no tty, and no config.toml.
///
/// Both are the launcher's flags one line lower — `--offline` is a patch to
/// `llm`, `--headless` a patch to `surface` — so this is the product's own
/// assembly with the environment's two questions answered, not a second
/// assembly.
const SCRIPTED: &str = "[[patch]]\nid = \"llm\"\nname = \"llm-replay\"\n";

const NO_TTY: &str = "[[patch]]\nid = \"surface\"\nname = \"surface-headless\"\n";

fn assembled(home: &std::path::Path, extra: &[&str]) -> atomcode_plexus::ConfigTree {
    let assembly = product::assembly();
    let profiles = assembly.profiles().rooted_at(home).with_home();
    let mut overlays = vec![SCRIPTED, NO_TTY];
    overlays.extend_from_slice(extra);
    profiles
        .resolve(product::PROFILE, &overlays)
        .unwrap_or_else(|e| panic!("the product profile resolves: {e}"))
}

#[test]
fn every_row_coding_names_is_addressable_in_this_product() {
    // What the product claims to be, named by the crate that owns the claim.
    let working_dir = std::env::current_dir().expect("cwd");
    let artifacts = working_dir.join(".atomcode").join("artifacts");
    let coding = format!(
        "{}\n{}",
        atomcode_coding::on_harness::CODING_DEFAULTS,
        coding_overlay(&working_dir, &artifacts, Presence::Attended, "a-model-name"),
    );

    let tree = assembled(&scratch("gap"), &[]);
    let mounted: Vec<&str> = tree.entries.iter().map(|e| e.id.as_str()).collect();

    let missing: Vec<String> = ids_named_by(&coding)
        .into_iter()
        .filter(|id| !mounted.contains(&id.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "coding names {} row(s) this product never mounts: {missing:?}\n\
         A row list can only be changed by the crate that owns it; a host that \
         stacks it gets every row or the alignment is a story.",
        missing.len()
    );

    // **Being addressable is not the same as being mounted.**
    //
    // Every id above can be present in the tree and the row still switched off
    // (`disabled = true`) or handed a different configuration by a layer that
    // runs later — which is exactly how this product silently acquired a
    // 24-round cap: `infra` ships `round-cap` at 24, coding never mentions it,
    // and "the id is in the tree" said nothing.
    //
    // So each row coding configures is checked against what coding asked for.
    // Names coding only *names* (a `[[remove]]`, say) have nothing to compare
    // and are skipped.
    let mut wrong: Vec<String> = Vec::new();
    // Which (row, field) pairs really do differ, declaration or not. Gathered so
    // the declarations can be checked against reality at the end: one that
    // stopped being true is a note explaining something that no longer happens,
    // which is worse than no note — same rule as `KNOWN_TOOL_DIFFERENCES` in
    // `differential.rs`.
    let mut differing: Vec<(String, String)> = Vec::new();
    // **The comparison is against the tree, and the tree includes this
    // product's own layers.** `CODING_ROWS` is the first word on a row, not the
    // last: `tui-app`/`repl-app` patch `approval-interactive` on afterwards
    // (`bundle.rs`), so reading only coding's string answers with a value the
    // product has already overruled — which is how this check first reported a
    // row `--dump-config` calls `Active`. What is compared, then, is what
    // coding asked for against what the tree ended up with, and the product's
    // own edits are edits like any other: declared, or a failure.
    for (id, want_disabled, want_config) in ids_and_config(&coding) {
        let Some(mine) = tree.entries.iter().find(|e| e.id == id) else {
            continue; // already reported above
        };
        // `disabled` first: a row the product switched off is a different agent
        // no matter what config it carries. **Per field**, so a declared
        // divergence on one key of a row does not also excuse `disabled`.
        if let Some(want) = want_disabled {
            if mine.disabled != want {
                differing.push((id.clone(), "disabled".to_string()));
                if !deliberate(&id, "disabled") {
                    wrong.push(format!(
                        "`{id}` — coding sets disabled = {want}, this product has {}",
                        mine.disabled
                    ));
                }
            }
        }
        // Then only the keys coding actually set, so a row with extra options
        // the product adds on purpose is not called a mismatch.
        if let Some(want) = want_config.as_object() {
            for (k, v) in want {
                let got = mine.config.get(k);
                if got != Some(v) {
                    differing.push((id.clone(), k.clone()));
                    if !deliberate(&id, k) {
                        wrong.push(format!(
                            "`{id}` — coding sets {k} = {v}, this product has {got:?}"
                        ));
                    }
                }
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "coding's rows are in the tree but not as coding asked for them:\n  {}\n\
         A row that is present, off, or reconfigured is a different agent, and \
         no id-level check can see it. If the change is deliberate, add it to \
         DELIBERATE_DIVERGENCES with its reason.",
        wrong.join("\n  ")
    );

    // And every declaration has to be real. One that no longer differs is a
    // reason kept for something that stopped happening — and, worse, an
    // exemption still granting cover over a field the product no longer
    // overrules.
    let stale: Vec<&str> = DELIBERATE_DIVERGENCES
        .iter()
        .filter(|(row, field, _)| !differing.iter().any(|(r, f)| r == row && f == field))
        .map(|(row, _, _)| *row)
        .collect();
    assert!(
        stale.is_empty(),
        "these rows are declared as deliberate divergences but no longer differ, \
         so the declaration is stale: {stale:?} — drop the entry rather than \
         leaving a reason for something that stopped happening"
    );
}

/// Divergences from coding's own list that this product makes **on purpose**.
///
/// The list is the point, in both directions. Without it, every deliberate
/// difference reads as a regression and the check gets switched off the first
/// time it is inconvenient — which is how the id-level version of this test
/// came to exist.
///
/// **Scoped to one field, and that scope is enforced.** An earlier version
/// declared the whole row, so `triple`-style exemptions meant `persona-atomcode`
/// could have its `disabled`, its `name` or anything else changed and still pass
/// — a list of rows is a list of holes. Each entry names the one field this
/// product overrules, and every other field of that row is still compared.
///
/// Each entry: the row, the field, and why.
const DELIBERATE_DIVERGENCES: &[(&str, &str, &str)] = &[
    (
        "persona-atomcode",
        "model",
        "left empty on purpose: an empty model means 'ask the running tree', \
         because the provider here is built by the `llm` row and `--model` can \
         change it after this layer was written (product.rs docs)",
    ),
    (
        "trace",
        "stream",
        "silenced: the screen is the output, so nothing else may write to it",
    ),
    ("trace", "tools", "silenced, same reason as `stream`"),
    ("trace", "summary", "silenced, same reason as `stream`"),
    (
        "ui-handle",
        "disabled",
        "off: this product's agent is driven by the pump inside `ui-tui2`, and \
         both rows fill `ui`",
    ),
    (
        "approval-interactive",
        "disabled",
        "on: this product stacks `tui-app`/`repl-app`, which patch it on, and \
         `Presence::Attended` is exactly the mode whose out-of-workspace calls are \
         meant to arrive as questions rather than refusals (product.rs docs)",
    ),
];

/// Whether this one field of this row is a difference the product declares.
///
/// Field-scoped rather than row-scoped: an exemption for a row is an exemption
/// for everything about it, which is how a declared `model` divergence would
/// silently cover a later `disabled` one.
fn deliberate(row: &str, field: &str) -> bool {
    DELIBERATE_DIVERGENCES
        .iter()
        .any(|(r, f, _)| *r == row && *f == field)
}

/// Every `(id, disabled, config)` the layer ends up asking for.
///
/// `ids_named_by` above answers "is it addressed at all"; this answers "with
/// what **in the end**". The distinction is not pedantry: a layer may name the
/// same id twice — `CODING_ROWS` inserts `approval-interactive` disabled and a
/// later patch turns it on — and the loader's rule is that the later one wins.
/// A reader that collects every mention answers with the *first*, which is how
/// this check first reported a row that `--dump-config` calls `Active`.
///
/// `disabled` accumulates the way the loader resolves it: an explicit `Some`
/// sets it, a patch that says nothing about `disabled` leaves the earlier
/// answer standing.
fn ids_and_config(src: &str) -> Vec<(String, Option<bool>, serde_json::Value)> {
    let layer = Layer::from_toml(src).expect("layer parses");
    let mut out: Vec<(String, Option<bool>, serde_json::Value)> = Vec::new();
    let mut put = |id: &str, disabled: Option<bool>, config: Option<&serde_json::Value>| match out
        .iter_mut()
        .find(|(seen, _, _)| seen == id)
    {
        Some(slot) => {
            if disabled.is_some() {
                slot.1 = disabled;
            }
            if let Some(c) = config {
                slot.2 = c.clone();
            }
        }
        None => out.push((
            id.to_string(),
            disabled,
            config.cloned().unwrap_or(serde_json::Value::Null),
        )),
    };
    for op in &layer.ops {
        match op {
            Op::Insert(entries) => {
                for e in entries {
                    // `Some(e.disabled)`, not `then_some(true)`: an insert that
                    // ships a row **enabled** is stating `disabled = false`, and
                    // `then_some(true)` turned that statement into "no opinion" —
                    // so switching such a row off in this product compared
                    // nothing at all. `verify-cadence` is one, which is how the
                    // hole was found: disable it and the check stayed green.
                    put(&e.id.clone(), Some(e.disabled), Some(&e.config));
                }
            }
            Op::Patch {
                id,
                config,
                disabled,
                ..
            } => put(id, *disabled, config.as_ref()),
            Op::Remove { .. } => {}
        }
    }
    out
}

#[test]
fn the_screen_drives_the_agent_and_the_generic_persona_stands_down() {
    let tree = assembled(&scratch("front"), &[]);
    let row = |id: &str| tree.entries.iter().find(|e| e.id == id);

    // The front end is this crate's, and coding's driver protocol is the row
    // that stands down — not the other way round, which would mean the screen
    // is drawn by nobody.
    assert_eq!(row("ui").expect("`ui` is mounted").name, "ui-tui2");
    assert!(
        row("ui-handle").is_some_and(|e| e.disabled),
        "`ui-handle` is disabled, not removed: it is still the way in for a \
         runtime that speaks the protocol"
    );
    // Two personas in one system prompt is worse than either, and coding's is
    // the one that says what this product is.
    assert!(
        row("persona-atomcode").is_some_and(|e| !e.disabled),
        "coding's persona is the one that runs"
    );
    assert!(
        row("persona-coding").is_none(),
        "the harness's generic persona must not be mounted beside it"
    );
}

#[test]
fn the_persons_own_patch_still_wins() {
    // The hazard this test exists for: a product layer that RESTATES a row
    // replaces it, so if the product's rows were stacked after the user's home
    // patch, this override would be silently reverted. It is asserted on a row
    // the product's own bundle is the one that turned ON.
    let home = scratch("homepatch");
    std::fs::write(
        home.join("harness.patch.toml"),
        "[[patch]]\nid = \"code-graph\"\ndisabled = true\n",
    )
    .expect("home patch");

    let tree = assembled(&home, &[]);
    let graph = tree
        .entries
        .iter()
        .find(|e| e.id == "code-graph")
        .expect("the product mounts `code-graph`");
    assert!(
        graph.disabled,
        "the person's own patch is the last word, including on the rows the \
         product turned on"
    );
}

#[tokio::test]
async fn the_prompt_a_row_contributes_leaves_with_the_row_on_this_profile_too() {
    // `differential.rs` locks this on `mount_swappable`; the product reaches the
    // same rows through a different path (`Profiles` + bundles), and it is the
    // path `atui` actually ships. What the model reads must be one answer per
    // tool, contributed by the row that mounts it — the failure this catches is
    // the coding persona describing the CHAIN assembly's tools again, which is
    // what made "the composition ships a capability it names but does not
    // mount" visible to the model rather than only to `--audit`.
    let assembly = product::assembly();
    let mut app = App::new(assembly.catalog(), assembled(&scratch("prompt"), &[]));
    app.start().await.unwrap_or_else(|e| panic!("mount: {e}"));
    let prompt = app
        .context()
        .service::<atomcode_harness::seams::SystemPromptSvc>()
        .expect("the prompt registry is mounted")
        .render();

    // Each row's own words about its own tool, including the judgment a
    // parameter list cannot carry.
    for (fragment, why) in [
        (
            "delegates a self-contained job",
            "the `task` row describes its own tool",
        ),
        (
            "runs named child agents that stay around",
            "so does the `team` row",
        ),
        (
            "Not for a single read or grep",
            "the when-NOT-to-delegate rule survives on this path",
        ),
        (
            "not only the next action",
            "the `tool-todo` row carries the planning rules, not just a pointer",
        ),
        (
            "pass the requested scope and path filters straight to it",
            "the `tool-code-review` row carries the review routing rule",
        ),
        (
            "## MEMORY",
            "`memory` contributes no paragraph, so the persona keeps this one — \
             gated on `has(\"memory\")`, not on the env",
        ),
    ] {
        assert!(prompt.contains(fragment), "{why}:\n{prompt}");
    }

    // And the other engine's copy of any of it, nowhere: this product mounts the
    // harness rows, whose `team` has no `wait` and whose `task` takes no
    // `subagent_type`.
    for fragment in [
        "## TEAM AGENT:",
        "## DELEGATING WITH `task`",
        "subagent_type",
    ] {
        assert!(
            !prompt.contains(fragment),
            "`{fragment}` describes the chain assembly's tools, which this tree does not mount:\n{prompt}"
        );
    }
    for section in ["## TASK TRACKING:", "## CODE REVIEW:"] {
        assert_eq!(
            prompt.matches(section).count(),
            0,
            "`{section}` belongs to the row that mounts the tool now:\n{prompt}"
        );
    }
}

#[tokio::test]
async fn the_product_mounts_and_audits_clean() {
    // Mounting is the only way to catch a row that names a plugin nobody
    // registered: the tree resolves happily, and the row is simply not there.
    let assembly = product::assembly();
    let mut app = App::new(assembly.catalog(), assembled(&scratch("mount"), &[]));
    app.start().await.unwrap_or_else(|e| panic!("mount: {e}"));

    let mut consumed: Vec<&str> = atomcode_harness::seam_map::HOST_CONSUMED.to_vec();
    consumed.extend_from_slice(&["tui-modules", "tui-commands"]);
    let findings = app.audit_with(&consumed, atomcode_harness::seam_map::HOST_PROVIDED);
    let defects: Vec<String> = findings
        .iter()
        .filter(|f| f.is_defect())
        .map(|f| f.to_string())
        .collect();
    assert!(defects.is_empty(), "composition defects: {defects:?}");

    // And the agent this screen drives is really there — a tree that audits
    // clean but hands over nothing is the failure mode `--audit` cannot see.
    assert!(
        app.context()
            .service::<atomcode_tui::plugin::AgentClientSvc>()
            .is_some(),
        "`ui-tui2` provides the command channel to its agent"
    );
}

/// The round budget is the engine's, not `infra`'s.
///
/// `infra` ships `round-cap` at `max_rounds = 24`, and this product inherited
/// that by saying nothing — so a session the coding engine would have run
/// unbounded was cut off after 24 rounds. The engine's own default for the
/// equivalent knob is `0` meaning *unbounded*
/// (`atomcode-coding/src/config.rs`: `default_turn_max_rounds` → `0`, honored by
/// `if cfg.max_rounds != 0 { builder.max_rounds(…) }`).
///
/// **Why this needs its own test.** The id-level check above cannot see it and
/// neither can the config-level version: `round-cap` comes from `infra` and
/// coding never names it, so "coding's rows, as coding asked" has nothing to
/// say about a row coding does not know exists. A row can be silently
/// reconfigured by the *absence* of a patch, and only a test that names the row
/// catches that.
#[test]
fn the_round_budget_is_the_engines_default_not_the_infras() {
    let tree = assembled(&scratch("rounds"), &[]);
    let row = tree
        .entries
        .iter()
        .find(|e| e.id == "round-cap")
        .expect("`infra` ships `round-cap`, so it is in this tree");
    assert!(
        !row.disabled,
        "`round-cap` must be mounted — an unmounted row is not a budget, it is \
         a missing fuse that looks like an unlimited one"
    );
    assert_eq!(
        row.config.get("max_rounds"),
        Some(&serde_json::json!(0)),
        "this product must not cap rounds where the engine does not: `0` is the \
         engine's own default and it means unbounded. Got {:?}",
        row.config.get("max_rounds")
    );
}

/// And a deployment that wants a fuse can still set one.
///
/// The mapping above is a *default*, not a policy: `--patch` and
/// `harness.patch.toml` both land after the product's layer, so the escape
/// hatch has to keep working or the fix trades one wrong answer for another.
#[test]
fn a_deployment_can_still_cap_rounds() {
    let tree = assembled(
        &scratch("rounds-patch"),
        &["[[patch]]\nid = \"round-cap\"\nconfig = { max_rounds = 12 }\n"],
    );
    let row = tree
        .entries
        .iter()
        .find(|e| e.id == "round-cap")
        .expect("the row is there");
    assert_eq!(
        row.config.get("max_rounds"),
        Some(&serde_json::json!(12)),
        "a later layer must still win over this product's default"
    );
}
