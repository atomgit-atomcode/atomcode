//! What the product actually mounts: the row order, the seams, and the config
//! each row ends up with.
//!
//! Every test here sets `ATOMCODE_HOME`, so every one of them takes the
//! `atomcode_home` serial lock — the same one `mount_wiring` uses. Without it
//! they clobber each other's home mid-run, which shows up as a test that passes
//! alone and fails in company.
mod support;

use std::sync::Arc;

use atomcode_coding::CodingAgentConfig;

#[derive(Debug)]
struct Silent;

#[async_trait::async_trait]
impl atomcode_kernel::provider::LlmProvider for Silent {
    fn model_name(&self) -> &str {
        "m"
    }
    async fn chat_stream(
        &self,
        _: &[atomcode_kernel::message::Message],
        _: &[atomcode_kernel::tool::ToolDef],
        _: &atomcode_kernel::provider::ChatOptions,
    ) -> Result<
        futures::stream::BoxStream<'static, atomcode_kernel::stream::StreamEvent>,
        atomcode_kernel::stream::ProviderError,
    > {
        Ok(Box::pin(futures::stream::iter(vec![
            atomcode_kernel::stream::StreamEvent::Done { truncated: false },
        ])))
    }
}

#[tokio::test]
#[serial_test::serial(atomcode_home)]
#[ignore = "probe, not a criterion: prints the mounted row order"]
async fn dump_row_order() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let mut cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
    let (rules, invalid) =
        atomcode_capabilities::tools::PermissionRules::parse(&["Bash(curl *)".to_string()], &[]);
    assert!(invalid.is_empty());
    cfg.permission_rules = Arc::new(rules);

    let mounted = support::mount(&cfg, support::quiet_options(), Arc::new(Silent)).await;
    for (i, row) in mounted.rows().iter().enumerate() {
        println!("{i:3}  {row}");
    }
    mounted.stop();
}

/// The product mounts clean: no row declares a seam it then leaves empty, and
/// nothing injects a seam no registered plugin can fill.
///
/// Mounting is the only way to catch these — the config tree resolves happily
/// either way. This is the sweep the hand-written chain never had an equivalent
/// of: a chain that forgot to wire something simply did not wire it, and you
/// found out from behaviour. `atomcode-tui` has run this since it existed; the
/// coding product did not until the chain came out and the row list became the
/// only assembly.
///
/// Negative control: add a row whose plugin declares `provides` and returns
/// without filling the slot, and this reports `DeclaredButNotProvided`.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn the_product_mounts_and_audits_clean() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());

    let mounted = support::mount(&cfg, support::quiet_options(), Arc::new(Silent)).await;
    let defects = mounted.audit();
    assert!(defects.is_empty(), "composition defects: {defects:?}");
    mounted.stop();

    // Again with the optional capabilities switched on, which is where a row
    // left dangling by the collapse would hide: the lean mount simply never
    // reaches it.
    let opts = atomcode_coding::PrepareOptions {
        memory: true,
        web: true,
        review: true,
        subagents: atomcode_coding::SubagentPolicy::Enabled,
        ..support::quiet_options()
    };
    let mounted = support::mount(&cfg, opts, Arc::new(Silent)).await;
    let defects = mounted.audit();
    assert!(
        defects.is_empty(),
        "composition defects with capabilities on: {defects:?}"
    );
    mounted.stop();
}

/// The person's `[permissions]` rules are evaluated once per tool call, not
/// twice.
///
/// The host registers the gate as a named middleware AND the middleware table
/// used to emit a `kernel-middleware-<name>` row for everything it held, so the
/// same rules ran at the front of the waterfall and again innermost. Harmless
/// for a pure decision, not harmless for one that ever counts, logs or audits —
/// and it hid the ordering question behind an instance that was always last.
///
/// Negative control: drop the `claimed` filter in `HostMiddleware::rows` and
/// this finds two rows.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn the_permission_gate_is_mounted_once() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let mut cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
    let (rules, invalid) =
        atomcode_capabilities::tools::PermissionRules::parse(&["Bash(curl *)".to_string()], &[]);
    assert!(invalid.is_empty());
    cfg.permission_rules = Arc::new(rules);

    let mounted = support::mount(&cfg, support::quiet_options(), Arc::new(Silent)).await;
    let carrying: Vec<String> = mounted
        .rows()
        .into_iter()
        .filter(|r| r.contains("permission-rules") || r.starts_with("permissions"))
        .collect();
    assert_eq!(
        carrying.len(),
        1,
        "the permission rules are mounted more than once: {carrying:?}"
    );
    mounted.stop();
}

/// No row silently loses a field the layers below it configured.
///
/// `Op::Patch` replaces a row's config WHOLESALE. So a later patch that omits a
/// field the base bundle set does not leave that field alone — it drops it back
/// to the serde default, and `--dump-config` cannot tell you whether the value
/// you are reading was chosen or defaulted.
///
/// Two real bugs came out of exactly this and nothing would have caught either:
/// `llm-retry` lost `attempts` on a `/model` swap, and `agent-loop` loses
/// `max_rounds` (the base sets 100, the runtime patches three other fields, and
/// the fuse quietly becomes the serde default — which happens to also be 100).
///
/// A drop that is meant lives in the allowlist below, WITH its reason. That is
/// the point: an intentional one is a sentence someone wrote, an accidental one
/// is a red test.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn no_row_silently_loses_a_configured_field() {
    // (row, field, why losing it is intended)
    // `session-persistence-jsonl` used to need an entry here for `resume`. It
    // turned out base was setting a key that row has never read — see
    // `bundle.rs` — so the answer was to delete it there, not to excuse it here.
    const INTENDED: &[(&str, &str, &str)] = &[(
        "agent-loop",
        "max_rounds",
        "The coarse runaway fuse is base's, and this product keeps it while carrying \
         its own budget on `round-cap`. Documented at length above the `round-cap` \
         patch in CODING_DEFAULTS, including that a host patching `agent-loop` \
         reverts this field — which is what makes it a decision rather than a slip.",
    )];

    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());

    // What the layers under the product declare, by row id. Stacked in the real
    // order — `CODING_DEFAULTS` patches rows `INFRA` inserts, so it does not
    // stand up on its own.
    let mut declared: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    let under = atomcode_plexus::ConfigTree::from_layers([
        atomcode_harness::bundle::infra().expect("infra parses"),
        atomcode_plexus::Layer::from_toml(atomcode_coding::on_harness::CODING_DEFAULTS)
            .expect("CODING_DEFAULTS parses"),
    ])
    .expect("the layers under the product stack");
    for entry in under.active() {
        if let Some(obj) = entry.config.as_object() {
            declared
                .entry(entry.id.clone())
                .or_default()
                .extend(obj.keys().cloned());
        }
    }

    let mounted = support::mount(&cfg, support::quiet_options(), Arc::new(Silent)).await;
    let mut lost: Vec<String> = Vec::new();
    for (id, config) in mounted.row_configs() {
        let Some(want) = declared.get(&id) else {
            continue;
        };
        let have: std::collections::BTreeSet<String> = config
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        for field in want.difference(&have) {
            if INTENDED
                .iter()
                .any(|(row, key, _)| *row == id && key == field)
            {
                continue;
            }
            lost.push(format!("`{id}` lost `{field}`"));
        }
    }
    mounted.stop();
    assert!(
        lost.is_empty(),
        "a patch replaced a row's config and dropped a field the layer below set \
         (carry it in the patch, or add it to INTENDED with a reason): {lost:?}"
    );
}

/// A session's log goes where the session is: the session store, under the
/// lease of the session the runtime opened (`docs/adr/0024` §5).
///
/// The harness's JSONL row, left mounted, would keep a second copy under no
/// lease — two writers of one session again — so it is off, and the store the
/// tree appends to is the one that reports the session's own log file.
#[tokio::test]
#[serial_test::serial(atomcode_home)]
async fn a_sessions_log_is_kept_in_the_session_store() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());
    let project = tempfile::tempdir().unwrap();
    let cfg = CodingAgentConfig::new("k", "http://localhost", "m", project.path());
    let mut opts = support::quiet_options();
    opts.session = atomcode_coding::SessionMode::Fresh;
    let parts = atomcode_coding::prepare(&cfg, opts.clone())
        .await
        .expect("prepare");
    let binding = parts.session.as_ref().expect("a fresh session is bound");

    let mounted = support::mount_parts(&parts, &cfg, &opts, Arc::new(Silent)).await;
    let rows = mounted.rows();
    assert!(
        rows.iter()
            .any(|row| row.starts_with("session-persistence-jsonl")
                && row.ends_with("session-store")),
        "the store row stands in the persistence row's place, not the harness's own: {rows:?}"
    );
    let store = mounted
        .context()
        .service::<atomcode_harness::seams::SessionPersistenceSvc>()
        .expect("a store is mounted");
    assert_eq!(
        std::path::PathBuf::from(store.location(&binding.id).unwrap()),
        binding.manager.events_path(&binding.id).unwrap(),
    );
    assert!(binding.manager.is_event_session(&binding.id));
    mounted.stop();
}
