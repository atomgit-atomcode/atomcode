//! The three-role convention, enforced rather than documented.
//!
//! DeepSeek Harness keeps a curated `SERVICE_ROLES` table and a generator that
//! cross-checks it against the source. These tests are the same idea with the
//! curation removed: the classification lives on the service definition, the
//! roles live on the plugins, and the checks below make a lie impossible to
//! merge.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_harness::profile::Profiles;
use atomcode_harness::seam_map::{
    seam_definitions, seam_map, undeclared_services, AGENT_PROVIDED, HOST_CONSUMED, HOST_PROVIDED,
};
use atomcode_harness::seams::{LlmSvc, ToolsSvc, UiSvc};
use atomcode_harness::{bundle, plugins};
use atomcode_plexus::{
    App, AuditFinding, ConfigTree, Context, Layer, Plugin, PluginRegistry, SeamMode, ServiceKey,
};
use serde_json::Value;

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

fn offline_tree(extra: &[&str]) -> ConfigTree {
    let script = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [ { text = "ok" } ] }
"#;
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    let sandbox = std::env::temp_dir().join(format!("plexus-convention-{}", std::process::id()));
    std::fs::create_dir_all(&sandbox).unwrap();
    let scoped = format!(
        "[[patch]]\nid = \"skills\"\nconfig = {{ project_root = {root:?}, home = {root:?} }}\n\n\
         [[patch]]\nid = \"memory\"\nconfig = {{ project_root = {root:?} }}\n",
        root = sandbox.to_string_lossy()
    );
    let mut layers = vec![
        bundle::base().unwrap(),
        Layer::from_toml(bundle::ONESHOT_APP).unwrap(),
    ];
    for src in [script, quiet, scoped.as_str()] {
        layers.push(Layer::from_toml(src).unwrap());
    }
    for src in extra {
        layers.push(Layer::from_toml(src).unwrap());
    }
    ConfigTree::from_layers(layers).unwrap()
}

#[test]
fn every_service_a_plugin_touches_is_on_the_capability_map() {
    // The failure this prevents: someone adds a service, wires it up, ships it,
    // and the architecture doc never learns it exists.
    let missing = undeclared_services(&plugins::catalog());
    assert!(
        missing.is_empty(),
        "these services are declared by plugins but have no seam definition: {missing:?}"
    );
}

#[test]
fn every_seam_definition_has_at_least_one_provider() {
    // A definition nobody implements is an interface with no behaviour behind
    // it — the seam equivalent of a dangling reference. The exception is a slot
    // only the host can fill: reconfiguring the tree needs the `App`, and no
    // plugin has one.
    let orphans: Vec<&str> = seam_map(&plugins::catalog())
        .into_iter()
        .filter(|row| {
            row.providers.is_empty()
                && !HOST_PROVIDED.contains(&row.name)
                && !AGENT_PROVIDED.contains(&row.name)
        })
        .map(|row| row.name)
        .collect();
    assert!(orphans.is_empty(), "seams with no provider: {orphans:?}");
}

#[test]
fn a_host_provided_seam_is_declared_as_such() {
    // `control` has no plugin behind it on purpose. Saying so in one place
    // keeps the two checks above honest instead of quietly special-casing it.
    for name in HOST_PROVIDED {
        assert!(
            seam_definitions().iter().any(|(n, _, _)| n == name),
            "`{name}` is host-provided but not on the map"
        );
    }
}

#[test]
fn every_seam_definition_has_at_least_one_consumer() {
    let unused: Vec<&str> = seam_map(&plugins::catalog())
        .into_iter()
        .filter(|row| !row.has_consumers())
        .map(|row| row.name)
        .collect();
    assert!(
        unused.is_empty(),
        "seams nothing reads (dead weight, or a consumer that forgot to declare it): {unused:?}"
    );
}

#[test]
fn the_map_reports_which_seams_are_only_nominally_replaceable() {
    let rows = seam_map(&plugins::catalog());
    let nominal: Vec<&str> = rows
        .iter()
        .filter(|row| row.is_nominal_seam())
        .map(|row| row.name)
        .collect();
    // Not a failure — a young seam legitimately has one provider. The point is
    // that the map says so out loud instead of implying a choice that does not
    // exist yet.
    assert!(
        !nominal.is_empty(),
        "if every seam has two providers, this assertion has outlived its purpose"
    );
    for row in rows.iter().filter(|r| r.mode == SeamMode::Seam) {
        assert!(
            !row.title.is_empty(),
            "`{}` is a seam with no title; the map would show a blank",
            row.name
        );
    }
}

#[test]
fn the_classification_lives_on_the_definition() {
    // A trait object face is what makes replacement possible, so a `Seam` must
    // have one and a `Core` registry is a concrete type. Checked on two
    // representatives rather than by reflection, which Rust does not have.
    assert_eq!(LlmSvc::MODE, SeamMode::Seam);
    assert_eq!(ToolsSvc::MODE, SeamMode::Core);
    assert_eq!(LlmSvc::NAME, "llm");

    // And every definition carries both facts.
    for (name, _mode, title) in seam_definitions() {
        assert!(!name.is_empty());
        assert!(!title.is_empty(), "`{name}` has no title");
    }
}

#[tokio::test]
async fn the_shipped_tree_audits_clean() {
    let mut app = App::new(plugins::catalog(), offline_tree(&[]));
    app.start().await.unwrap();
    let findings = app.audit_with(HOST_CONSUMED, HOST_PROVIDED);
    assert!(
        findings.is_empty(),
        "the default composition is inconsistent: {}",
        findings
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    );
}

/// Resolve a shipped profile with the model scripted, so no profile needs a key.
fn profile_tree(name: &str) -> ConfigTree {
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    Profiles::builtin()
        .resolve(name, &[bundle::OFFLINE, quiet])
        .unwrap_or_else(|e| panic!("profile `{name}` does not resolve: {e}"))
}

#[tokio::test]
async fn every_shipped_profile_mounts_and_audits_clean() {
    // Every named assembly, not a sample: a profile that does not come up is a
    // broken product surface, and the only way to know is to mount all of them.
    for name in Profiles::builtin().names() {
        let mut app = App::new(plugins::catalog(), profile_tree(name));
        app.start()
            .await
            .unwrap_or_else(|e| panic!("profile `{name}` does not mount: {e}"));
        let findings: Vec<_> = app
            .audit_with(HOST_CONSUMED, HOST_PROVIDED)
            .into_iter()
            .filter(|f| f.is_defect())
            .collect();
        assert!(
            findings.is_empty(),
            "profile `{name}` is inconsistent: {}",
            findings
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
}

#[tokio::test]
async fn every_shipped_profile_mounts_a_front_end() {
    for name in Profiles::builtin().names() {
        let mut app = App::new(plugins::catalog(), profile_tree(name));
        app.start().await.unwrap();
        assert!(
            app.context().service::<UiSvc>().is_some(),
            "profile `{name}` mounts no `ui` row, so the launcher has nothing to hand over to"
        );
    }
}

#[tokio::test]
async fn every_overlay_composes_with_every_profile() {
    // The claim being tested is composability, not any one combination: an
    // overlay that only works on the profile it was written against is a flag
    // wearing a patch file's clothes.
    let overlays = [
        ("plan", bundle::PLAN),
        ("read-only", bundle::READ_ONLY),
        ("native-tools", bundle::NATIVE_TOOLS),
        ("full", bundle::FULL),
    ];
    let quiet =
        "[[patch]]\nid = \"trace\"\nconfig = { stream = false, tools = false, summary = false }";
    for name in Profiles::builtin().names() {
        for (label, overlay) in overlays {
            let tree = Profiles::builtin()
                .resolve(name, &[bundle::OFFLINE, quiet, overlay])
                .unwrap_or_else(|e| panic!("`{name}` + `{label}` does not resolve: {e}"));
            let mut app = App::new(plugins::catalog(), tree);
            app.start()
                .await
                .unwrap_or_else(|e| panic!("`{name}` + `{label}` does not mount: {e}"));
            let findings: Vec<_> = app
                .audit_with(HOST_CONSUMED, HOST_PROVIDED)
                .into_iter()
                .filter(|f| f.is_defect())
                .collect();
            assert!(
                findings.is_empty(),
                "`{name}` + `{label}` is inconsistent: {}",
                findings
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
    }
}

// ---- the audit catches the two ways a declaration can lie -----------------

/// Says it provides `llm` and quietly does not.
struct LiesAboutProviding;

#[async_trait]
impl Plugin for LiesAboutProviding {
    fn name(&self) -> &'static str {
        "lies-about-providing"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    async fn apply(&self, _ctx: &Context, _config: &Value) -> Result<(), String> {
        Ok(())
    }
}

/// Fills `llm` without declaring it — invisible to the capability map.
struct ProvidesInSecret;

#[async_trait]
impl Plugin for ProvidesInSecret {
    fn name(&self) -> &'static str {
        "provides-in-secret"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<LlmSvc>(Arc::new(SilentProvider))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

struct SilentProvider;

#[async_trait]
impl atomcode_kernel::provider::LlmProvider for SilentProvider {
    fn model_name(&self) -> &str {
        "silent"
    }
    async fn chat_stream(
        &self,
        _messages: &[atomcode_kernel::message::Message],
        _tools: &[atomcode_kernel::tool::ToolDef],
        _options: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        Ok(Box::pin(futures::stream::iter(Vec::new())))
    }
}

async fn audit_with(plugin: Arc<dyn Plugin>, row: &str) -> Vec<AuditFinding> {
    let mut registry = PluginRegistry::new();
    registry.register(plugin);
    let tree = ConfigTree::from_layers([Layer::from_toml(row).unwrap()]).unwrap();
    let mut app = App::new(registry, tree);
    app.start().await.unwrap();
    app.audit()
}

#[tokio::test]
async fn declaring_a_provider_and_not_filling_the_slot_is_caught() {
    let findings = audit_with(
        Arc::new(LiesAboutProviding),
        "[[insert]]\nname = \"lies-about-providing\"",
    )
    .await;
    assert!(
        findings.contains(&AuditFinding::DeclaredButNotProvided {
            entry: "lies-about-providing".into(),
            service: "llm",
        }),
        "a half-mounted seam looks configured and is not: {findings:?}"
    );
}

#[tokio::test]
async fn filling_a_slot_without_declaring_it_is_caught() {
    let findings = audit_with(
        Arc::new(ProvidesInSecret),
        "[[insert]]\nname = \"provides-in-secret\"",
    )
    .await;
    assert!(
        findings.contains(&AuditFinding::ProvidedButNotDeclared {
            entry: "provides-in-secret".into(),
            service: "llm",
        }),
        "an undeclared provider cannot appear on the map: {findings:?}"
    );
}

#[tokio::test]
async fn injecting_something_nobody_can_provide_is_caught_before_it_deadlocks() {
    struct WantsTheImpossible;
    #[async_trait]
    impl Plugin for WantsTheImpossible {
        fn name(&self) -> &'static str {
            "wants-the-impossible"
        }
        fn uses(&self) -> &'static [&'static str] {
            &["a-service-that-does-not-exist"]
        }
        async fn apply(&self, _ctx: &Context, _config: &Value) -> Result<(), String> {
            Ok(())
        }
    }
    let findings = audit_with(
        Arc::new(WantsTheImpossible),
        "[[insert]]\nname = \"wants-the-impossible\"",
    )
    .await;
    assert!(
        findings.iter().any(|f| matches!(
            f,
            AuditFinding::InjectedButUnprovidable { service, .. }
                if *service == "a-service-that-does-not-exist"
        )),
        "{findings:?}"
    );
}

#[test]
fn the_map_renders_both_a_table_and_a_graph() {
    let rows = seam_map(&plugins::catalog());
    let table = atomcode_harness::seam_map::render_table(&rows);
    assert!(table.contains("Seams — replaceable capabilities"));
    assert!(table.contains("llm  — Model adapter"));
    assert!(
        table.contains("provided by: llm-atomcode-config, llm-openai-compat, llm-replay"),
        "the map lists every provider of a seam, not the mounted one"
    );

    let mermaid = atomcode_harness::seam_map::render_mermaid(&rows);
    assert!(mermaid.starts_with("flowchart LR"));
    assert!(mermaid.contains("pkg_llm_replay -->|provides| svc_llm"));
    assert!(mermaid.contains("svc_llm -->|injects| pkg_agent_loop"));
}

/// The capability map must be able to say **who gave the model a tool**.
///
/// `provides` says who *owns* a catalog; before `contributes` existed, that was
/// all the map knew, so `tools` read `provided by: tools` while a dozen other
/// rows were the ones putting anything in it. This pins the visible half of the
/// fix: the slot `tools` names its contributors, and the items are the tool
/// names the model is actually offered.
#[tokio::test]
async fn a_catalog_names_the_rows_that_contributed_to_it() {
    let mut app = App::new(plugins::catalog(), offline_tree(&[]));
    app.start().await.unwrap();

    let rows = seam_map(&plugins::catalog());
    let tools = rows
        .iter()
        .find(|r| r.name == "tools")
        .expect("`tools` is on the map");

    // The holder is still the one row that builds the catalog...
    assert_eq!(
        tools.providers,
        vec!["tools"],
        "one holder owns the catalog — and it is not a contributor, which is \
         exactly why the map needed a second column"
    );
    // ...and the contributors are the many.
    assert!(
        tools.contributions.len() >= 8,
        "the catalog has many contributors, not {}: {:?}",
        tools.contributions.len(),
        tools.contributions
    );
    let named = |item: &str, row: &str| {
        tools
            .contributions
            .iter()
            .any(|(i, r)| *i == item && *r == row)
    };
    assert!(
        named("read_file", "tool-fs-world"),
        "{:?}",
        tools.contributions
    );
    assert!(
        named("bash", "tool-bash-world"),
        "{:?}",
        tools.contributions
    );
    assert!(named("todowrite", "tool-todo"), "{:?}", tools.contributions);

    // And the audit agrees with the tree it was read from: every contribution
    // was declared. Without `contributes` this is where the undeclared ones
    // would surface — which is the point of recording them at the write path.
    let defects: Vec<_> = app
        .audit_with(HOST_CONSUMED, HOST_PROVIDED)
        .into_iter()
        .filter(AuditFinding::is_defect)
        .collect();
    assert!(defects.is_empty(), "{defects:?}");
}

/// The defect that keeps the map honest must actually fire.
///
/// A contribution the row never declared is invisible on the capability map —
/// the exact failure `contributes` was added to fix — so the audit has to catch
/// it. Verified by mounting a row that records a contribution and declares
/// nothing: a check that never goes red is a check nobody can trust.
#[tokio::test]
async fn an_undeclared_contribution_is_a_defect() {
    struct Sneaky;

    #[async_trait]
    impl Plugin for Sneaky {
        fn name(&self) -> &'static str {
            "test-sneaky"
        }
        async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
            // What a row does when it adds to a catalog through some path that
            // forgot to declare it. `provides`/`contributes` are both silent.
            ctx.note_contribution("tools", "sneaky_tool");
            Ok(())
        }
    }

    let mut catalog = plugins::catalog();
    catalog.register(Arc::new(Sneaky));
    let mut app = App::new(
        catalog,
        offline_tree(&["[[insert]]\nname = \"test-sneaky\"\n"]),
    );
    app.start().await.unwrap();

    let findings: Vec<String> = app
        .audit_with(HOST_CONSUMED, HOST_PROVIDED)
        .into_iter()
        .filter(AuditFinding::is_defect)
        .map(|f| f.to_string())
        .collect();
    assert!(
        findings.iter().any(|f| f.contains("sneaky_tool")),
        "an undeclared contribution must be named: {findings:?}"
    );
}
