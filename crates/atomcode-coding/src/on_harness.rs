//! Coding, as a row list.
//!
//! This is the product's assembly: the capabilities named as rows in a config
//! tree, mounted as a plexus app. It replaced a hand-written chain — the same
//! capabilities wired into a kernel `Agent` in a fixed order, decided at compile
//! time — and the reason for the change is what a list can do that Rust cannot:
//! a chain is changed by editing Rust and rebuilding, a list by editing the list.
//! That is what "one engine, several products" needs.
//!
//! Nothing above it changed in the swap. `CodingRuntimeHandle` talks to its engine
//! through exactly one channel pair (8 `AgentCommand`s in, 25 `AgentEvent`s out),
//! and the harness's `agent-handle` row hands out the very same
//! [`atomcode_kernel::agent::AgentHandle`] `Agent::spawn()` did.
//!
//! What the chain did in each scenario is kept: `tests/golden/differential/*.json`
//! records its event streams, and `tests/differential.rs` runs this tree against
//! that record under a ratchet. `tests/runtime_criteria.rs` holds the promises
//! measured across turns, each one falsified once before it was kept.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::agent::AgentHandle;
use atomcode_kernel::provider::LlmProvider;
use atomcode_plexus::{App, ConfigTree, Context, Entry, Layer, Plugin};

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

/// What this agent can DO, said by this product instead of inherited.
///
/// The harness ships its own answer (`bundle::DEFAULTS`) and until now this
/// assembly took it and patched the disagreements. That reads fine and is wrong
/// in one specific way: the harness is generic, so every row it cannot be sure
/// about ships OFF — no ast-grep toolchain assumed, no whole-repo graph paid
/// for, no network reached. Abstention is the right default THERE and the wrong
/// one here, because a product does not experience an inherited abstention as a
/// question. It experiences it as a capability that used to work, and finds out
/// one support question at a time — which is exactly how `ast_grep`, the code
/// graph and `open_file` were found missing, each by a person, none by a test.
///
/// So this is not a diff against base. It is the whole answer, and a row added
/// to `bundle::DEFAULTS` tomorrow does not silently arrive here.
/// [`atomcode_harness::bundle::INFRA`] — the registries, the session log, the
/// loop, the recovery policies — is still taken whole, because none of that is
/// a product decision: there is no version of a coding agent that wants a
/// different `tool-args-repair`.
///
/// Read it against `parts::PrepareOptions`, which is where the hand-written
/// chain says the same things in Rust.
///
/// Public because this is the product decision, stated once. [`mount_swappable`]
/// stacks it for a host that drives the agent through `ui-handle`; a host that
/// brings its own front end (the full-screen TUI) stacks the same const rather
/// than keeping a second copy of this list, which is how two products come to
/// disagree about what a coding agent can do.
pub const CODING_DEFAULTS: &str = r#"
# --- how long a turn may run ----------------------------------------------
#
# `infra` ships `round-cap` at `max_rounds = 24`, so every tree that says
# nothing inherits a 24-round turn. The engine this assembly stands in for does
# not cap turns at all: its equivalent knob defaults to `0`, documented as
# "`0` = unbounded" (`atomcode-coding/src/config.rs`,
# `default_turn_max_rounds`), and honored by
# `if cfg.max_rounds != 0 { builder.max_rounds(…) }` in `parts.rs` and
# `assemble.rs` alike.
#
# So the mapping is not "pick a number" — it is stating the same policy the
# engine states, in the unit this row counts. Leaving `infra`'s 24 in place made
# this assembly a *different agent* from the one the same codebase runs
# everywhere else, and the difference was invisible: a row can be reconfigured
# by the **absence** of a patch.
#
# Here rather than in a host's own bundle, because both hosts stack this list —
# `product.rs` (the full-screen TUI) and `mount_swappable` (a driver holding an
# `AgentHandle`). Putting it in one of them is how the two came to disagree:
# round 1 of this fix did exactly that, and the harness-side host kept stopping
# at 24.
#
# `0` is written literally, so a deployment that wants a fuse sets it in a layer
# of its own (`--patch`, or `harness.patch.toml`) — both land after this one.
# `RoundCap::call` guards with `max_rounds > 0` so `0` means "no limit" rather
# than "stop before the first request".
#
# NOT mapped, and worth knowing: `[coding].max_rounds` /
# `ATOMCODE_TURN_MAX_ROUNDS` do not reach this row. Those are read where the
# engine builds its loop (`config.rs`), which a config *layer* cannot do — a
# layer is data. Bridging them means a plugin that reads the environment and
# patches this row, which does not exist yet; until it does, a deployment that
# sets them gets the engine's cap on the chain path and none here.
#
# ALSO NOT mapped, deliberately, and this one is a divergence rather than a gap:
# `agent-loop` carries a second, coarser stop — `infra` sets
# `config = { max_rounds = 100 }`, and `agent_loop.rs` ends the turn with
# `StopReason::RunawayFuse` at that round. The engine's counterpart is
# `cfg.max_rounds` → the kernel fuse, whose comment reads "`0` leaves the
# neutral kernel fuse unwired" — i.e. the engine ships it **off** and relies on
# the repetition guards, which is what this assembly's `tool-loop-guard` +
# `repeat-fuse` rows are.
#
# Matching that here means setting the fuse to `0`, and that is two changes, not
# one: `agent_loop.rs` compares `step >= self.max_rounds`, so a bare `0` stops
# the turn before its first request (the same trap `round-cap` had), and
# `Op::Patch` **replaces a row's whole config** — so a host that patches
# `agent-loop` with just its `working_dir` (both hosts do) would drop the field
# straight back to the default of 100. Doing it half-way would produce a fuse
# that reads 0 in a `--dump-config` whose host has already reverted it.
#
# Left as is on purpose: the fuse is a safety net, removing one is its own
# decision, and the divergence is documented here rather than silently made.
[[patch]]
id = "round-cap"
config = { max_rounds = 0 }

# --- where a session's log goes ---------------------------------------------
# Into the session store, under the lease of the session the runtime opened:
# the host inserts `session-store` for that (see `session_store`). The harness's
# own JSONL row would write a second copy beside the store under no lease, and a
# tree with no session binding keeps nothing. The host swaps `session-store`
# into this row's place, which is ahead of the agent that resumes from it.
[[patch]]
id = "session-persistence-jsonl"
disabled = true

# --- what the model can do to the repository ------------------------------
# All routed through the execution world (`fs`/`shell`), so the fence and the
# approval seam apply to every one of them rather than to whoever remembered.
[[insert]]
name = "tool-fs-world"

[[insert]]
name = "tool-search-world"

# Structural search. Base leaves it off because a generic harness cannot assume
# an ast-grep toolchain; a coding agent assumes one.
[[insert]]
name = "tool-ast-grep"

[[insert]]
name = "tool-bash-world"

# The local implementations of the same three, mounted dormant. They claim the
# same tool names as the world-routed rows above, so enabling one means
# disabling its twin — the catalog refuses a duplicate rather than silently
# picking. Kept in the list so a host that wants the unrouted ones has a row to
# patch instead of a crate to fork.
[[insert]]
name = "tool-fs"
disabled = true

[[insert]]
name = "tool-search"
disabled = true

[[insert]]
name = "tool-bash"
disabled = true

# --- what the model can do that is not the repository ----------------------
[[insert]]
name = "skills"

[[insert]]
name = "codeintel"

# The graph layer builds and caches a whole-repo index. Base leaves it off
# because a generic run should not pay for one; this product's whole job is
# cross-file reasoning, so it pays.
[[insert]]
name = "code-graph"

# Reaches the public internet. Off in base as a deliberate choice; on here,
# because the chain has had `web: true` in `PrepareOptions::default()` since
# before this tree existed and every shipped driver leaves it on.
#
# No `provider` set, which means the `ATOMCODE_WEB_SEARCH_PROVIDER` env knob and
# then the tool's default — the same order the chain resolves, minus
# `config.toml`'s `web_search_provider`, which this row cannot see. In offline
# mode the row mounts nothing, so the catalog matches what the persona says.
[[insert]]
name = "tool-web"

# The model catalog, and the two rows that only make sense with one.
#
# Off in the list and switched on by `mount_swappable` when the host actually
# hands one over — a tree mounted without a catalog (the differential's plain
# `mount`, an embedder that only has one provider) would otherwise wait forever
# for a seam nobody fills. They are here rather than inserted from Rust because
# this list is meant to be the whole answer: a reader should see that this
# product has a catalog, and that the side-call model comes out of it.
[[insert]]
name = "models-host"
disabled = true

# The side-call model: titles, summaries, the `simple` team roles. Unset picks
# the weakest model on offer, which is what a side call wants by definition.
[[insert]]
name = "llm-utility-selected"
disabled = true

# One line telling the model that `task` and `team` take a `model` id, and where
# to look. It says nothing countable on purpose — see the row's own docs.
# Harmless without a catalog: it reads the seam rather than waiting on it.
[[insert]]
name = "model-catalog"

# Reviewing the current changes in-session. On, because the chain has had
# `review: true` in `PrepareOptions::default()` from the start and every shipped
# driver leaves it on — `/review` is a product feature, not an extra.
#
# `model` is patched by `swap_provider` beside the persona: this row holds the
# provider it was given at mount, and `App::patch` remounts only rows whose own
# entry changed. Without that patch the reviewer would keep talking to the model
# the person just switched away from.
[[insert]]
name = "tool-code-review"

[[insert]]
name = "memory"

# Searching the log the harness already writes. Separate from `memory`: memory
# is what the user chose to state, recall is everything that was said.
[[insert]]
name = "recall"

[[insert]]
name = "tool-todo"

# The list only helps while it is true, and a tool description is the furthest
# thing in the prompt from the step being taken.
[[insert]]
name = "todo-reminder"

# Asking is a capability, not a manner. Without a tool for it the agent has two
# moves when a decision is the person's — guess, or stop — and it guesses,
# because guessing looks like progress.
[[insert]]
name = "tool-ask"

# Delegation, both halves of it. Every shipped driver passes
# `SubagentPolicy::Enabled`, so the chain has always mounted `task` and `team`;
# base leaves them off because a generic tree should opt into multiplying model
# calls. A person asked why this engine had no subagent, and the answer was
# that it inherited an abstention — which is this whole list's reason to exist.
#
# `task` runs a child in its own realm with a reduced tool set and hands back
# only the answer. `team` is the other shape: named members with roles that
# stay, report back, and can be told more.
[[insert]]
name = "subagent-in-process"

[[insert]]
name = "team-in-process"

# External MCP servers are other people's processes. On in the chain
# (`mcp: true`), off here for the same reason the rig patches it off on both
# sides: connecting them is a side effect, and this row has not been driven
# through a real server on this engine yet.
[[insert]]
name = "mcp"
disabled = true

# --- who the agent is, and what it may do without asking -------------------
# `persona-coding`, the harness's generic identity row, is deliberately absent:
# `persona-atomcode` in `CODING_ROWS` fills that slot in this product's own
# words, and two personas in one system prompt is worse than either.

# What a delegated agent (a team member, a `task` child) may never do, refused
# ahead of every rule and question: nobody is watching it call tools.
[[insert]]
name = "delegation-bounds"

# No rules by default, so the row is inert until a user writes some.
[[insert]]
name = "permissions"

# A read-only tool never asks, so it could read ~/.ssh or .env unasked. This
# row asks the approval seam as if such a call were risky.
[[insert]]
name = "sensitive-paths"
config = { allow = [], deny = [] }

# Base's never-asks policy row, off in both presences. `ui-handle` claims the
# `approval` seam and round-trips the question to the driver; one provider per
# seam, so leaving this enabled would make the tree fail to start. It stays in
# the list, disabled, because an assembly with no front end wants exactly this
# row back — and then it is a patch, not a fork.
[[insert]]
name = "approval"
config = { mode = "deny-risky" }
disabled = true

# The interactive alternative to the row above: same seam, but it asks the
# terminal directly. This product asks through its driver instead.
[[insert]]
name = "approval-interactive"
disabled = true

# Read-only exploration. Off by default; `--plan` turns it on.
[[insert]]
name = "plan-mode"
disabled = true

# The person's own `hooks.json`. Here, disabled, only to hold its place: after
# the hard boundaries, ahead of the gates that auto-approve — so a PreToolUse
# hook sees a write before a convenience gate waves it through, the position
# the chain gives it. Mounting it is enabling it (it runs the person's
# commands), so a host that found hooks patches it on.
[[insert]]
name = "cc-hooks"
disabled = true

# On in base because the harness binary has no other front end. Off here, and
# not as a nicety: with a driver rendering, this row printed every assistant
# token a SECOND time, and a headless run answered "pongpong".
[[insert]]
name = "trace"
config = { stream = true, tools = true, summary = true }
disabled = true

# Names the session as soon as the first prompt lands, in the background.
[[insert]]
name = "session-title-on-first-prompt"

# Dormant, and must stay so: `ui-handle` claims `user-questions` when it mounts,
# and an incumbent here makes the tree fail to start. A tree with no front end
# at all wants this row back, the same way it wants `approval` back.
[[insert]]
name = "user-questions-unattended"
disabled = true

[[insert]]
name = "telemetry"
disabled = true
"#;

// ---- the shapes this product patches rows with ------------------------------
//
// One struct per config this crate computes, serialized by the layer builder
// rather than formatted into TOML. Two things that used to be a matter of
// remembering are now the compiler's:
//
//   - a value the person wrote reaches the tree as a value, not as text that
//     has to survive `{:?}` and come back as legal TOML (a path with a control
//     character, a `NaN` temperature);
//   - a patch that means to carry a field has a field to carry, so dropping one
//     is a missing initializer rather than a missing substring.
//
// They are deliberately NOT the harness's own row structs: those are private,
// and making them public would turn every row's schema into an API this crate
// pins. These say what THIS product sends, which is the thing worth typing.

#[derive(serde::Serialize)]
struct AgentLoopPatch<'a> {
    working_dir: &'a std::path::Path,
}

#[derive(serde::Serialize)]
struct LlmInjectedRow<'a> {
    provider_id: &'a str,
}

#[derive(serde::Serialize)]
struct FsWorldPatch<'a> {
    /// `None` is the attended world: no fence, because there is someone to ask.
    #[serde(skip_serializing_if = "Option::is_none")]
    root: Option<&'a std::path::Path>,
}

#[derive(serde::Serialize)]
struct DatalogPatch<'a> {
    working_dir: &'a std::path::Path,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<&'a str>,
}

/// The rows that are this product, on top of [`CODING_DEFAULTS`].
///
/// The split between the two lists is: `CODING_DEFAULTS` answers the same
/// questions the harness answers for itself (which tools, which stance), so it
/// reads as a list of decisions. This one is what no generic harness has a
/// version of — the approval gates the chain expresses as kernel middleware,
/// coding's persona, its verify cadence, its skill steering, and the driver
/// protocol the runtime talks to.
///
/// Deliberately not a profile in `atomcode-harness`: that crate's `PROFILES`
/// table says product specializations stay out of it, "what stops this crate
/// from being the place four products quietly fork". A coding assembly is a
/// product, so the list lives with the product.
///
/// `{working_dir}`, `{artifacts}` and `{force_verify}` are substituted by
/// [`coding_overlay`].
///
/// Public for the same reason [`CODING_DEFAULTS`] is: a host with its own front
/// end stacks this list instead of restating it.
pub const CODING_ROWS: &str = r#"
# Approval gates that the chain mounted as kernel `ToolMiddleware`s.
# Same judgements — each row calls the same L1 function the middleware does —
# reached through the `approval` seam instead of a private `PermissionStore`.
[[insert]]
name = "tool-open-file-workspace"
config = { working_dir = {working_dir} }

[[insert]]
name = "tool-credential-shell"

[[insert]]
name = "tool-write-approval"
config = { working_dir = {working_dir} }

[[insert]]
name = "tool-bash-workspace"
config = { working_dir = {working_dir} }

# An oversized tool result is stored whole and shown head + tail.
[[insert]]
name = "tool-output-artifact"
config = { dir = {artifacts} }

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

# `open_file` and the thing that can open one. `opener-local` rides in the repl
# and tui bundles rather than `base`, because a harness with nobody at a display
# has nowhere to open anything — but this assembly is driven through `ui-handle`,
# which means a person IS there.
[[insert]]
name = "opener-local"

[[insert]]
name = "tool-open-file"

# Coding's own persona, in place of the harness's generic one. `{model}` is
# rewritten by a `/model` patch so this row remounts with it.
[[insert]]
name = "persona-atomcode"
config = { model = {model} }

# The self-correction loop: an in-workspace code edit the model walked away
# from without checking gets one nudge. `force` follows presence, the same rule
# `CodingAgentConfig::is_attended` applies to `VerifyCadenceHook`: a person who
# is watching can ask for the check themselves.
[[insert]]
name = "verify-cadence"
config = { working_dir = {working_dir}, force = {force_verify} }

# Request options the person configured (max tokens, temperature, reasoning
# effort), on every request that does not state its own. Empty config says
# nothing, so a default config sends what it always sent.
[[insert]]
name = "chat-options"

# The driver protocol: this is what `CodingRuntimeHandle` drives.
[[insert]]
name = "ui-handle"
"#;

/// What a `CodingAgentConfig` has to say to the rows.
///
/// The chain reads the person's `config.toml` straight off that struct, field by
/// field, at the point each capability is built. The tree reads its knobs from
/// row config instead, so somebody has to carry the values across — and this is
/// that somebody, in one place, rather than a field remembered at one call site
/// and forgotten at the next.
///
/// Deliberately short, and deliberately not a mapping of the whole struct: a
/// field belongs here once its row exists and can use it. `web_search_provider`
/// is the first, and it is here because turning `tool-web` on is what revealed
/// that the row had no way to be told which backend to use — the person's
/// `[web_search] provider = "duckduckgo"` would have been read by the chain and
/// silently dropped by the tree.
/// The `persona-atomcode` row's config for a model and a language.
#[derive(serde::Serialize)]
struct PersonaPatch<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<atomcode_config::locale::Locale>,
}

fn persona_config(
    model: &str,
    language: Option<atomcode_config::locale::Locale>,
) -> PersonaPatch<'_> {
    PersonaPatch { model, language }
}

/// Rows whose config follows the running model — patched at mount and again on
/// every model switch, because each one is a fact about the model.
/// What `llm-retry` carries in `bundle::INFRA`, restated here because a swap to
/// a provider with no opinion has to actively put it back.
const DEFAULT_RETRY_ATTEMPTS: u32 = 3;

#[derive(serde::Serialize)]
struct CodeReviewModelPatch<'a> {
    model: &'a str,
}

#[derive(serde::Serialize)]
struct FsVisionPatch {
    vision: bool,
}

#[derive(serde::Serialize)]
struct RateLimitPatch<'a> {
    base_url: &'a str,
    // Carried, not omitted: `Op::Patch` replaces a row's config wholesale, so a
    // field this struct does not have is a field the row loses. Naming them here
    // is what keeps `bundle::INFRA`'s values. `no_row_silently_loses_a_configured_field`
    // fails if base grows one this struct lacks; a base that changes one of these
    // VALUES is the gap no test closes — keep them in step by hand.
    max_waits: u32,
    max_wait_secs: u64,
    fallback_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_attempts: Option<u32>,
}

#[derive(serde::Serialize)]
struct ChatOptionsPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<atomcode_kernel::provider::ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(serde::Serialize)]
struct RetryPatch {
    attempts: u32,
    backoff_ms: u64,
    cap_ms: u64,
}

/// Rows whose config follows the running model — patched at mount and again on
/// every model switch, because each one is a fact about the model.
fn model_rows(cfg: &crate::CodingAgentConfig) -> Result<Layer, String> {
    // Retries. An explicit upstream budget is the visible tier and wins; an
    // explicit per-model adapter budget otherwise switches the outer tier off,
    // the coupling the chain applies. Attempts count the first request.
    //
    // Always emitted, never conditional: a swap TO a provider that says nothing
    // has to put the default back, and a patch that is simply absent leaves the
    // previous provider's budget in place. Criterion:
    // `the_retry_budget_follows_a_model_switch`.
    let attempts = match (cfg.upstream_retry_max_attempts, cfg.retry_max_attempts) {
        (Some(n), _) => n.saturating_add(1),
        (None, Some(_)) => 1,
        (None, None) => DEFAULT_RETRY_ATTEMPTS,
    };
    Layer::new()
        .patch(
            "tool-fs-world",
            FsVisionPatch {
                vision: cfg.supports_vision,
            },
        )
        .map_err(|e| e.to_string())?
        // A 429 is judged against the endpoint the request went to: whether it
        // is the CodingPlan gateway (whose windows say when to come back)
        // follows the model.
        .swap("llm-rate-limit", "rate-limit-coding")
        .patch(
            "llm-rate-limit",
            RateLimitPatch {
                base_url: &cfg.base_url,
                max_waits: 5,
                max_wait_secs: 120,
                fallback_secs: 5,
                max_attempts: cfg.retry_max_attempts,
            },
        )
        .map_err(|e| e.to_string())?
        .patch(
            "chat-options",
            ChatOptionsPatch {
                reasoning_effort: cfg.chat_options.reasoning_effort,
                max_tokens: cfg.chat_options.max_tokens,
                // A non-finite temperature is dropped HERE, on purpose and where
                // it can be read. `nan` is legal in `config.toml` and has no JSON
                // number to become: left alone it serializes as `null`, which the
                // row reads as "not set" anyway — but as an accident of serde
                // rather than a decision. Saying it here means the next person
                // sees a rule instead of a surprise.
                temperature: cfg.chat_options.temperature.filter(|t| t.is_finite()),
            },
        )
        .map_err(|e| e.to_string())?
        // Retries belong HERE rather than in `config_rows` because
        // `retry_max_attempts` is the PROVIDER's (`ProviderConfig`): a `/model`
        // that moves the conversation to another provider moves whatever that
        // one said about retrying, exactly as the 429 judgement above does.
        .patch(
            "llm-retry",
            RetryPatch {
                attempts,
                backoff_ms: 3000,
                cap_ms: 30000,
            },
        )
        .map_err(|e| e.to_string())
}

#[derive(serde::Serialize)]
struct TruncationPatch {
    max_continuations: u32,
    checkpoint: bool,
}

#[derive(serde::Serialize)]
struct RoundCapPatch {
    max_rounds: u32,
    max_seconds: u64,
    checkpoint: bool,
}

#[derive(serde::Serialize)]
struct LoopGuardPatch {
    warn_after: u32,
    stop_after: u32,
}

#[derive(serde::Serialize)]
struct CompactionTailPatch {
    threshold: f32,
    /// Carried because a patch replaces the whole config and base sets it. Only
    /// the model-free row reads it: `compaction-coding`, which the product swaps
    /// in, keeps recent turns by token budget.
    keep_turns: u32,
}

#[derive(serde::Serialize)]
struct AskTimeoutPatch {
    ask_timeout_secs: u64,
}

#[derive(serde::Serialize)]
struct CredentialShellPatch<'a> {
    policy: &'a str,
}

/// Never an `api_key`: row config is printable data. The key reaches the row as
/// a plugin instance instead — see `HostState::web_search_api_key`.
#[derive(serde::Serialize)]
struct WebProviderPatch<'a> {
    provider: &'a str,
}

/// Everything in `config.toml` that a row can carry, as a layer.
///
/// Empty for a default config, so the tree reads the same as if this were not
/// here at all.
pub fn config_rows(cfg: &crate::CodingAgentConfig) -> Result<Layer, String> {
    use atomcode_capabilities::tools::CredentialShellPolicy;

    let mut out = model_rows(cfg)?;
    if cfg.preferred_language.is_some() {
        out = out
            .patch(
                "persona-atomcode",
                persona_config(&cfg.model, cfg.preferred_language),
            )
            .map_err(|e| e.to_string())?;
    }
    // With the checkpoint, a front end that draws the question is asked before a
    // turn is cut off by its budget or ends with its answer cut off.
    if cfg.round_cap_checkpoint {
        out = out
            .patch(
                "truncation-recovery",
                TruncationPatch {
                    max_continuations: 4,
                    checkpoint: true,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    // A turn's own round budget; `0` is unbounded, which `CODING_DEFAULTS` states.
    if cfg.max_rounds != 0 {
        out = out
            .patch(
                "round-cap",
                RoundCapPatch {
                    max_rounds: cfg.max_rounds,
                    max_seconds: 0,
                    checkpoint: cfg.round_cap_checkpoint,
                },
            )
            .map_err(|e| e.to_string())?;
    }
    // The exact-repeat guard. `None` is the person turning it off.
    out = match cfg.tool_loop_policy {
        Some(policy) => out
            .patch(
                "tool-loop-guard",
                LoopGuardPatch {
                    warn_after: policy.warning_threshold(),
                    stop_after: policy.stop_threshold(),
                },
            )
            .map_err(|e| e.to_string())?,
        None => out.disable("tool-loop-guard"),
    };
    out = out
        .patch(
            "compaction-tail",
            CompactionTailPatch {
                threshold: cfg.compact_threshold,
                keep_turns: 2,
            },
        )
        .map_err(|e| e.to_string())?
        // How long a question to the person waits. `0` parks until answered.
        .patch(
            "ui-handle",
            AskTimeoutPatch {
                ask_timeout_secs: cfg.request_timeout.map(|d| d.as_secs().max(1)).unwrap_or(0),
            },
        )
        .map_err(|e| e.to_string())?;
    // `[coding] shell_guard_policy`. The row's own default is `prompt`, the same
    // as the L1 default, so only a person who chose otherwise changes the tree.
    let policy = match cfg.credential_shell_policy {
        CredentialShellPolicy::Off => Some("off"),
        CredentialShellPolicy::Prompt => None,
        CredentialShellPolicy::Strict => Some("strict"),
    };
    if let Some(policy) = policy {
        out = out
            .patch("tool-credential-shell", CredentialShellPatch { policy })
            .map_err(|e| e.to_string())?;
    }
    if let Some(provider) = cfg
        .web_search_provider
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        out = out
            .patch("tool-web", WebProviderPatch { provider })
            .map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// The coding overlay with this working directory substituted in.
pub fn coding_overlay(
    working_dir: &Path,
    artifacts: &Path,
    presence: Presence,
    model: &str,
) -> String {
    // Each placeholder stands **where its value goes**, not inside quotes of its
    // own — so what is substituted is [`toml_string`]'s whole output, quotes and
    // all.
    //
    // This was wrong once, in a way worth remembering: the placeholders used to
    // sit inside `"…"` and the substitution trimmed `toml_string`'s quotes to
    // fit. That works only by accident, because the serializer does not always
    // choose double quotes — for a value containing `"` or a backslash and no
    // `'`, it emits a **literal** string (`'/tmp/we"ird'`), and trimming that
    // leaves raw text to be spliced into `"…"`, which is to say no escaping at
    // all. The awkward paths stayed broken; only the values the serializer
    // happens to double-quote were fixed. Hence: whole output, placeholder
    // unquoted.
    //
    // `{force_verify}` below is the exception, and it is not a string: it is a
    // TOML **boolean** (`true`/`false`), so it goes in bare.
    CODING_ROWS
        .replace(
            "{working_dir}",
            &atomcode_harness::bundle::toml_string(&working_dir.to_string_lossy()),
        )
        .replace(
            "{artifacts}",
            &atomcode_harness::bundle::toml_string(&artifacts.to_string_lossy()),
        )
        .replace("{model}", &atomcode_harness::bundle::toml_string(model))
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
    ///
    /// **The table holds exactly one provider — the current one.** It started
    /// out accumulating every provider it was ever handed, which reads
    /// harmlessly (ids are small, and nothing looks up an old one) and was a
    /// hole: after `/logout` the credentialled provider was unreachable through
    /// the seam and still ALIVE in this map, which is precisely the thing a
    /// logout is supposed to end. Dropping the old entry here is what makes
    /// [`deactivate_provider`]'s claim true rather than nearly true.
    ///
    /// Safe because nobody resolves an old id: `llm-injected` looks its id up
    /// once at mount and then holds the `Arc` itself, and a row retried out of
    /// `App::pending` re-reads the tree, which by then names the new id.
    pub fn insert(&self, provider: Arc<dyn LlmProvider>) -> String {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!("gen-{n}");
        let mut slots = self.slots.write().unwrap_or_else(|e| e.into_inner());
        slots.clear();
        slots.insert(id.clone(), provider);
        id
    }

    /// The one provider this table holds: what the `llm` row is serving now.
    ///
    /// Well-defined precisely because [`Self::insert`] keeps exactly one entry.
    pub fn current(&self) -> Option<Arc<dyn LlmProvider>> {
        self.slots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .next()
            .cloned()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn LlmProvider>> {
        self.slots
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }
}

/// What the host knows about models, handed to the tree at mount.
///
/// Three things the tree cannot work out for itself: which models exist (the
/// person's `config.toml` plus whatever the gateway wrote there at login), how
/// to BUILD one (auth, account, signing — the host's provider factory), and
/// which one this conversation is on.
///
/// Deliberately the ingredients rather than a finished `Models`: the catalog
/// also needs the live `ProviderSlots`, which does not exist until the mount is
/// under way.
pub struct HostModels {
    pub config: Arc<atomcode_config::config::Config>,
    pub providers: Arc<crate::SubagentModelProviders>,
    /// The selection id this conversation runs on.
    pub current: String,
}

/// The `models` seam, answered from what the host already had.
///
/// Note how little is new here: `logical_models()` is the catalog the settings
/// UI and `/model` already read, and `SubagentModelProviders` is the lazily
/// building, telemetry-wrapping resolver the chain used for
/// `task` since before this tree existed — including being reset on `/model` by
/// `refresh_subagent_tiers`. **Both engines resolve a model selection through
/// the same code**, which is the only way the two stay honest about it.
struct CodingModels {
    host: HostModels,
    slots: Arc<ProviderSlots>,
}

#[async_trait]
impl atomcode_harness::seams::Models for CodingModels {
    fn list(&self) -> Vec<atomcode_harness::seams::ModelInfo> {
        self.host
            .config
            .logical_models()
            .into_iter()
            .map(|(id, m)| atomcode_harness::seams::ModelInfo {
                display_name: m.display_name.clone().unwrap_or_else(|| m.model.clone()),
                context_window: m.context_window,
                // Same rule `ProviderConfig::accepts_images` applies: an explicit
                // value wins, else the name heuristic. Reading it any other way
                // here would make the catalog disagree with what the provider
                // actually does.
                supports_vision: m
                    .supports_vision
                    .unwrap_or_else(|| atomcode_config::util::model_name_suggests_vision(&m.model)),
                capable_rank: m.capable_model,
                effort_levels: m.reasoning_effort_levels.clone().unwrap_or_default(),
                note: m.note.clone(),
                account: m.account.clone(),
                id,
            })
            .collect()
    }

    fn current(&self) -> Option<String> {
        Some(self.host.current.clone())
    }

    async fn provider(&self, id: &str) -> Result<Arc<dyn LlmProvider>, String> {
        // `get` answers `Ok(None)` for "that is the host's own model" — the
        // collapse path the chain uses too. Serving the live slot rather than
        // building a second provider for the same endpoint keeps one set of
        // credentials in the process, which is what `/logout` relies on.
        if id != self.host.current {
            if let Some(built) = self.host.providers.get(id)? {
                return Ok(built);
            }
        }
        self.slots
            .current()
            .ok_or_else(|| "no provider is behind the `llm` seam".to_string())
    }
}

/// Hands the tree the host's catalog.
struct InjectModels(Arc<dyn atomcode_harness::seams::Models>);

#[async_trait]
impl Plugin for InjectModels {
    fn name(&self) -> &'static str {
        "models-host"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["models"]
    }
    fn description(&self) -> &'static str {
        "the models this host can build a provider for"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::ModelsSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
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
    swap_provider_for(app, slots, next, model, None).await
}

/// As [`swap_provider`], carrying everything else that follows the model: the
/// persona's language, whether `read_file` may return pictures, and the request
/// options (reasoning effort, max tokens, temperature) the chain reads off the
/// new config when it reassembles.
pub async fn swap_provider_for(
    app: &mut App,
    slots: &ProviderSlots,
    next: Arc<dyn LlmProvider>,
    model: &str,
    config: Option<&crate::CodingAgentConfig>,
) -> Result<(), String> {
    // What the slot holds now, so a patch that does not land can be undone.
    // The slot is not bookkeeping: `CodingModels::provider` serves it to the
    // `models` seam, which is where a delegated child (`task`, `team`,
    // `code_review`) gets "the model this conversation is on". Swapping it
    // before the patch and leaving it there on failure puts the conversation on
    // one provider and its own children on another — with the model NAME still
    // reading as the old one, because that comes from the config the tree
    // mounted with.
    let previous = slots.current();
    let id = slots.insert(next);
    // Three rows, one patch, and the two extras are there for the same reason:
    // `App::patch` remounts only rows whose OWN entry changed, so a row that
    // captured something from the `llm` seam keeps what it captured.
    //
    //   persona-atomcode  bakes the model into its identity line — a swap that
    //                     moved only `llm` leaves the model reading a first line
    //                     that names the model it used to be.
    //   tool-code-review  holds the provider it hands the child reviewer — so
    //                     `/model` would move the conversation and leave the
    //                     reviewer on the old model, and `/logout` would leave
    //                     it holding the credentials. The logout criterion in
    //                     the differential fails the moment this line is gone.
    let restore = |error: String| {
        if let Some(previous) = previous.clone() {
            slots.insert(previous);
        }
        error
    };
    let mut layer = Layer::new()
        .patch("llm", LlmInjectedRow { provider_id: &id })
        .and_then(|layer| {
            layer.patch(
                "persona-atomcode",
                persona_config(model, config.and_then(|c| c.preferred_language)),
            )
        })
        .and_then(|layer| layer.patch("tool-code-review", CodeReviewModelPatch { model }))
        .map_err(|e| restore(e.to_string()))?;
    if let Some(config) = config {
        layer = layer.then(model_rows(config).map_err(&restore)?);
    }
    app.patch(&layer).await.map_err(|e| restore(e.to_string()))
}

/// What fills the `llm` seam after a logout.
///
/// Deactivating a provider is a SECURITY act — the credentials must stop living
/// in this process — and on the chain that means tearing the agent down, because
/// the provider is baked into the assembled chain. Here it does not: the
/// credentials live in the provider object, and the provider lives behind a
/// seam, so removing them is swapping what is behind it.
///
/// The seam cannot simply be emptied: rows that `inject` `llm` would be
/// unloaded with it. So it is filled with something that holds nothing and
/// refuses everything. In practice nothing calls it — the runtime marks itself
/// unavailable and rejects a turn before it reaches the agent — but a
/// placeholder that silently succeeded would be a much worse thing to be wrong
/// about than one that says why.
struct NoProvider;

#[async_trait]
impl LlmProvider for NoProvider {
    fn model_name(&self) -> &str {
        "(signed out)"
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
        Err(atomcode_kernel::stream::ProviderError {
            retryable: false,
            message: "signed out: no provider is configured".into(),
            http_status: None,
            code: None,
            retry_after_secs: None,
        })
    }
}

/// Take the credentials out of the tree without taking the agent with them.
///
/// The counterpart of [`swap_provider`], and the reason a logout does not have
/// to end a session on this engine: the agent, its conversation and its handle
/// all survive, and a later login is another swap.
/// A provider holding nothing and refusing everything: what stands in any slot a
/// logout empties.
pub(crate) fn signed_out_provider() -> Arc<dyn LlmProvider> {
    Arc::new(NoProvider)
}

pub async fn deactivate_provider(app: &mut App, slots: &ProviderSlots) -> Result<(), String> {
    swap_provider(app, slots, Arc::new(NoProvider), "(signed out)").await
}

/// Hands the tree whichever provider its config names.
/// The row that serves a provider the host already built, by id.
///
/// Public for a host composing its own tree — its own front end, its own layer
/// order — that still builds the provider itself, because building one takes
/// credentials, gateway signing and subagent tiers a tree must not handle.
/// Register one per [`ProviderSlots`] table and lay [`provider_layer`]; from
/// then on [`swap_provider`] and [`deactivate_provider`] work on that tree as
/// they do on a mounted one, provided coding's `persona-atomcode` and
/// `tool-code-review` rows are in it (disabled is fine) — a swap re-points those
/// two as well, and a patch cannot address a row no layer inserted.
pub struct InjectProvider(Arc<ProviderSlots>);

impl InjectProvider {
    pub fn new(slots: Arc<ProviderSlots>) -> Self {
        Self(slots)
    }
}

/// The layer that puts [`InjectProvider`] behind the `llm` seam, serving
/// `provider_id`. What [`mount_hosted`] lays for itself, for a host laying its
/// own.
pub fn provider_layer(provider_id: &str) -> Result<Layer, String> {
    Layer::new()
        .swap("llm", "llm-injected")
        .patch("llm", LlmInjectedRow { provider_id })
        .map_err(|e| e.to_string())
}

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
        // What replacing a row costs: the generic `llm` rows describe how a
        // model gets switched in THEIR product (an env var and a restart, or a
        // `--patch` file). This row displaced them and said nothing, so
        // `describe_self` had no entry for the one question people actually ask
        // — and the agent filled the gap by repeating the harness binary's
        // story, telling a user to edit config and restart when `/model` was
        // right there. Same failure as the persona: displacing a row means
        // inheriting what it was responsible for.
        let model = provider.model_name().to_string();
        atomcode_harness::plugins::self_knowledge::describes(
            ctx,
            "model",
            5,
            format!(
                "MODEL — this conversation runs `{model}`. The person switches it with \
                 `/model` (or `--model` at launch); it takes effect on the next turn and \
                 does NOT restart the session or lose the conversation. You cannot switch \
                 it yourself. To run delegated work on another model, a delegation tool \
                 that takes a `model` id (its description says whether it does) can \
                 name one; \
                 `describe_self(aspect=\"models\")` lists what is available."
            ),
        );
        let _ = ctx
            .provide::<atomcode_harness::seams::LlmSvc>(provider)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// Every row this crate owns, for a host that is assembling a coding product.
///
/// Separate from [`mount_swappable`] because the two hosts differ in exactly
/// two ways and this is neither of them: which front end drives the agent, and
/// where the provider comes from. Both mount `CODING_DEFAULTS` and `CODING_ROWS`
/// and both need these five rows to instantiate them — a row list naming a
/// plugin nobody registered fails to mount, which is the failure this function
/// exists to keep out of a second host's assembly.
///
/// The two rows at the end are registered and NOT in [`CODING_ROWS`], which is
/// deliberate and documented there: for them, mounting is enabling. `datalog`
/// writes a full transcript of every request to the user's disk; `cc-hooks`
/// runs the person's own external commands. Both are inert until a host inserts
/// the row.
///
/// `InjectProvider` and `InjectModels` are absent on purpose: they carry
/// per-mount state (the provider table one host built), so they belong to
/// whoever mounts rather than to the catalog.
pub fn plugins() -> Vec<Arc<dyn Plugin>> {
    vec![
        Arc::new(VerifyCadencePlugin),
        Arc::new(CodingPersonaPlugin),
        Arc::new(ExecutionPolicyPlugin),
        Arc::new(SkillCatalogPlugin),
        Arc::new(SkillFirstPlugin),
        Arc::new(DatalogPlugin),
        Arc::new(CcHooksPlugin),
        Arc::new(ChatOptionsPlugin),
    ]
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
    extra_layers: &[Layer],
) -> Result<(AgentHandle, App), String> {
    let (handle, app, _) =
        mount_swappable(working_dir, presence, provider, None, extra_layers).await?;
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
    models: Option<HostModels>,
    extra_layers: &[Layer],
) -> Result<(AgentHandle, App, Arc<ProviderSlots>), String> {
    mount_hosted(
        working_dir,
        presence,
        provider,
        models,
        HostState::default(),
        extra_layers,
    )
    .await
}

/// What the coding runtime hands a tree beyond its provider: the session it
/// continues, and the lifecycle hooks it already built against that session.
///
/// See [`crate::host_rows`] for how each becomes a row.
#[derive(Default)]
pub struct HostState {
    pub session: crate::host_rows::SessionSeed,
    pub hooks: Option<Arc<crate::host_rows::HostHooks>>,
    /// The person's live switches, and the session grants plan mode keeps for
    /// MCP tools. Absent for a host with no switches: the tree keeps the
    /// harness's mount-time rows.
    pub modes: Option<HostModes>,
    /// Tool middleware the host built (telemetry, the AtomGit push label).
    pub middleware: Option<Arc<crate::host_rows::HostMiddleware>>,
    /// The person's `hooks.json` engine, as the host loaded it — plugin hooks,
    /// session id and transcript path included. `None` when there are no hooks.
    pub cc_hooks: Option<Arc<atomcode_capabilities::cc_hooks::CCExternalHooks>>,
    /// Where the datalog writes, when the person turned it on.
    pub datalog: Option<atomcode_config::config::DatalogConfig>,
    /// The session context block, and the system prompt a continued session
    /// was stored with (its git section is kept).
    pub session_context: Option<HostContext>,
    /// Tools the runtime built around its own controllers.
    pub tools: Vec<Arc<dyn atomcode_kernel::tool::Tool>>,
    /// The skill registry the runtime loaded, its prompt catalog, and where it
    /// looked.
    pub skills: Option<crate::host_rows::LoadedSkills>,
    /// The runtime's own capabilities — goal, loop, the local-context queue,
    /// the policy intervention — offered as commands a person runs
    /// (`docs/adr/0021` §3, §10).
    pub(crate) runtime_commands: Option<Arc<dyn crate::runtime::RuntimeCommands>>,
    /// Row edits the runtime's own options call for (a capability switched off,
    /// a directory to resolve against).
    pub rows: Layer,
    /// The runtime's MCP registry, published into the tree by `mcp-host`.
    pub(crate) mcp: Option<crate::host_rows::McpPublication>,
    /// What the person chose this conversation to run, when the host knows it.
    ///
    /// Not `provider.model_name()`: a provider may report something other than
    /// what was asked for — a gateway alias, a test double — and the identity
    /// line, the model catalog entry and `describe_self` should say what the
    /// person picked. `swap_provider_for` has always passed the configured name
    /// for the same reason; this is the other half of it, at mount.
    pub model: Option<String>,
    /// The provider a requested compaction's summary is written with.
    pub summary_provider: Option<atomcode_review::SharedReviewProvider>,
    /// The native snapshot writer a committed compaction is stored through.
    pub compaction_checkpoint: Option<Arc<atomcode_capabilities::session::SnapshotHook>>,
    /// Where CodingPlan usage windows come from, when this host has an account.
    pub rate_limit_source: Option<Arc<dyn crate::rate_limit::RateLimitWindowSource>>,
    /// The settings file this runtime was configured from, when it was.
    pub config_file: Option<std::path::PathBuf>,
    /// Rows the host wrote, registered alongside coding's own.
    ///
    /// What `extra_layers` may name. Without this a host could reorder, patch
    /// and disable coding's rows but never add one of its own: a layer naming a
    /// plugin nobody registered fails to mount, and the registry is built in
    /// here, out of the caller's reach. That was the whole of what kept this
    /// entry closed to a product outside this workspace.
    ///
    /// A name already taken — by the harness's catalog, by [`plugins`], or by
    /// the rows above — is refused rather than registered. Replacing one of
    /// coding's rows is a patch pointing that row at a differently named plugin
    /// (`[[patch]]` `id = "persona-atomcode"` `name = "my-persona"`), which keeps
    /// the row's id, its place in the tree, and every patch aimed at it.
    pub plugins: Vec<Arc<dyn atomcode_plexus::Plugin>>,
    /// `[web_search] api_key`. Handed to `tool-web` as a plugin instance, never
    /// as row config — a config tree is printed verbatim, a credential must not be.
    pub web_search_api_key: Option<String>,
    /// A front end outside the App, fed by the `front-end-feed` row.
    pub front_end: Option<Arc<crate::front_end::FrontEnd>>,
    /// The provider a delegated agent inheriting the conversation's model runs
    /// on, billed to the session apart from it.
    pub delegated_llm: Option<Arc<dyn LlmProvider>>,
    /// Where delegated agents' turns go as `Team` events, for the shipped panel.
    pub(crate) team_events: Option<crate::team_progress::TeamSink>,
}

/// See [`crate::host_rows::SessionContextPlugin`].
pub struct HostContext {
    pub hook: Arc<atomcode_capabilities::session::SessionContextHook>,
    pub stored: Option<String>,
}

/// The switches `set_mode` writes, handed to the rows that obey them.
pub struct HostModes {
    pub modes: atomcode_harness::seams::Modes,
    pub plan_mcp_grants: Arc<dyn atomcode_capabilities::tools::PermissionStore>,
    /// Every other "always allow", in the store the approval seam remembers into.
    pub approval_grants: Arc<dyn atomcode_capabilities::tools::PermissionStore>,
}

impl HostState {
    /// What a host outside this workspace fills: the rows it wrote, and nothing
    /// else. The other fields are this runtime's own wiring — its session store,
    /// its MCP publication, the switches `set_mode` writes — and three of them
    /// are crate-private, so `..Default::default()` is not available out there.
    pub fn with_plugins(plugins: Vec<Arc<dyn atomcode_plexus::Plugin>>) -> Self {
        Self {
            plugins,
            ..Default::default()
        }
    }
}

/// As [`mount_swappable`], carrying the runtime's own state into the tree.
pub async fn mount_hosted(
    working_dir: &Path,
    presence: Presence,
    provider: Arc<dyn LlmProvider>,
    models: Option<HostModels>,
    host: HostState,
    extra_layers: &[Layer],
) -> Result<(AgentHandle, App, Arc<ProviderSlots>), String> {
    let model = host
        .model
        .clone()
        .unwrap_or_else(|| provider.model_name().to_string());
    let (providers, provider_id) = ProviderSlots::new(provider);
    let artifacts = working_dir.join(".atomcode").join("artifacts");
    // The two halves of one rule. Attended: no `root`, so the fs world is not
    // fenced and a target next door reaches `tool-write-approval`, which asks.
    // Headless: fenced, and the `approval` row stays the `deny-risky` one that
    // refuses without asking — there is nobody to ask.
    //
    // Only the fence. Which row answers `approval` is not presence-dependent and
    // is settled once, in `CODING_DEFAULTS`: base's never-asks row is off in both
    // modes because `ui-handle` claims that seam and round-trips the driver, and
    // mounting `ui-handle` at all means a driver is present.
    let boundary = match presence {
        Presence::Attended => Layer::new()
            .patch("fs", FsWorldPatch { root: None })
            .map_err(|e| e.to_string())?,
        Presence::Headless => Layer::new()
            .patch(
                "fs",
                FsWorldPatch {
                    root: Some(working_dir),
                },
            )
            .map_err(|e| e.to_string())?,
    };
    // Built, not formatted. A working directory is whatever the person made, and
    // a `{:?}` of it writes a control character as `\u{7f}` — which is not TOML,
    // and the layer then fails to parse a long way from whoever set the path.
    // `toml_string` exists to dodge that; serializing the row's own shape means
    // there is nothing to dodge.
    let scoped = Layer::new()
        .when(models.is_some(), |layer| {
            // With a catalog, the two rows that need one come on; without, they
            // stay down and `task`/`team` run on the conversation's model, which
            // is what they did before the seam existed.
            layer.enable("models-host").enable("llm-utility-selected")
        })
        .then(boundary)
        .patch("agent-loop", AgentLoopPatch { working_dir })
        .map_err(|e| e.to_string())?
        .swap("llm", "llm-injected")
        .patch(
            "llm",
            LlmInjectedRow {
                provider_id: &provider_id,
            },
        )
        .map_err(|e| e.to_string())?;
    // `infra`, not `base`: the machine is the harness's, the product decisions
    // are this crate's. See [`CODING_DEFAULTS`] for why that is not the same as
    // taking base and patching it.
    let mut layers = vec![atomcode_harness::bundle::infra().map_err(|e| e.to_string())?];
    layers.push(Layer::from_toml(CODING_DEFAULTS).map_err(|e| e.to_string())?);
    layers.push(scoped);
    layers.push(
        Layer::from_toml(&coding_overlay(working_dir, &artifacts, presence, &model))
            .map_err(|e| e.to_string())?,
    );
    // The session is the runtime's: its id, and its log in the session store,
    // appended by `session-store` under the runtime's lease and replayed by
    // `session-native`. The harness's own `session` row would mint an id.
    // With live switches, plan mode is the product's and is always mounted — it
    // decides per call whether it is on. Patched in place so it keeps
    // `plan-mode`'s position, ahead of the approval gates: a write plan mode
    // refuses must not first be asked about.
    let mut hosted = Layer::new()
        .swap("session", "session-native")
        // In the persistence row's place, ahead of the agent that resumes from
        // it: a row inserted at the end would mount after `ui-handle` had
        // already created the agent.
        .when(host.session.stored.is_some(), |layer| {
            layer
                .swap("session-persistence-jsonl", "session-store")
                .enable("session-persistence-jsonl")
        })
        // With live switches, plan mode is the product's and is always mounted
        // — it decides per call whether it is on.
        .when(host.modes.is_some(), |layer| {
            layer
                .swap("plan-mode", "plan-mode-live")
                .enable("plan-mode")
                .insert(Entry::named("modes-host"))
                .insert(Entry::named("grants-host"))
        })
        .when(host.cc_hooks.is_some(), |layer| {
            layer.swap("cc-hooks", "cc-hooks-host").enable("cc-hooks")
        })
        .when(host.session_context.is_some(), |layer| {
            layer.swap("project-instructions", "session-context")
        })
        .when(!host.tools.is_empty(), |layer| {
            layer.insert(Entry::named("host-tools"))
        })
        .when(host.skills.is_some(), |layer| {
            layer
                .swap("skills", "skills-host")
                .disable("skill-catalog-inline")
        })
        .when(host.runtime_commands.is_some(), |layer| {
            layer.insert(Entry::named("capability-commands"))
        })
        .when(host.mcp.is_some(), |layer| {
            layer.swap("mcp", "mcp-host").enable("mcp")
        })
        .when(host.delegated_llm.is_some(), |layer| {
            layer.insert(Entry::named("llm-delegated-host"))
        })
        .when(host.team_events.is_some(), |layer| {
            layer.insert(Entry::named("team-progress"))
        })
        .when(host.compaction_checkpoint.is_some(), |layer| {
            layer.insert(Entry::named("native-compaction-checkpoint"))
        })
        .when(host.summary_provider.is_some(), |layer| {
            layer.swap("compaction-tail", "compaction-coding")
        })
        .when(host.config_file.is_some(), |layer| {
            layer.insert(Entry::named("config-file"))
        })
        .when(host.web_search_api_key.is_some(), |layer| {
            layer.swap("tool-web", "tool-web-keyed")
        })
        .when(host.front_end.is_some(), |layer| {
            layer.insert(Entry::named("front-end-feed"))
        });
    if let Some(datalog) = host.datalog.as_ref() {
        hosted = hosted
            .insert(
                Entry::named("datalog")
                    .with(DatalogPatch {
                        working_dir,
                        model: &model,
                        dir: datalog.dir.as_deref(),
                    })
                    .map_err(|e| e.to_string())?,
            )
            .clone();
    }
    if let Some(hooks) = host.hooks.as_ref() {
        hosted = hosted.then(hooks.rows());
    }
    if let Some(middleware) = host.middleware.as_ref() {
        hosted = hosted.then(middleware.rows());
    }
    layers.push(hosted.then(host.rows.clone()));
    for layer in extra_layers {
        layers.push(layer.clone());
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
    for row in plugins() {
        registry.register(row);
    }
    registry.register(Arc::new(InjectProvider(providers.clone())));
    if let Some(stored) = host.session.stored.clone() {
        registry.register(Arc::new(crate::session_store::SessionStorePlugin(stored)));
    }
    registry.register(Arc::new(crate::host_rows::SessionNativePlugin(Arc::new(
        host.session,
    ))));
    registry.register(Arc::new(crate::host_rows::KernelHooksPlugin(
        host.hooks.unwrap_or_default(),
    )));
    registry.register(Arc::new(crate::host_rows::RateLimitCodingPlugin(
        host.rate_limit_source.clone(),
    )));
    registry.register(Arc::new(crate::host_rows::KernelMiddlewarePlugin(
        host.middleware.unwrap_or_default(),
    )));
    if let Some(slot) = host.summary_provider {
        registry.register(Arc::new(crate::host_rows::CompactionCodingPlugin(slot)));
    }
    if let Some(front_end) = host.front_end {
        registry.register(Arc::new(crate::front_end::FrontEndFeedPlugin(front_end)));
    }
    if let Some(provider) = host.delegated_llm {
        registry.register(Arc::new(DelegatedLlmPlugin(provider)));
    }
    if let Some(sink) = host.team_events {
        registry.register(Arc::new(crate::team_progress::TeamProgressPlugin(sink)));
    }
    if let Some(hook) = host.compaction_checkpoint {
        registry.register(Arc::new(
            crate::host_rows::NativeCompactionCheckpointPlugin(hook),
        ));
    }
    if let Some(publication) = host.mcp {
        registry.register(Arc::new(crate::host_rows::McpHostPlugin::new(publication)));
    }
    if let Some(skills) = host.skills {
        registry.register(Arc::new(crate::host_rows::SkillsHostPlugin(skills)));
    }
    if let Some(commands) = host.runtime_commands {
        registry.register(Arc::new(crate::host_rows::CapabilityCommandsPlugin(
            commands,
        )));
    }
    if let Some(file) = host.config_file {
        registry.register(Arc::new(crate::host_rows::ConfigFilePlugin(file)));
    }
    if let Some(key) = host.web_search_api_key {
        registry.register(Arc::new(
            atomcode_harness::plugins::capabilities::WebPlugin::with_api_key(key),
        ));
    }
    if !host.tools.is_empty() {
        registry.register(Arc::new(crate::host_rows::HostToolsPlugin(host.tools)));
    }
    if let Some(engine) = host.cc_hooks {
        registry.register(Arc::new(crate::host_rows::CcHooksHostPlugin(engine)));
    }
    if let Some(context) = host.session_context {
        registry.register(Arc::new(crate::host_rows::SessionContextPlugin {
            hook: context.hook,
            stored: context.stored,
        }));
    }
    if let Some(modes) = host.modes {
        registry.register(Arc::new(crate::host_rows::ModesHostPlugin(modes.modes)));
        registry.register(Arc::new(crate::host_rows::PlanModeLivePlugin(
            modes.plan_mcp_grants,
        )));
        registry.register(Arc::new(crate::host_rows::GrantsHostPlugin(
            modes.approval_grants,
        )));
    }
    if let Some(host) = models {
        registry.register(Arc::new(InjectModels(Arc::new(CodingModels {
            host,
            slots: providers.clone(),
        }))));
    }
    for plugin in host.plugins {
        let name = plugin.name();
        if registry.get(name).is_some() {
            return Err(format!(
                "plugin `{name}` is already in the coding catalog; to replace that row, patch \
                 it to a plugin with a different name rather than registering a second `{name}`"
            ));
        }
        registry.register(plugin);
    }

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
// discipline). In the chain it was `VerifyCadenceHook`, sitting on
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
            atomcode_harness::agent::MessageOrigin::Internal,
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
    ctx: Context,
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
        //
        // The person talks to the lead. A delegated agent's request carries the
        // lead's task, not the person's restriction, so it must not rewrite it:
        // members work under whatever the lead is held to (`docs/adr/0023`,
        // addendum).
        if restates_the_persons_restriction(&self.ctx) {
            self.policy.update_from_messages(&req.messages);
        }
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
        // Deliberately NOT consulting `authorization`. Every other gate does,
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

/// Whether the request being made is one whose messages say what the person
/// restricted: the conversation's own, not a delegated agent's.
fn restates_the_persons_restriction(ctx: &Context) -> bool {
    atomcode_harness::agent::scoped(ctx)
        .service::<atomcode_harness::seams::DelegationLaneSvc>()
        .is_none()
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
            ctx: ctx.clone(),
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
pub(crate) const SKILLS_FRAGMENT: (&str, i32) = ("skills", 60);

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
    /// What to record this session's rounds under. Empty asks the provider,
    /// which may answer with something other than what the person chose.
    #[serde(default)]
    model: String,
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
        // The name the person chose when the host knows it: a record of what ran
        // is worth little if it says "canned" because that is what the adapter
        // calls itself.
        let model = if row.model.is_empty() {
            llm.as_ref()
                .map(|l| l.model_name().to_string())
                .unwrap_or_default()
        } else {
            row.model.clone()
        };
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
            // An explicit hook `allow` short-circuits the downstream CONVENIENCE
            // gates, which is the point of saying it. Presumed, not the person:
            // a hook is code installed once, so installing one must not quietly
            // turn off a boundary the person switched on.
            BeforeOutcome::Allow { .. } => {
                exec.authorization = atomcode_harness::events::Authorization::Presumed
            }
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
        mount_cc_hooks(ctx, Arc::new(engine));
        Ok(())
    }
}

/// Mount a loaded `hooks.json` engine at the Claude-Code-compatible moments.
///
/// Shared by `cc-hooks` (which loads the file itself) and `cc-hooks-host` (which
/// mounts the engine the runtime loaded, plugin hooks included), so the two
/// cannot disagree about when a person's hook runs.
pub(crate) fn mount_cc_hooks(
    ctx: &Context,
    engine: Arc<atomcode_capabilities::cc_hooks::CCExternalHooks>,
) {
    let hooks = Arc::new(CcHooks {
        ctx: ctx.clone(),
        engine,
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
    /// The language commit messages and the like are written in, when the
    /// person chose one.
    #[serde(default)]
    language: Option<atomcode_config::locale::Locale>,
}

#[async_trait]
impl Plugin for CodingPersonaPlugin {
    fn name(&self) -> &'static str {
        "persona-atomcode"
    }
    fn uses(&self) -> &'static [&'static str] {
        // `system-prompt` is what it writes; `tools` and `llm` are what it reads
        // to decide what to write. It used to read the tool catalog to decide
        // whether to describe `todowrite` / `ask_user` — those decisions now
        // belong to the rows that mount them, which is what makes the
        // description leave when the tool does — but `has("memory")` still asks
        // the catalog, and `model = ""` (the default here) asks the `llm` seam
        // for the running model. Undeclared reads are invisible: `/audit` cannot
        // report a row that is absent when the consumer never said it wanted it.
        &["system-prompt", "tools", "llm"]
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
        // Empty means "ask the running tree". A host whose provider is built by
        // a config row rather than by the host itself (`llm-atomcode-config`)
        // cannot know the model name before the tree is up, which is when the
        // row config is written — and it can be changed afterwards with
        // `--model`. Reading the seam is what the row's own documentation
        // already said it was protecting: a persona naming the previous model
        // is a quiet lie in the first line the model reads.
        let model = if row.model.is_empty() {
            ctx.service::<atomcode_harness::seams::LlmSvc>()
                .map(|llm| llm.model_name().to_string())
                .unwrap_or_default()
        } else {
            row.model.clone()
        };
        // Three kinds of thing used to be decided here and are not any more: the two delegation
        // paragraphs (`team-in-process`, `subagent-in-process`), `## TASK TRACKING` (`tool-todo`)
        // and `## CODE REVIEW` (`tool-code-review`) are each contributed by the row that mounts
        // the tool and leave with it. What stays in the persona is asked of the running tree
        // rather than of the `ATOMCODE_MEMORY_TOOL` / `ATOMCODE_REQUEST_USER_INPUT` envs the
        // chain reads, which cannot see a tree that failed to mount the tool. See
        // `coding_persona_rows`.
        let text = crate::persona::coding_persona_rows(&model, row.language, &has);
        let Some(prompts) = ctx.service::<atomcode_harness::seams::SystemPromptSvc>() else {
            return Ok(());
        };
        // Rank 0 and the generic row's id: the identity line goes first, and
        // there is only ever one of it.
        prompts.contribute("persona-coding", 0, text);
        Ok(())
    }
}

// ---- request options ----------------------------------------------------

/// The person's request options, on requests that state none of their own.
pub struct ChatOptionsPlugin;

#[derive(serde::Deserialize, Default)]
struct ChatOptionsRow {
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
}

struct ChatOptions {
    reasoning_effort: Option<atomcode_kernel::provider::ReasoningEffort>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
}

#[async_trait]
impl atomcode_plexus::Waterfall<atomcode_harness::events::AgentRequest> for ChatOptions {
    async fn handle(
        &self,
        req: &mut atomcode_harness::events::ModelRequest,
        next: atomcode_plexus::Next<'_, atomcode_harness::events::AgentRequest>,
    ) -> Result<atomcode_harness::events::ModelResponse, atomcode_harness::events::RequestError>
    {
        let options = &mut req.options;
        if options.reasoning_effort.is_none() {
            options.reasoning_effort = self.reasoning_effort;
        }
        if options.max_tokens.is_none() {
            options.max_tokens = self.max_tokens;
        }
        if options.temperature.is_none() {
            options.temperature = self.temperature;
        }
        next.run(req).await
    }
}

#[async_trait]
impl Plugin for ChatOptionsPlugin {
    fn name(&self) -> &'static str {
        "chat-options"
    }
    fn description(&self) -> &'static str {
        "the person's reasoning effort, max tokens and temperature on every request"
    }
    async fn apply(&self, ctx: &Context, config: &serde_json::Value) -> Result<(), String> {
        let row: ChatOptionsRow = if config.is_null() {
            ChatOptionsRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let options = ChatOptions {
            reasoning_effort: atomcode_kernel::provider::ReasoningEffort::from_config(
                row.reasoning_effort.as_deref(),
            ),
            max_tokens: row.max_tokens,
            temperature: row.temperature,
        };
        // Said the way it is applied: for every agent, unless something more
        // specific — a member's role — already said otherwise.
        if let Some(level) = options.reasoning_effort {
            let _ = ctx.on_emit::<atomcode_harness::events::DescribeAgent>(
                move |describing: &atomcode_harness::events::Describing| {
                    let mut description =
                        describing.description.lock().expect("description poisoned");
                    description.reasoning_effort.get_or_insert(level);
                },
            );
        }
        if options.reasoning_effort.is_none()
            && options.max_tokens.is_none()
            && options.temperature.is_none()
        {
            return Ok(());
        }
        // Outermost, so a request a later row retries or trims still carries them.
        let _ = ctx.on_waterfall::<atomcode_harness::events::AgentRequest>(Arc::new(options), true);
        Ok(())
    }
}

/// `llm-delegated-host`: the model a delegated agent inheriting the
/// conversation's runs on — the runtime's own, wrapped so its spend is billed
/// to the session apart from the conversation and metered as a subagent's.
struct DelegatedLlmPlugin(Arc<dyn LlmProvider>);

#[async_trait]
impl Plugin for DelegatedLlmPlugin {
    fn name(&self) -> &'static str {
        "llm-delegated-host"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["llm-delegated"]
    }
    fn description(&self) -> &'static str {
        "the conversation's model for delegated agents, billed to the session apart from it"
    }
    async fn apply(&self, ctx: &Context, _config: &serde_json::Value) -> Result<(), String> {
        let _ = ctx
            .provide::<atomcode_harness::seams::DelegatedLlmSvc>(self.0.clone())
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod delegated_restriction_tests {
    use super::*;

    /// Only the conversation's own request restates the person's execution
    /// restriction; a delegated agent's carries the lead's task, and re-reading
    /// it would lift the restriction while the lead is still held to it.
    #[tokio::test]
    async fn a_delegated_request_never_restates_the_persons_restriction() {
        let app = App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::empty(),
        );
        let root = app.context();
        assert!(restates_the_persons_restriction(&root));

        let member = root.isolate();
        let _lane = member
            .provide::<atomcode_harness::seams::DelegationLaneSvc>(Arc::new(
                atomcode_harness::seams::DelegationLane {
                    scopes: vec!["**".into()],
                },
            ))
            .unwrap();
        let tree = root.clone();
        let restates = atomcode_harness::agent::as_agent(member, async move {
            restates_the_persons_restriction(&tree)
        })
        .await;
        assert!(!restates);
    }
}
