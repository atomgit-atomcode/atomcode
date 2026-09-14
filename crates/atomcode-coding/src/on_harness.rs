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
/// `{working_dir}`, `{artifacts}` and `{force_verify}` are substituted by
/// [`coding_overlay`].
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

# How the model is told about skills. The generic `skills` row advertises a
# count and a pointer; coding lists the catalog, because every other piece of
# its skill steering refers to that catalog by name.
[[insert]]
name = "skill-catalog-inline"

# …and a weak model is told to read it before doing anything else.
[[insert]]
name = "skill-first"

# The per-turn execution boundary the person states in prose ("do not run any
# command"). Registered outermost on tools/execute, above every approval gate:
# an Allow short-circuits everything downstream, and this must survive that.
[[insert]]
name = "execution-policy"

# Coding's own persona, in place of the harness's generic one. `{model}` is
# rewritten by a `/model` patch so this row remounts with it.
[[insert]]
name = "persona-atomcode"
config = { model = "{model}" }

# The self-correction loop: an in-workspace code edit the model walked away
# from without checking gets one nudge. `force` follows presence, the same rule
# `CodingAgentConfig::is_attended` applies to `VerifyCadenceHook`: a person who
# is watching can ask for the check themselves.
[[insert]]
name = "verify-cadence"
config = { working_dir = "{working_dir}", force = {force_verify} }

# The driver protocol: this is what `CodingRuntimeHandle` drives.
[[insert]]
name = "ui-handle"

# ...and because a driver is rendering, the tree must not also write to the
# terminal. `trace` is on in `base` because the harness binary has no other
# front end; here it produced every assistant token TWICE — once from `trace`'s
# own `print!`, once from the driver — and a headless run answered "pongpong".
#
# Found by running it, not by the rig: the differential patches `trace` off for
# quiet output, and in doing so masked this exactly.
[[patch]]
id = "trace"
disabled = true
"#;

/// The coding overlay with this working directory substituted in.
pub fn coding_overlay(
    working_dir: &Path,
    artifacts: &Path,
    presence: Presence,
    model: &str,
) -> String {
    CODING_ROWS
        .replace("{working_dir}", &working_dir.to_string_lossy())
        .replace("{artifacts}", &artifacts.to_string_lossy())
        .replace("{model}", model)
        .replace(
            "{force_verify}",
            // Same rule as the fence above, read the other way round: with
            // nobody watching, the agent has to be its own reviewer.
            match presence {
                Presence::Attended => "false",
                Presence::Headless => "true",
            },
        )
}

/// Providers the host has built, addressable from a config row by id.
///
/// The indirection is what makes `/model` an ordinary config change. Building a
/// provider is the host's business — it needs auth, the CodingPlan account,
/// subagent tiers, vision detection — so the row must not do it. But a row that
/// CAPTURES one can never be given another: the plugin instance holds it, and
/// patching the row's config would remount the same captured value.
///
/// So the host keeps the table and the row keeps an id. Putting a different
/// provider behind the `llm` seam is then: add it to the table, patch the row's
/// `provider_id`. `App::patch` unloads and remounts a row whose config changed,
/// the row looks up the new id, and the next turn resolves it — `agent-loop`
/// reads `LlmSvc` per turn, so nothing has to be told.
///
/// The agent survives that remount. Rows are sibling fibers under
/// `ROOT_FIBER`, and `Fibers::unload` cascades to CHILDREN, not to consumers —
/// so unloading `llm` leaves `agent-loop` and `ui-handle` running. That is the
/// whole reason a model swap here does not have to rebuild anything.
pub struct ProviderSlots {
    slots: std::sync::RwLock<std::collections::HashMap<String, Arc<dyn LlmProvider>>>,
    next: std::sync::atomic::AtomicU64,
}

impl ProviderSlots {
    /// A table holding one provider, and the id the `llm` row should name.
    pub fn new(initial: Arc<dyn LlmProvider>) -> (Arc<Self>, String) {
        let table = Arc::new(Self {
            slots: std::sync::RwLock::new(std::collections::HashMap::new()),
            next: std::sync::atomic::AtomicU64::new(0),
        });
        let id = table.insert(initial);
        (table, id)
    }

    /// Add a provider and return the id a row can name it by.
    ///
    /// Ids are never reused: a patch only remounts a row whose config CHANGED,
    /// so reusing an id would make a swap a no-op.
    pub fn insert(&self, provider: Arc<dyn LlmProvider>) -> String {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("gen-{n}");
        self.slots
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), provider);
        id
    }

    fn get(&self, id: &str) -> Option<Arc<dyn LlmProvider>> {
        self.slots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }
}

/// Put a newly built provider behind the `llm` seam.
///
/// Returns once the row has remounted, so the caller can rely on the next turn
/// using it. Between the unload and the remount `llm` is absent; a turn started
/// in that window would fail with "no llm provider", which is why the runtime
/// does this only while no turn is active.
/// `model` is the name the CONFIG asked for, not `next.model_name()`. A provider
/// factory may hand back something whose self-reported name does not match what
/// was requested — the test factories do exactly that — and the persona's
/// identity line should say what the person chose.
pub async fn swap_provider(
    app: &mut App,
    slots: &ProviderSlots,
    next: Arc<dyn LlmProvider>,
    model: &str,
) -> Result<(), String> {
    let id = slots.insert(next);
    // Both rows, one patch. The persona bakes the model into its identity line,
    // so a swap that moved only `llm` would leave the model reading a first
    // line that names the model it used to be.
    let layer = Layer::from_toml(&format!(
        "[[patch]]\nid = \"llm\"\nconfig = {{ provider_id = {id:?} }}\n\n\
         [[patch]]\nid = \"persona-atomcode\"\nconfig = {{ model = {model:?} }}\n"
    ))
    .map_err(|e| e.to_string())?;
    app.patch(&layer).await.map_err(|e| e.to_string())
}

/// Hands the tree whichever provider its config names.
struct InjectProvider(Arc<ProviderSlots>);

#[derive(serde::Deserialize)]
struct LlmRow {
    provider_id: String,
}

#[async_trait]
impl Plugin for InjectProvider {
    fn name(&self) -> &'static str {
        "llm-injected"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm"]
    }
    fn description(&self) -> &'static str {
        "the provider the host built, named by id so a patch can change it"
    }
    async fn apply(&self, ctx: &Context, config: &serde_json::Value) -> Result<(), String> {
        let row: LlmRow =
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?;
        let provider = self
            .0
            .get(&row.provider_id)
            .ok_or_else(|| format!("no provider registered as `{}`", row.provider_id))?;
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmSvc>(provider)
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
    let (handle, app, _) = mount_swappable(working_dir, presence, provider, extra_layers).await?;
    Ok((handle, app))
}

/// As [`mount`], but the caller keeps the provider table.
///
/// Holding it is what makes a later `/model` a patch rather than a rebuild —
/// see [`ProviderSlots`] and [`swap_provider`]. A caller that never switches
/// models can use [`mount`] and ignore it.
pub async fn mount_swappable(
    working_dir: &Path,
    presence: Presence,
    provider: Arc<dyn LlmProvider>,
    extra_layers: &[&str],
) -> Result<(AgentHandle, App, Arc<ProviderSlots>), String> {
    let model = provider.model_name().to_string();
    let (providers, provider_id) = ProviderSlots::new(provider);
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
        Presence::Attended => "[[patch]]\nid = \"fs\"\nconfig = {}\n\n\
             [[patch]]\nid = \"approval\"\ndisabled = true\n"
            .to_string(),
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
         [[patch]]\nid = \"llm\"\nname = \"llm-injected\"\nconfig = {{ provider_id = {pid:?} }}\n",
        wd = working_dir.to_string_lossy(),
        pid = provider_id,
    );
    let mut layers = vec![atomcode_harness::bundle::base().map_err(|e| e.to_string())?];
    for src in [
        scoped.as_str(),
        coding_overlay(working_dir, &artifacts, presence, &model).as_str(),
    ] {
        layers.push(Layer::from_toml(src).map_err(|e| e.to_string())?);
    }
    for src in extra_layers {
        layers.push(Layer::from_toml(src).map_err(|e| e.to_string())?);
    }
    let tree = ConfigTree::from_layers(layers).map_err(|e| e.to_string())?;

    // The five approval gates now come from `plugins::catalog()` with everything
    // else: they ask through the `approval` seam and call the same L1 decision
    // functions the kernel middleware calls, so nothing about them is specific
    // to this product. Registering them here would have kept the harness binary
    // and every other assembly from ever mounting them.
    //
    // What IS registered here is what this crate owns: the coding discipline.
    let mut registry = atomcode_harness::plugins::catalog();
    registry.register(Arc::new(VerifyCadencePlugin));
    registry.register(Arc::new(CodingPersonaPlugin));
    registry.register(Arc::new(ExecutionPolicyPlugin));
    registry.register(Arc::new(SkillCatalogPlugin));
    registry.register(Arc::new(SkillFirstPlugin));
    // Registered but NOT in `CODING_ROWS`, and that is the whole design: for
    // this row, mounting IS enabling, and the datalog writes a full transcript
    // of every request to the user's disk. `datalog.enabled` defaults to false
    // in config.toml for the same reason. A host that wants it inserts the row.
    registry.register(Arc::new(DatalogPlugin));
    // Also registered but NOT in `CODING_ROWS`: this one runs the person's own
    // external commands. The row is inert without a `hooks.json`, but "inert"
    // is not the same as "mounted by default" — a host that wants CC hooks
    // inserts it, the way the chain mounts the engine only when one exists.
    registry.register(Arc::new(CcHooksPlugin));
    registry.register(Arc::new(InjectProvider(providers.clone())));

    let mut app = App::new(registry, tree);
    app.start().await.map_err(|e| e.to_string())?;
    let handle = app
        .context()
        .service::<atomcode_harness::seams::AgentHandleSvc>()
        .ok_or("the `ui-handle` row must provide a handle")?
        .take()
        .ok_or("the handle, once")?;
    Ok((handle, app, providers))
}

// ---- the verify cadence, as a row ---------------------------------------
//
// One of the three things this crate says it owns (lib.rs: assembly, persona,
// discipline). In the hand-written chain it is `VerifyCadenceHook`, sitting on
// the kernel's `offer_continuation`. The harness has no such hook — but it has
// the two halves the discipline actually needs, and they are already how the
// `truncation-recovery` row keeps a turn alive:
//
//   1. `agent/request` to see the round that just finished, and
//   2. the agent's inbox, whose `has_waking_input()` is exactly what the loop
//      re-reads before deciding the turn is over.
//
// A message queued with `MessageOrigin::Harness` is logged as
// `InjectionOrigin::Continuation` and rendered as a SYNTHETIC user message —
// the same thing `offer_continuation` produces, which is why
// `verify_reminder_already_present` and `current_real_user_start` keep working
// against it unchanged.
//
// The differential found this: the chain ran a third model call after an
// unverified edit and the row list stopped at two. Nothing else in the scenario
// list edits a file and then walks away, so it was invisible until asked.

/// Asks the model to check code it edited and did not verify.
struct VerifyCadence {
    ctx: Context,
    workspace: std::path::PathBuf,
    /// The edit already nudged for, so one edit is asked about once.
    nudged: std::sync::Mutex<Option<crate::discipline::NudgedEdit>>,
}

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for VerifyCadence {
    async fn handle(
        &self,
        req: &mut atomcode_harness::events::ModelRequest,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
    ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
    {
        let response = next.run(req).await?;
        // A round that asked for tools is not finished, and the loop will carry
        // on by itself. The cadence is about the round where the model says it
        // is done.
        if !response.tool_calls.is_empty() || response.truncated {
            return Ok(response);
        }
        // A user who said "don't run tests" is not asking to be nudged into
        // running them. The hook consults the same policy; a row that skipped
        // this would override the person it is supposed to be standing in for.
        if crate::execution_policy::execution_policy_for_messages(&req.messages)
            .skips_verification()
        {
            return Ok(response);
        }
        // `req.messages` is what the model was shown: the edit and its result
        // are both in there, which is the whole history the judgement needs.
        let Some(edit) = crate::discipline::unverified_edit(&req.messages, &self.workspace) else {
            return Ok(response);
        };
        {
            let mut nudged = self.nudged.lock().unwrap_or_else(|e| e.into_inner());
            if nudged.as_ref() == Some(&edit) {
                // Already asked about this exact edit. Asking again would spend
                // the budget on the same answer; let the turn stop.
                return Ok(response);
            }
            *nudged = Some(edit);
        }
        let Some(agent) = atomcode_harness::agent::scoped(&self.ctx)
            .service::<atomcode_harness::seams::SessionSvc>()
            .and_then(|session| {
                let id = session.id().to_string();
                self.ctx
                    .service::<atomcode_harness::seams::AgentsSvc>()
                    .and_then(|agents| agents.by_session(&id))
            })
        else {
            return Ok(response);
        };
        // A message, not an injection: an injection is context that rides along
        // with the next message and never wakes anything, which is precisely
        // the difference between a note and a continuation.
        agent.inbox().send_from(
            crate::discipline::NUDGE,
            atomcode_harness::agent::MessageOrigin::Harness,
        );
        Ok(response)
    }
}

/// Mounts the verify cadence.
pub struct VerifyCadencePlugin;

#[derive(serde::Deserialize)]
struct VerifyCadenceRow {
    #[serde(default)]
    working_dir: String,
    /// Whether to FORCE the check. Off when a person is attending: they see the
    /// edit and can ask for the check themselves, which is the same rule
    /// `CodingAgentConfig::is_attended` applies to the hook.
    #[serde(default)]
    force: bool,
}

#[async_trait]
impl Plugin for VerifyCadencePlugin {
    fn name(&self) -> &'static str {
        "verify-cadence"
    }
    fn description(&self) -> &'static str {
        "ask the model to check code it edited and walked away from"
    }
    async fn apply(&self, ctx: &Context, config: &serde_json::Value) -> Result<(), String> {
        let row: VerifyCadenceRow = if config.is_null() {
            VerifyCadenceRow {
                working_dir: String::new(),
                force: false,
            }
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        if !row.force {
            return Ok(());
        }
        // Outermost, so the round it judges is the SETTLED one: a rate-limit
        // wait, a retry, an overflow trim and a truncation resume all happen
        // inside this, and a cadence that fired on an intermediate response
        // would nudge about an answer the model never finished giving.
        let _ = ctx.on_waterfall::<atomcode_harness::events::AgentRequest>(
            Arc::new(VerifyCadence {
                ctx: ctx.clone(),
                workspace: std::path::PathBuf::from(row.working_dir),
                nudged: std::sync::Mutex::new(None),
            }),
            true,
        );
        Ok(())
    }
}

// ---- the per-turn execution boundary, as a row --------------------------
//
// The production chain registers `TurnExecutionPolicy` BEFORE every middleware
// that can `Allow`, and the comment there says why: an `Allow` short-circuits
// everything downstream, so a boundary the person set must not be something an
// approval gate can wave through. The row keeps that property by sitting
// outermost on `tools/execute` — a later gate's `Allow` never reaches it,
// because it never delegates.
//
// The differential found this one too, and starkly: the chain refused the call
// before the tool started, while the row list ran the command.
//
// Two halves, one shared handle, because the halves learn and enforce at
// different moments:
//   - `agent/request`, before delegating: re-read the restriction from the
//     messages. It is stated in prose mid-conversation, so nothing in the tree
//     configuration can express it and every round has to look again.
//   - `tools/execute`, outermost: refuse what the restriction forbids.

struct ExecutionBoundary {
    policy: Arc<crate::execution_policy::TurnExecutionPolicy>,
}

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for ExecutionBoundary {
    async fn handle(
        &self,
        req: &mut atomcode_harness::events::ModelRequest,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
    ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
    {
        // BEFORE delegating: the calls this round produces are gated on what
        // the person said, and they are gated by the other half below, which
        // has no messages of its own to read.
        self.policy.update_from_messages(&req.messages);
        next.run(req).await
    }
}

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::ToolsExecuteBatch> for ExecutionBoundary {
    async fn handle(
        &self,
        batch: &mut atomcode_harness::events::ToolBatch,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::ToolsExecuteBatch>,
    ) -> Vec<atomcode_kernel::tool::ToolResult> {
        // The BATCH seam, not `tools/execute`, for the same reason
        // `RefuseTruncatedCalls` uses it: a call that must not run should be
        // held back before a scheduler picks it up, not refused once it is
        // already in flight.
        //
        // It does NOT close the `ToolStarted` gap — that was this row's first
        // theory and it is wrong. `ui-handle` announces every MOUNTED tool as
        // started the moment the assistant message is logged, which is earlier
        // than either tool seam, so a refused call still reads as "started" to a
        // driver. That is a general property of the two engines rather than
        // anything this row does: the plain approval path shows it too. See
        // `a_refused_call_still_reads_as_started_on_the_harness` in the
        // differential, which owns the finding.
        //
        // Deliberately NOT consulting `pre_approved`. Every other gate does,
        // because approval is something a person can grant; this is the
        // person's own restriction, and "already approved" is precisely the
        // short-circuit it exists to survive.
        let policy = self.policy.current();
        let blocked: Vec<usize> = batch
            .calls
            .iter()
            .enumerate()
            .filter(|(_, call)| {
                crate::execution_policy::blocks_call(policy, &call.name, &call.arguments)
            })
            .map(|(i, _)| i)
            .collect();
        if blocked.is_empty() {
            return next.run(batch).await;
        }

        // Hold back the forbidden ones and let the rest through: a round that
        // also contained allowed calls should not lose them.
        //
        // Denying is returning a result, not raising: the model must see why
        // its call did not run, and the history has to stay pairable.
        let refused: Vec<(usize, atomcode_kernel::tool::ToolResult)> = blocked
            .iter()
            .map(|&i| {
                (
                    i,
                    atomcode_kernel::tool::ToolResult {
                        call_id: batch.calls[i].id.clone(),
                        content: crate::execution_policy::BLOCKED.to_string(),
                        is_error: true,
                        images: vec![],
                    },
                )
            })
            .collect();
        batch.calls = batch
            .calls
            .iter()
            .enumerate()
            .filter(|(i, _)| !blocked.contains(i))
            .map(|(_, call)| call.clone())
            .collect();
        let mut results = if batch.calls.is_empty() {
            Vec::new()
        } else {
            next.run(batch).await
        };
        // Back where the model expects them, so results still line up with the
        // calls it emitted.
        for (index, result) in refused {
            let at = index.min(results.len());
            results.insert(at, result);
        }
        results
    }
}

/// Mounts the per-turn execution boundary.
pub struct ExecutionPolicyPlugin;

#[async_trait]
impl Plugin for ExecutionPolicyPlugin {
    fn name(&self) -> &'static str {
        "execution-policy"
    }
    fn description(&self) -> &'static str {
        "refuse what the person forbade for this turn, above any approval"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let boundary = Arc::new(ExecutionBoundary {
            policy: Arc::new(crate::execution_policy::TurnExecutionPolicy::new()),
        });
        let _ = ctx.on_waterfall::<atomcode_harness::events::AgentRequest>(boundary.clone(), true);
        // Outermost, which is this row's whole point: a gate that returns
        // `Allow` never delegates further, so anything registered inside it
        // would be skipped by the very short-circuit the boundary must survive.
        let _ = ctx.on_waterfall::<atomcode_harness::events::ToolsExecuteBatch>(boundary, true);
        Ok(())
    }
}

// ---- how the model is told about skills ---------------------------------
//
// The bigger of the two findings here, and it is not a hook at all. The generic
// harness `skills` row advertises a POINTER — "1 skill(s) are available. Call
// `list_skills` to see them" — which is the right default for a harness that
// cannot know how many skills a deployment installs: a catalog costs tokens on
// every single request.
//
// Coding inlines the whole catalog, names and descriptions, and has a reason on
// the record: registering `use_skill` without telling the model what exists made
// skills "basically never fire" (see `skills::catalog_hook`). Every other piece
// of coding's skill steering — the persona line, the `use_skill` description,
// and the skill-first nudge below — refers to "the `=== AVAILABLE SKILLS ===`
// catalog above". Without it they point at nothing.
//
// So this row is a product overriding a harness default, which is what a product
// row is for. It contributes under the SAME fragment id the generic row uses,
// replacing rather than appending: two descriptions of the same thing in one
// system prompt is worse than either alone.

/// Fragment id and rank of the generic `skills` advertisement, which this
/// replaces. Same id is the mechanism (`PromptRegistry::contribute` retains by
/// id), and it is deliberate rather than incidental.
const SKILLS_FRAGMENT: (&str, i32) = ("skills", 60);

/// Puts the full skill catalog in the system prompt.
pub struct SkillCatalogPlugin;

#[async_trait]
impl Plugin for SkillCatalogPlugin {
    fn name(&self) -> &'static str {
        "skill-catalog-inline"
    }
    fn description(&self) -> &'static str {
        "list the installed skills in the system prompt, not just their count"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let Some(skills) = ctx.service::<atomcode_harness::seams::SkillsSvc>() else {
            return Ok(());
        };
        // No skills → leave the generic row's judgement alone. It already says
        // nothing when the count is zero, and a catalog header over an empty
        // list is worse than silence.
        let Some(catalog) = skills.render_catalog() else {
            return Ok(());
        };
        let Some(prompts) = ctx.service::<atomcode_harness::seams::SystemPromptSvc>() else {
            return Ok(());
        };
        let (id, rank) = SKILLS_FRAGMENT;
        prompts.contribute(id, rank, catalog);
        Ok(())
    }
}

/// Tells a weak model to check the catalog before it does anything else.
struct SkillFirst {
    ctx: Context,
}

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for SkillFirst {
    async fn handle(
        &self,
        req: &mut atomcode_harness::events::ModelRequest,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
    ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
    {
        // Opening turn only, and round 1 of it: the reminder has to land before
        // the model's first action, not after it has already gone exploring.
        // `ModelRequest` carries both numbers, so this is the same condition the
        // hook writes as `ctx.turn_id != 1 || ctx.round != 1`.
        if req.turn != 1 || req.round != 1 {
            return next.run(req).await;
        }
        let enabled = self
            .ctx
            .service::<atomcode_harness::seams::LlmSvc>()
            .is_some_and(|llm| crate::persona::model_needs_firm_execution(llm.model_name()))
            && self
                .ctx
                .service::<atomcode_harness::seams::SkillsSvc>()
                .is_some_and(|skills| !skills.is_empty());
        if !enabled {
            return next.run(req).await;
        }
        // Appended to the REQUEST, not committed to the log: it is ephemeral
        // steering for one round, and a log full of nudges is a log nobody can
        // read. The loop's "model-visible content is logged" invariant is
        // checked on the assembled messages before the request is built, so a
        // tail added here is outside it by construction.
        req.messages
            .push(atomcode_capabilities::reminder::synthetic_system_reminder(
                crate::skill_first::SKILL_FIRST_BODY,
            ));
        next.run(req).await
    }
}

/// Mounts the opening-turn skill-first reminder.
pub struct SkillFirstPlugin;

#[async_trait]
impl Plugin for SkillFirstPlugin {
    fn name(&self) -> &'static str {
        "skill-first"
    }
    fn description(&self) -> &'static str {
        "make a weak model check the skill catalog before it starts exploring"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        // Innermost (`prepend = false`): the tail should be the last thing added
        // before the provider sees the request, so recency — the whole point of
        // putting it at the tail — is not spent by something appending after it.
        let _ = ctx.on_waterfall::<atomcode_harness::events::AgentRequest>(
            Arc::new(SkillFirst { ctx: ctx.clone() }),
            false,
        );
        Ok(())
    }
}

// ---- the datalog --------------------------------------------------------
//
// Not a session-log listener, which is the first thing to try and the wrong
// answer. The session log holds FACTS — a user message, an assistant message, a
// tool result — and the datalog's JSONL record is the ASSEMBLED REQUEST: the
// system prompt exactly as sent, every message, the tools, the options, the
// cache epoch. That is what a prompt-cache or a "did the model actually see it"
// investigation reads, and it exists nowhere else in the system.
//
// So it needs `agent/request`, plus three cheap listeners for the turn's shape.
// Four seams here against six kernel hook methods there, and the sink between
// them is the same object.

struct Datalog {
    sink: Arc<atomcode_capabilities::datalog::DatalogHook>,
}

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for Datalog {
    async fn handle(
        &self,
        req: &mut atomcode_harness::events::ModelRequest,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
    ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
    {
        // Innermost by registration, so the record is the request the PROVIDER
        // saw: a retry, an overflow trim or a skill-first tail all happen
        // outside this, and a record taken before them would describe a request
        // that was never sent. Being wrong in that direction is worse than
        // useless — it is a log that lies while looking authoritative.
        let ctx = atomcode_kernel::hook::TurnCtx {
            turn_id: req.turn,
            round: req.round,
            request_id: req.round as u64,
            ..Default::default()
        };
        self.sink
            .record_request(&req.messages, &req.tools, &req.options, &ctx)
            .await;
        let answered = next.run(req).await;
        match &answered {
            Ok(response) => {
                let mut message = atomcode_kernel::message::Message::assistant(
                    response.text.clone(),
                    response.tool_calls.clone(),
                );
                if !response.reasoning.is_empty() {
                    message.reasoning = Some(response.reasoning.clone());
                }
                self.sink.record_response(&message);
            }
            Err(error) => self.sink.record_error(&error.message),
        }
        answered
    }
}

/// Writes a per-turn markdown transcript and one JSONL record per round.
pub struct DatalogPlugin;

#[derive(serde::Deserialize, Default)]
struct DatalogRow {
    #[serde(default)]
    working_dir: String,
    /// Root the per-project directory is created under. Same meaning as
    /// `datalog.dir` in config.toml, including `~` and the relative form.
    #[serde(default)]
    dir: Option<String>,
}

#[async_trait]
impl Plugin for DatalogPlugin {
    fn name(&self) -> &'static str {
        "datalog"
    }
    fn description(&self) -> &'static str {
        "write every request, response and tool result to a per-turn transcript"
    }
    async fn apply(&self, ctx: &Context, config: &serde_json::Value) -> Result<(), String> {
        let row: DatalogRow = if config.is_null() {
            DatalogRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        // Mounting the row IS enabling it — there is no `enabled = false` here,
        // because a row you do not want is a row you do not insert. The config
        // flag exists in `config.toml` for a chain that is always assembled.
        let cfg = atomcode_config::config::DatalogConfig {
            enabled: true,
            dir: row.dir,
        };
        let llm = ctx.service::<atomcode_harness::seams::LlmSvc>();
        let model = llm
            .as_ref()
            .map(|l| l.model_name().to_string())
            .unwrap_or_default();
        let context_window = llm.as_ref().map(|l| l.context_window()).unwrap_or(0);
        let Some(sink) = atomcode_capabilities::datalog::DatalogHook::new(
            std::path::PathBuf::from(row.working_dir),
            &cfg,
            model,
            context_window,
        ) else {
            return Ok(());
        };
        let sink = Arc::new(sink);

        // A turn opened, and with what. `TurnStarted` carries the prompt, which
        // is the one thing `user_prompt_submit` was for.
        let opened = sink.clone();
        let _ = ctx.on_emit::<atomcode_harness::events::TurnStart>(
            move |started: &atomcode_harness::events::TurnStarted| {
                opened.begin_turn(&started.prompt);
            },
        );
        // Tool results, in the order they land. These listeners are SYNC, which
        // is why `record_tool_result` was made sync: spawning a task per result
        // would interleave the markdown and produce a transcript in an order
        // nothing actually happened in.
        let tooled = sink.clone();
        let _ = ctx.on_emit::<atomcode_harness::events::ToolResultEvent>(
            move |result: &atomcode_kernel::tool::ToolResult| {
                tooled.record_tool_result(result);
            },
        );
        let ended = sink.clone();
        let _ = ctx.on_emit::<atomcode_harness::events::TurnEnd>(
            move |outcome: &atomcode_harness::seams::TurnOutcome| {
                if let Some(error) = &outcome.error {
                    ended.record_error(error);
                }
                ended.finish_turn_named(&format!("{:?}", outcome.stop));
                // The stats are written by the line above; only the WAIT is
                // spawned. `emit` dispatches sync listeners, so there is nowhere
                // here to await — and what would be awaited is a flush, not a
                // write, so nothing is lost but the timing of the fsync.
                let flushing = ended.clone();
                tokio::spawn(async move { flushing.flush().await });
            },
        );
        let _ = ctx.on_waterfall::<atomcode_harness::events::AgentRequest>(
            Arc::new(Datalog { sink }),
            false,
        );
        Ok(())
    }
}

// ---- the user's own hooks -----------------------------------------------
//
// `hooks.json`: external commands the PERSON configured, run at CC-compatible
// moments. Inert unless they wrote one — `CCExternalHooks::load_with_extra`
// returns an empty engine and the chain does not mount it.
//
// Most of it needed no extraction at all. `user_prompt_submit`, `session_start`
// and `turn_complete` are already neutral — they take a `&mut String`, a
// `&mut Conversation`, a `&StopReason` — so the row calls the trait methods
// directly. Only the PreToolUse fold had to be split, and for exactly one
// reason: resolving a hook's `ask` into a real prompt goes through `RequestCtx`
// in the chain and through the `approval` seam here.
//
// The seam map, four against six:
//   agent/pre-step  → session_start (once) + user_prompt_submit
//   tools/execute   → pre_tool_gate → [ask] → post_tool
//   turn/end        → turn_complete   (observation only, so spawning is faithful)
//   (session_end has no harness moment yet; see the row's doc comment)

/// Forces an approval prompt for one call, and refuses to remember the answer.
///
/// A hook-forced `ask` is deliberately NOT grantable: "always" would turn the
/// person's explicit "stop and ask me about this" into a one-time question. The
/// chain says the same thing by having no grant store on that path.
struct CcAsk(Arc<dyn atomcode_kernel::tool::Tool>);

#[async_trait]
impl atomcode_kernel::tool::Tool for CcAsk {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn description(&self) -> &str {
        self.0.description()
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.0.parameters_schema()
    }
    fn risk(&self, _args: &str) -> atomcode_kernel::tool::RiskLevel {
        // Risky whatever the tool normally is: the hook asked for a prompt, and
        // a Safe tool would skip straight past one.
        atomcode_kernel::tool::RiskLevel::Risky
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        atomcode_harness::seams::NEVER_GRANT.to_string()
    }
    async fn execute(
        &self,
        args: &str,
        ctx: &atomcode_kernel::tool::ToolContext,
    ) -> atomcode_kernel::tool::ToolResult {
        self.0.execute(args, ctx).await
    }
}

struct CcHooks {
    ctx: Context,
    engine: Arc<atomcode_capabilities::cc_hooks::CCExternalHooks>,
    /// SessionStart runs once, lazily, on the first step.
    ///
    /// Not on `AgentCreated`: that listener is sync, so the hooks would have to
    /// be spawned, and a spawned SessionStart can land AFTER the first request —
    /// which is the one thing its context must precede. Here it is awaited
    /// inside the step that is about to run.
    started: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::PreStep> for CcHooks {
    async fn handle(
        &self,
        decision: &mut atomcode_harness::events::StepDecision,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::PreStep>,
    ) -> atomcode_harness::events::StepDecision {
        use atomcode_kernel::hook::LifecycleHooks;
        if !self.started.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let mut convo = atomcode_kernel::message::Conversation::new();
            self.engine.session_start(&mut convo, false).await;
            for message in convo.messages {
                decision.injections.push((
                    message.text,
                    atomcode_harness::session::InjectionOrigin::Reminder,
                ));
            }
        }
        if let Some(text) = &mut decision.message {
            if let Err(reason) = self.engine.user_prompt_submit(text).await {
                // The person's hook said no. `rejected` ends the turn without a
                // step, which is what "block this prompt" means here.
                decision.rejected = Some(reason);
                return decision.clone();
            }
        }
        next.run(decision).await
    }
}

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::ToolsExecute> for CcHooks {
    async fn handle(
        &self,
        exec: &mut atomcode_harness::events::ToolExec,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::ToolsExecute>,
    ) -> atomcode_kernel::tool::ToolResult {
        use atomcode_kernel::middleware::{AfterOutcome, BeforeOutcome};
        let call_id = exec.call.id.clone();
        let refuse = move |reason: String| atomcode_kernel::tool::ToolResult {
            call_id,
            content: reason,
            is_error: true,
            images: vec![],
        };
        // The fold may REWRITE the arguments (CC `updatedInput`), so it works on
        // the live call rather than a copy.
        let mut gate = self.engine.pre_tool_gate(&mut exec.call).await;
        if matches!(gate, BeforeOutcome::Ask { .. }) {
            gate = match (
                self.ctx.service::<atomcode_harness::seams::ApprovalSvc>(),
                self.ctx
                    .service::<atomcode_harness::seams::ToolsSvc>()
                    .and_then(|t| t.get(&exec.call.name)),
            ) {
                (Some(policy), Some(tool)) => {
                    let asking: Arc<dyn atomcode_kernel::tool::Tool> = Arc::new(CcAsk(tool));
                    match policy.decide(&exec.call, &asking).await {
                        atomcode_harness::seams::Decision::Allow => {
                            BeforeOutcome::Allow { reason: None }
                        }
                        atomcode_harness::seams::Decision::Deny(reason) => {
                            BeforeOutcome::Deny { reason }
                        }
                    }
                }
                // Nobody to ask. The chain fails CLOSED here for the same reason:
                // a forced ask that silently becomes a yes is worse than a
                // refusal, because the person asked to be stopped.
                _ => BeforeOutcome::Deny {
                    reason: format!("hook asked for approval, nobody to ask: {}", exec.call.name),
                },
            };
        }
        self.engine.note_call_for_post(&exec.call, &gate);
        match gate {
            BeforeOutcome::Deny { reason } | BeforeOutcome::DenyTurn { reason } => {
                return refuse(reason)
            }
            // An explicit hook `allow` short-circuits the downstream gates, which
            // is the point of saying it.
            BeforeOutcome::Allow { .. } => exec.pre_approved = true,
            _ => {}
        }
        let mut result = next.run(exec).await;
        if let AfterOutcome::Block { reason } = self.engine.post_tool(&mut result).await {
            // PostToolUse `block` speaks TO THE MODEL after the fact; the call
            // already ran, so this appends rather than replacing the result.
            result.content.push_str("\n\n");
            result.content.push_str(&reason);
            result.is_error = true;
        }
        result
    }
}

/// Runs the user's `hooks.json` at the CC-compatible moments.
pub struct CcHooksPlugin;

#[derive(serde::Deserialize, Default)]
struct CcHooksRow {
    #[serde(default)]
    working_dir: String,
    /// Stamped into every CC payload as `session_id`, so a hook can correlate.
    #[serde(default)]
    session_id: String,
}

#[async_trait]
impl Plugin for CcHooksPlugin {
    fn name(&self) -> &'static str {
        "cc-hooks"
    }
    fn description(&self) -> &'static str {
        "run the user's hooks.json at the Claude-Code-compatible moments"
    }
    async fn apply(&self, ctx: &Context, config: &serde_json::Value) -> Result<(), String> {
        let row: CcHooksRow = if config.is_null() {
            CcHooksRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let working_dir = std::path::PathBuf::from(&row.working_dir);
        let mut engine = atomcode_capabilities::cc_hooks::CCExternalHooks::load_with_extra(
            &working_dir,
            Vec::new(),
        );
        if !row.session_id.is_empty() {
            engine = engine.with_session_id(&row.session_id);
        }
        // No hooks configured → register nothing at all, so the no-hooks path
        // costs exactly what it costs in the chain: nothing.
        if engine.is_empty() {
            return Ok(());
        }
        let hooks = Arc::new(CcHooks {
            ctx: ctx.clone(),
            engine: Arc::new(engine),
            started: std::sync::atomic::AtomicBool::new(false),
        });
        let _ = ctx.on_waterfall::<atomcode_harness::events::PreStep>(hooks.clone(), false);
        // Prepended: the person's own hook decides before any built-in gate gets
        // to auto-approve the call out from under it. The chain registers it in
        // the same position, ahead of the workspace and approval gates.
        let _ = ctx.on_waterfall::<atomcode_harness::events::ToolsExecute>(hooks.clone(), true);
        let ended = hooks.clone();
        let _ = ctx.on_emit::<atomcode_harness::events::TurnEnd>(
            move |outcome: &atomcode_harness::seams::TurnOutcome| {
                use atomcode_harness::seams::StopReason as Harness;
                // The one branch CC actually reads off this value is Stop vs
                // StopFailure, and `stop_is_failure` keys on a provider/stream
                // failure — so that is the distinction worth carrying across two
                // enums neither crate owns.
                let reason = match outcome.stop {
                    Harness::ProviderError => atomcode_kernel::event::StopReason::ProviderError,
                    _ => atomcode_kernel::event::StopReason::Stopped,
                };
                let engine = ended.engine.clone();
                // Spawned, and faithfully so: `turn_complete` is documented
                // "observation only — fire all matching hooks and ignore output",
                // so nothing downstream is waiting on the answer.
                tokio::spawn(async move {
                    use atomcode_kernel::hook::LifecycleHooks;
                    engine
                        .turn_complete(
                            &atomcode_kernel::message::Conversation::new(),
                            &reason,
                            &atomcode_kernel::hook::TurnCtx::default(),
                        )
                        .await;
                });
            },
        );
        Ok(())
    }
}

// ---- the persona --------------------------------------------------------
//
// `lib.rs` says this crate owns three things: assembly, PERSONA, discipline.
// The discipline came across as `verify-cadence`; the persona had not, and
// nothing noticed — the harness's own `persona-coding` row was filling the slot
// with entirely different words:
//
//   coding   "You are AtomCode, an AI coding agent by AtomGit running the {model} model…"
//   harness  "You are a coding agent working in a real repository…"
//
// The differential could not see it: `normalise` records a `Snapshot` as the
// LIST OF ROLES, never the text, so two engines can send the model completely
// different instructions and compare equal. It surfaced only when `/model`
// stopped falling back to the chain — the test that asserts the persona had
// been passing because the fallback rebuilt a chain agent, persona and all.

/// Contributes coding's own persona, displacing the generic one.
///
/// Same fragment id as the harness row on purpose: two personas in one system
/// prompt is worse than either.
pub struct CodingPersonaPlugin;

#[derive(serde::Deserialize, Default)]
struct PersonaRow {
    /// The model the identity line names. Carried in config rather than read
    /// from the `llm` seam so that a `/model` patch, which rewrites it, also
    /// remounts this row — a persona still naming the old model would be a
    /// quiet lie in the first line the model reads.
    #[serde(default)]
    model: String,
}

#[async_trait]
impl Plugin for CodingPersonaPlugin {
    fn name(&self) -> &'static str {
        "persona-atomcode"
    }
    fn uses(&self) -> &'static [&'static str] {
        // The tool catalog decides what the persona may promise: it describes
        // `todowrite` and `ask_user` only when they are actually mounted.
        &["tools", "system-prompt"]
    }
    fn description(&self) -> &'static str {
        "coding's own persona, in place of the harness's generic one"
    }
    async fn apply(&self, ctx: &Context, config: &serde_json::Value) -> Result<(), String> {
        let row: PersonaRow = if config.is_null() {
            PersonaRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let tools = ctx.service::<atomcode_harness::seams::ToolsSvc>();
        let has = |name: &str| tools.as_ref().is_some_and(|t| t.get(name).is_some());
        let text = crate::persona::coding_persona(&row.model, has("todowrite"), has("ask_user"));
        let Some(prompts) = ctx.service::<atomcode_harness::seams::SystemPromptSvc>() else {
            return Ok(());
        };
        // Rank 0 and the generic row's id: the identity line goes first, and
        // there is only ever one of it.
        prompts.contribute("persona-coding", 0, text);
        Ok(())
    }
}
