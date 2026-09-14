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

/// Whether a person is at this agent.
///
/// One rule, two places it shows up. A call that reaches outside the workspace is
/// a question when someone can answer it and a refusal when nobody can — so the
/// same assembly fences by root in one mode and leaves the boundary to approval
/// in the other. Getting this backwards is how a headless run quietly
/// auto-approves itself, and how an attended one quietly loses the ability to
/// touch a file next door.
///
/// It is not a new mechanism: `world.rs` already says "no root, no fence", and
/// the `approval` row already ships in a never-asks flavour. This names WHICH to
/// pick, in one place, instead of leaving it to whoever writes the next overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// A person is here (a UI, a terminal, a driver that answers). Work outside
    /// the workspace is ASKED about: no fence, `approval` left to the front end.
    Attended,
    /// Nobody is here — a subagent, a team child, a headless run. Work outside
    /// the workspace is REFUSED, by fencing the fs world to the working dir.
    ///
    /// The fence is the part this overlay owns. "Never ask" is the front end's:
    /// mounting `ui-handle` means a driver is present by definition, so a truly
    /// unattended assembly picks a different front end and keeps base's
    /// `deny-risky` row. A prompt nobody answers is an auto-approval wearing a
    /// question mark, and the front end is where that is decided.
    Headless,
}

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
    presence: Presence,
    provider: Arc<dyn LlmProvider>,
    extra_layers: &[&str],
) -> Result<(AgentHandle, App), String> {
    let artifacts = working_dir.join(".atomcode").join("artifacts");
    // The two halves of one rule. Attended: no `root`, so the fs world is not
    // fenced and a target next door reaches `tool-write-approval`, which asks.
    // Headless: fenced, and the `approval` row stays the `deny-risky` one that
    // refuses without asking — there is nobody to ask.
    //
    // Disabling `approval` is what lets `ui-handle` claim the seam and round-trip
    // the driver, which reads backwards until you notice the row being disabled
    // is the policy that never asks.
    let boundary = match presence {
        Presence::Attended => format!(
            "[[patch]]\nid = \"fs\"\nconfig = {{}}\n\n\
             [[patch]]\nid = \"approval\"\ndisabled = true\n",
        ),
        // The FENCE is what this mode enforces. "Never ask" is the front end's
        // business, not this overlay's: mounting `ui-handle` at all means a driver
        // is present, so an assembly that wanted nobody-to-ask would pick a
        // different front end and leave base's `deny-risky` row in place. Here the
        // row is still disabled — otherwise it and `ui-handle` fight over the
        // `approval` seam — and the boundary is held by the root instead.
        Presence::Headless => format!(
            "[[patch]]\nid = \"fs\"\nconfig = {{ root = {wd:?} }}\n\n\
             [[patch]]\nid = \"approval\"\ndisabled = true\n",
            wd = working_dir.to_string_lossy(),
        ),
    };
    let scoped = format!(
        "{boundary}\n\
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
