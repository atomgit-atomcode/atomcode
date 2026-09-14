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
"#;

/// The coding overlay with this working directory substituted in.
pub fn coding_overlay(working_dir: &Path, artifacts: &Path, presence: Presence) -> String {
    CODING_ROWS
        .replace("{working_dir}", &working_dir.to_string_lossy())
        .replace("{artifacts}", &artifacts.to_string_lossy())
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
         [[patch]]\nid = \"llm\"\nname = \"llm-injected\"\nconfig = {{}}\n",
        wd = working_dir.to_string_lossy(),
    );
    let mut layers = vec![atomcode_harness::bundle::base().map_err(|e| e.to_string())?];
    for src in [
        scoped.as_str(),
        coding_overlay(working_dir, &artifacts, presence).as_str(),
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
    registry.register(Arc::new(VerifyCadencePlugin));
    registry.register(Arc::new(ExecutionPolicyPlugin));
    registry.register(Arc::new(SkillCatalogPlugin));
    registry.register(Arc::new(SkillFirstPlugin));
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
