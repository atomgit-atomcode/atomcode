//! Probe: the mounted row order, so a claim about gate ordering is read off the
//! tree instead of inferred from where a row is written in the list.
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
