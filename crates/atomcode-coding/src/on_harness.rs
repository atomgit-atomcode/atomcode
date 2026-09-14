//! Coding, assembled on the harness.
//!
//! The other assembly in this crate (`parts::prepare` → `parts::assemble`) wires
//! L1 capabilities into a kernel [`Agent`] with hand-written Rust: a fixed chain,
//! in a fixed order, decided at compile time. This one mounts a plexus tree
//! instead — the same capabilities, named as rows in a config tree.
//!
//! Nothing above changes. `CodingRuntimeHandle` talks to its engine through
//! exactly one channel pair (8 `AgentCommand`s in, 25 `AgentEvent`s out), and the
//! harness's `agent-handle` row hands out the very same
//! [`atomcode_kernel::agent::AgentHandle`] that `Agent::spawn()` does. The swap is
//! a swap, not an adaptation.
//!
//! Why bother, when the chain already works: a chain written in Rust can only be
//! changed by editing Rust. A row list can be changed by editing the list — which
//! is what "one engine, several products" needs, and what this crate's assembly
//! cannot offer today.
//!
//! Status: the tree mounts and is driven by the differential rig beside the
//! hand-written chain. It is NOT yet what `build_coding_agent` returns.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::provider::LlmProvider;
use atomcode_plexus::{App, ConfigTree, Context, Layer, Plugin};

/// The rows this product adds on top of the harness `base` assembly.
///
/// Deliberately not a profile in `atomcode-harness`: that crate's `PROFILES`
/// table says product specializations stay out of it, "what stops this crate
/// from being the place four products quietly fork". A coding assembly is a
/// product, so the list lives with the product.
///
/// `{working_dir}` is substituted by [`coding_overlay`].
const CODING_ROWS: &str = r#"
# Approval gates that the hand-written chain mounts as kernel `ToolMiddleware`s.
# Same judgements — each row calls the same L1 function the middleware does —
# reached through the `approval` seam instead of a private `PermissionStore`.
[[insert]]
name = "tool-open-file-workspace"
config = { working_dir = "{working_dir}" }

[[insert]]
name = "tool-credential-shell"

[[insert]]
name = "tool-write-approval"
config = { working_dir = "{working_dir}" }

[[insert]]
name = "tool-bash-workspace"
config = { working_dir = "{working_dir}" }

# An oversized tool result is stored whole and shown head + tail.
[[insert]]
name = "tool-output-artifact"
config = { dir = "{artifacts}" }

# The driver protocol: this is what `CodingRuntimeHandle` drives.
[[insert]]
name = "ui-handle"
"#;

/// The coding overlay with this working directory substituted in.
pub fn coding_overlay(working_dir: &Path, artifacts: &Path) -> String {
    CODING_ROWS
        .replace("{working_dir}", &working_dir.to_string_lossy())
        .replace("{artifacts}", &artifacts.to_string_lossy())
}

/// Put a caller-supplied provider into the tree's `llm` seam.
///
/// The row is named so a patch can address it; the provider itself comes from
/// the caller because choosing one is the host's business, not a row's.
struct InjectProvider(Arc<dyn LlmProvider>);

#[async_trait]
impl Plugin for InjectProvider {
    fn name(&self) -> &'static str {
        "llm-injected"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn description(&self) -> &'static str {
        "the provider the host constructed, handed to the tree"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Mount a coding assembly on the harness and take its driver handle.
///
/// Returns the handle AND the `App`, because the tree must outlive the handle:
/// dropping the `App` unloads every row, and the next command would reach a
/// conversation whose services are gone.
pub async fn mount(
    working_dir: &Path,
    provider: Arc<dyn LlmProvider>,
    extra_layers: &[&str],
) -> Result<(AgentHandle, App), String> {
    let artifacts = working_dir.join(".atomcode").join("artifacts");
    let scoped = format!(
        "[[patch]]\nid = \"fs\"\nconfig = {{ root = {wd:?} }}\n\n\
         [[patch]]\nid = \"agent-loop\"\nconfig = {{ working_dir = {wd:?} }}\n\n\
         [[patch]]\nid = \"llm\"\nname = \"llm-injected\"\nconfig = {{}}\n",
        wd = working_dir.to_string_lossy(),
    );
    let mut layers = vec![atomcode_harness::bundle::base().map_err(|e| e.to_string())?];
    for src in [
        scoped.as_str(),
        coding_overlay(working_dir, &artifacts).as_str(),
    ] {
        layers.push(Layer::from_toml(src).map_err(|e| e.to_string())?);
    }
    for src in extra_layers {
        layers.push(Layer::from_toml(src).map_err(|e| e.to_string())?);
    }
    let tree = ConfigTree::from_layers(layers).map_err(|e| e.to_string())?;

    // The five gate rows are not in `plugins::catalog()` yet — the file that
    // registers rows is being rewritten by another line of work. Registering them
    // here is also where they belong by the rule quoted above: rows a product's
    // assembly contributes, mounted from the same catalog. When the catalog is
    // free, the generic ones move into it and these lines go.
    let mut registry = atomcode_harness::plugins::catalog();
    use atomcode_harness::plugins::policy;
    registry.register(Arc::new(policy::OpenFileWorkspacePlugin));
    registry.register(Arc::new(policy::CredentialShellPlugin));
    registry.register(Arc::new(policy::WriteApprovalPlugin));
    registry.register(Arc::new(policy::BashWorkspacePlugin));
    registry.register(Arc::new(policy::OutputArtifactPlugin));
    registry.register(Arc::new(InjectProvider(provider)));

    let mut app = App::new(registry, tree);
    app.start().await.map_err(|e| e.to_string())?;
    let handle = app
        .context()
        .service::<atomcode_harness::seams::AgentHandleSvc>()
        .ok_or("the `ui-handle` row must provide a handle")?
        .take()
        .ok_or("the handle, once")?;
    Ok((handle, app))
}
