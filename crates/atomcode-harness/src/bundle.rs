//! Bundles: the shipped composition, expressed as config rather than code.
//!
//! `base` is everything an agent needs except a way to talk to it. Each app
//! bundle adds exactly one front-end row, so "the same agent behind a different
//! surface" is a profile rather than a build. See [`crate::profile`] for how
//! they stack.
//!
//! `base` is the shared first layer, the way `dsh-base` is for DeepSeek Harness.
//! Everything in it is addressable: the row id `llm` is what a patch targets to
//! change models or swap the adapter entirely, and `approval`'s mode is a config
//! value rather than a build flag.

use atomcode_plexus::{ConfigTree, Layer, Result};

/// The shared base: registries, a model adapter, tools, policy, the loop.
pub const BASE: &str = r#"
# --- agents: the registry everything else finds live work through -----------
[[insert]]
name = "agents"

# --- the session domain: the log, its projections, its durable store --------
[[insert]]
name = "session"

[[insert]]
name = "session-projection"

[[insert]]
name = "session-persistence-jsonl"
config = { resume = false }

# --- registries: the slots everything else fills or reads -------------------
[[insert]]
name = "tools"

[[insert]]
name = "system-prompt"

# --- the model. Row id `llm` is the address; the plugin behind it is a value.
# The default reads the provider the user already configured for AtomCode.
# `--env-model` swaps in the environment-driven row for CI and containers.
[[insert]]
id = "llm"
name = "llm-atomcode-config"

# --- the execution world: where files and processes actually live ----------
# Point these three somewhere else (a container, a remote sandbox) and every
# tool below follows, because no tool touches a path or spawns a process itself.
[[insert]]
id = "fs"
name = "fs-local"

[[insert]]
id = "subprocess"
name = "subprocess-local"

[[insert]]
id = "shell"
name = "bash-local"

# --- capabilities, all routed through the world above ----------------------
[[insert]]
name = "tool-fs-world"

[[insert]]
name = "tool-search"

# Structural search: cheap to mount, but only the audit variants ask the kind of
# question it answers, so base leaves it off.
[[insert]]
name = "tool-ast-grep"
disabled = true

[[insert]]
name = "tool-bash-world"

# The production local implementations, mounted dormant. They claim the same
# tool names as the world-routed rows, so enabling one means disabling the
# other — the catalog refuses a duplicate rather than silently picking.
[[insert]]
name = "tool-fs"
disabled = true

[[insert]]
name = "tool-bash"
disabled = true

# --- capabilities that are not part of the execution world ------------------
[[insert]]
name = "skills"

[[insert]]
name = "codeintel"

# The graph layer builds and caches a whole-repo index; opt in when the work
# needs cross-file reasoning rather than paying for it on every run.
[[insert]]
name = "code-graph"
disabled = true

# Reaches the public internet, so it is a deliberate choice, not a default.
[[insert]]
name = "tool-web"
disabled = true

[[insert]]
name = "memory"

[[insert]]
name = "tool-todo"

# Delegation runs a child agent in its own realm. Off by default: it multiplies
# model calls, and a tree should opt into that.
[[insert]]
name = "subagent-in-process"
disabled = true

# External MCP servers are other people's processes: opt in.
[[insert]]
name = "mcp"
disabled = true

[[insert]]
name = "persona-coding"

# --- turn policy: what the loop deliberately does not decide ---------------
[[insert]]
name = "round-cap"
config = { max_rounds = 24, max_seconds = 0 }

[[insert]]
name = "llm-retry"
config = { attempts = 3, backoff_ms = 1000 }

# Recovery, outermost first: a bound that each retry resets is not a bound,
# and waiting out a limit has to happen outside everything that would burn it.
[[insert]]
name = "llm-request-timeout"
config = { request_secs = 600 }

[[insert]]
name = "llm-rate-limit"
config = { max_waits = 5, max_wait_secs = 120, fallback_secs = 5 }

[[insert]]
name = "compaction-overflow"
config = { max_attempts = 3 }

[[insert]]
name = "truncation-recovery"
config = { max_continuations = 4 }

[[insert]]
name = "llm-stream-recovery"
config = { max_recoveries = 1 }

[[insert]]
name = "reasoning-filter"

[[insert]]
name = "compaction-tail"
config = { threshold = 0.75, keep_turns = 2 }

[[insert]]
name = "tool-loop-guard"
config = { warn_after = 3, stop_after = 4 }

# The coarse companion: same calls round after round, whatever they returned.
# The exact guard needs matching results too, so it misses a call whose output
# varies slightly every time — which is the shape that actually burns a budget.
[[insert]]
name = "repeat-fuse"
config = { nudge_at = 3, stop_at = 6 }

# --- policy, in wrap order: repair -> rules -> approve -> cap --------------
[[insert]]
name = "tool-args-repair"

# No rules by default, so the row is inert until a user writes some.
[[insert]]
name = "permissions"
config = { allow = [], deny = [] }

[[insert]]
name = "approval"
config = { mode = "deny-risky" }

# The interactive alternative to the row above: same seam, but it asks. Both
# fill `approval`, so exactly one may be enabled.
[[insert]]
name = "approval-interactive"
disabled = true

# Read-only exploration. Off by default; `--plan` turns it on.
[[insert]]
name = "plan-mode"
disabled = true

[[insert]]
name = "tool-result-cap"
config = { max_bytes = 65536 }

# Scheduling is a policy, not loop code: remove this row and a round's calls
# run one at a time, which is always correct and sometimes slow.
[[insert]]
name = "tool-exec-parallel"
config = { max_parallel = 4 }

# --- the driver --------------------------------------------------------------
# `max_rounds` here is the runaway fuse, not the budget: the budget is the
# `round-cap` row above. Remove that row and this is all that stops a loop.
[[insert]]
name = "agent-loop"
config = { max_rounds = 100 }

[[insert]]
name = "trace"
config = { stream = true, tools = true, summary = true }

# --- session-level services -------------------------------------------------
[[insert]]
name = "session-title-first-prompt"

# Dormant: nothing in the default tree asks a human, and a provider nobody
# consumes is dead weight the audit rightly complains about. `--interactive`
# turns it on together with the approval row that needs it.
[[insert]]
name = "user-questions-unattended"
disabled = true

[[insert]]
name = "telemetry"
disabled = true

[[insert]]
name = "token-budget"
config = { max_prompt_tokens = 0 }
"#;

/// Swap the model for a scripted one. The only row it touches is `llm`; the
/// loop, tools, policy and tracing rows are untouched and cannot tell.
pub const OFFLINE: &str = r#"
[[patch]]
id = "llm"
name = "llm-replay"
config = { script = [
  { text = "Let me look around first.", calls = [ { name = "list_directory", args = { path = ".", depth = 1 } } ] },
  { text = "And the manifest.", calls = [ { name = "read_file", args = { file_path = "Cargo.toml" } } ] },
  { text = "That is the crate layout." },
] }
"#;

/// A read-only profile.
///
/// Note what it does *not* do: it does not touch a single tool row. Swapping the
/// `fs` provider makes every write fail at the world boundary, which is the
/// difference between a policy the tools cooperate with and a world that cannot
/// be written to.
pub const READ_ONLY: &str = r#"
[[patch]]
id = "fs"
name = "fs-readonly"

[[patch]]
id = "approval"
config = { mode = "read-only" }
"#;

/// Everything on: the graph layer, web access, and delegation.
pub const FULL: &str = r#"
[[patch]]
id = "code-graph"
disabled = false

[[patch]]
id = "tool-web"
disabled = false

[[patch]]
id = "subagent-in-process"
disabled = false
"#;

/// An interactive terminal session instead of one prompt.
///
/// It also switches approval to the asking policy. A front end with a terminal
/// in front of it should ask rather than refuse — refusing outright is the
/// right default only when there is nobody to ask. The REPL fills
/// `user-questions` itself, so the unattended provider steps aside.
pub const REPL: &str = r#"
[[patch]]
id = "ui"
name = "ui-repl"
config = { banner = true, prompt = "› " }

[[patch]]
id = "user-questions-unattended"
disabled = true

[[patch]]
id = "approval"
disabled = true

[[patch]]
id = "approval-interactive"
disabled = false
"#;

/// Read-only exploration: investigate and produce a plan, change nothing.
pub const PLAN: &str = r#"
[[patch]]
id = "plan-mode"
disabled = false
"#;

/// Ask a human before every risky call, instead of refusing outright.
pub const INTERACTIVE: &str = r#"
[[patch]]
id = "approval"
disabled = true

[[patch]]
id = "approval-interactive"
disabled = false

# The interactive policy injects `user-questions`, so something must fill it.
# The shipped provider declines everything; pair this with `--repl` and the
# terminal front end fills the same slot with a real prompt instead.
[[patch]]
id = "user-questions-unattended"
disabled = false
"#;

/// Continue an existing session instead of starting a new one.
///
/// Takes the id, because "which conversation" is not something a harness should
/// guess. `--continue` resolves the most recent one and produces this.
pub fn resume_overlay(id: &str) -> String {
    format!(
        "[[patch]]\nid = \"session\"\nconfig = {{ id = {id:?} }}\n\n\
         [[patch]]\nid = \"session-persistence-jsonl\"\nconfig = {{ resume = true }}\n"
    )
}

/// Allow everything. For a sandbox, a container, or an eval where the whole
/// point is to let the agent act without a human in the loop.
pub const YOLO: &str = r#"
[[patch]]
id = "approval-interactive"
disabled = true

[[patch]]
id = "approval"
disabled = false
config = { mode = "yolo" }

# Nothing asks any more, so the asker is dead weight — which `--audit` would
# otherwise (correctly) report.
[[patch]]
id = "user-questions-unattended"
disabled = true
"#;

/// Take the model from the environment instead of the user's config file —
/// what a container or a CI job wants, where there is no `~/.atomcode`.
pub const ENV_MODEL: &str = r#"
[[patch]]
id = "llm"
name = "llm-openai-compat"
config = { api_key_env = "ATOMCODE_API_KEY" }
"#;

/// Swap the local tool implementations for the production ones. Same four tool
/// names, same model-facing behaviour, no `fs` seam in the path.
///
/// The execution world comes out with them. The production tools reach the disk
/// and spawn processes directly, so leaving `fs` / `subprocess` / `shell`
/// mounted would leave three providers running that nothing reads — which is
/// exactly what `--audit` reports if you try.
pub const NATIVE_TOOLS: &str = r#"
[[patch]]
id = "tool-fs-world"
disabled = true

[[patch]]
id = "tool-bash-world"
disabled = true

[[patch]]
id = "fs"
disabled = true

[[patch]]
id = "subprocess"
disabled = true

[[patch]]
id = "shell"
disabled = true

[[patch]]
id = "tool-fs"
disabled = false

[[patch]]
id = "tool-bash"
disabled = false
"#;

pub fn base() -> Result<Layer> {
    Layer::from_toml(BASE)
}

/// Stack the base bundle with any number of patch layers, in order.
pub fn tree(patches: &[&str]) -> Result<ConfigTree> {
    let mut layers = vec![base()?];
    for patch in patches {
        layers.push(Layer::from_toml(patch)?);
    }
    ConfigTree::from_layers(layers)
}

// ---- app bundles: one front end each ------------------------------------

/// One prompt, one turn, exit.
///
/// It runs in a terminal, so it can ask — and asking is the right default when
/// there is someone to ask. Refusing outright instead leaves the agent unable
/// to do the work it was given, which is a worse failure than an extra prompt.
/// `headless` is where refusing is correct, and it says so explicitly.
pub const ONESHOT_APP: &str = r#"
[[insert]]
id = "ui"
name = "ui-oneshot"

[[patch]]
id = "user-questions-unattended"
name = "user-questions-terminal"
disabled = false

[[patch]]
id = "approval"
disabled = true

[[patch]]
id = "approval-interactive"
disabled = false
"#;

/// An interactive terminal session.
///
/// It also switches approval to the asking policy: a front end with a terminal
/// in front of it should ask rather than refuse — refusing outright is the right
/// default only when there is nobody to ask.
pub const REPL_APP: &str = r#"
[[insert]]
id = "ui"
name = "ui-repl"
config = { banner = true, prompt = "› " }

[[patch]]
id = "user-questions-unattended"
name = "user-questions-terminal"
disabled = false

[[patch]]
id = "approval"
disabled = true

[[patch]]
id = "approval-interactive"
disabled = false
"#;

/// A full-screen terminal UI over the same seams as the line-oriented one.
pub const TUI_APP: &str = r#"
[[insert]]
id = "ui"
name = "ui-tui"

[[patch]]
id = "trace"
config = { stream = false, tools = false, summary = false }

# The TUI draws its own prompts inside the alternate screen, so the asker row
# becomes the one that knows how.
[[patch]]
id = "user-questions-unattended"
name = "user-questions-tui"
disabled = false

[[patch]]
id = "approval"
disabled = true

[[patch]]
id = "approval-interactive"
disabled = false
"#;

/// An HTTP server: events over SSE, messages over POST.
pub const WEB_APP: &str = r#"
[[insert]]
id = "ui"
name = "ui-web"
config = { addr = "127.0.0.1:7878" }

[[patch]]
id = "trace"
config = { stream = false, tools = false, summary = false }

# The web row asks through the browser, so the standalone asker stands down and
# the asking policy becomes usable here.
[[patch]]
id = "user-questions-unattended"
disabled = true

[[patch]]
id = "approval"
disabled = true

[[patch]]
id = "approval-interactive"
disabled = false
"#;

/// A line-delimited JSON-RPC server on stdio, for a program on the other end.
pub const SDK_APP: &str = r#"
[[insert]]
id = "ui"
name = "ui-jsonrpc"

# stdout is the protocol channel; nothing else may write to it.
[[patch]]
id = "trace"
config = { stream = false, tools = false, summary = false }

# The client can be asked over the same socket, so the asking policy works here
# and the standalone asker stands down.
[[patch]]
id = "user-questions-unattended"
disabled = true

[[patch]]
id = "approval"
disabled = true

[[patch]]
id = "approval-interactive"
disabled = false
"#;

/// No front end: the embedder drives.
pub const EMBED_APP: &str = r#"
[[insert]]
id = "ui"
name = "ui-quiet"

[[patch]]
id = "trace"
config = { stream = false, tools = false, summary = false }
"#;

/// Silence and no side files. An eval harness wants the outcome, not a
/// transcript on disk and not ANSI on its stdout.
pub const HEADLESS_PATCH: &str = r#"
[[patch]]
id = "trace"
config = { stream = false, tools = false, summary = false }

[[patch]]
id = "session-persistence-jsonl"
disabled = true

# Nobody to ask, so refuse. Deliberate here, not a leftover default: an eval or
# a CI job must never block on a prompt, and must never self-approve either.
[[patch]]
id = "approval-interactive"
disabled = true

[[patch]]
id = "approval"
disabled = false
config = { mode = "deny-risky" }

[[patch]]
id = "user-questions-unattended"
name = "user-questions-unattended"
disabled = true
"#;

/// Every bundle by name, for profiles to reference.
pub const BUNDLES: &[(&str, &str)] = &[
    ("base", BASE),
    ("longcode-spec", LONGCODE_SPEC),
    ("longcode-air-spec", LONGCODE_AIR_SPEC),
    ("code-security-spec", CODE_SECURITY_SPEC),
    ("code-review-spec", CODE_REVIEW_SPEC),
    ("oneshot-app", ONESHOT_APP),
    ("repl-app", REPL_APP),
    ("tui-app", TUI_APP),
    ("web-app", WEB_APP),
    ("sdk-app", SDK_APP),
    ("embed-app", EMBED_APP),
];

/// The shipped profiles: `(name, bundles, own patch, description)`.
///
/// A profile is data. Adding one here and dropping a file in
/// `$ATOMCODE_HOME/profiles/` are the same act, and the file wins.
pub const PROFILES: &[(&str, &[&str], Option<&str>, &str)] = &[
    (
        "oneshot",
        &["base", "oneshot-app"],
        None,
        "one prompt, one turn, exit",
    ),
    (
        "repl",
        &["base", "repl-app"],
        None,
        "an interactive terminal session that can ask questions",
    ),
    (
        "tui",
        &["base", "tui-app"],
        None,
        "a full-screen terminal UI",
    ),
    (
        "web",
        &["base", "web-app"],
        None,
        "an HTTP server with a live event stream",
    ),
    (
        "sdk",
        &["base", "sdk-app"],
        None,
        "line-delimited JSON-RPC on stdio",
    ),
    (
        "embed",
        &["base", "embed-app"],
        None,
        "no front end; a library caller drives",
    ),
    (
        "headless",
        &["base", "oneshot-app"],
        Some(HEADLESS_PATCH),
        "one prompt, no rendering, no persistence — for evals and CI",
    ),
    (
        "plan",
        &["base", "repl-app"],
        Some(PLAN),
        "read-only exploration: investigate and produce a plan",
    ),
    (
        "full",
        &["base", "repl-app"],
        Some(FULL),
        "everything on: code graph, web access, delegation",
    ),
    // ---- the four product variants -------------------------------------
    (
        "longcode",
        &["base", "repl-app", "longcode-spec"],
        None,
        "LongCode — the full coding agent, everything mounted",
    ),
    (
        "longcode-air",
        &["base", "repl-app", "longcode-air-spec"],
        None,
        "LongCode Air — the same agent with its costs cut",
    ),
    (
        "code-security",
        &["base", "oneshot-app", "code-security-spec"],
        None,
        "Code Security — read-only audit that reports findings",
    ),
    (
        "code-review",
        &["base", "oneshot-app", "code-review-spec"],
        None,
        "Code Review — read-only reviewer that reports findings",
    ),
];

// ---- specializations: four products, one base ---------------------------
//
// A specialization decides three things: what the model is told (persona), what
// it can reach (tools and the execution world), and what it is allowed to do
// (policy). None of them decides how a person talks to it — that is the app
// bundle's job, and keeping the two orthogonal is what lets `code-review` run
// behind a terminal, a browser or a JSON-RPC socket without a second assembly.

/// **LongCode** — the full coding agent.
///
/// Everything mounted, delegation available, asks before risky calls. This is
/// the variant that changes code, so it is also the one with a human in the
/// loop by default.
pub const LONGCODE_SPEC: &str = r#"
[[patch]]
id = "code-graph"
disabled = false

[[patch]]
id = "tool-web"
disabled = false

[[patch]]
id = "subagent-in-process"
disabled = false

[[patch]]
id = "mcp"
disabled = false

[[patch]]
id = "round-cap"
config = { max_rounds = 40, max_seconds = 0 }

[[patch]]
id = "compaction-tail"
config = { threshold = 0.75, keep_turns = 3 }
"#;

/// **LongCode Air** — the same agent with its costs cut.
///
/// Fewer tools, a tighter round budget, earlier compaction, smaller tool
/// results. The differences are all quantities and mounted rows; the loop, the
/// policy engine and the session model are the same ones, which is the point of
/// having a base at all.
pub const LONGCODE_AIR_SPEC: &str = r#"
# No graph index, no network, no delegation: each is a cost multiplier, and Air
# exists for the work that does not need them.
[[patch]]
id = "code-graph"
disabled = true

[[patch]]
id = "tool-web"
disabled = true

[[patch]]
id = "subagent-in-process"
disabled = true

[[patch]]
id = "mcp"
disabled = true

[[patch]]
id = "skills"
disabled = true

[[patch]]
id = "round-cap"
config = { max_rounds = 12, max_seconds = 0 }

# Compact earlier and keep less: a short context is the single biggest lever on
# cost per turn.
[[patch]]
id = "compaction-tail"
config = { threshold = 0.5, keep_turns = 1 }

[[patch]]
id = "tool-result-cap"
config = { max_bytes = 16384 }

[[patch]]
id = "agent-loop"
config = { max_rounds = 40 }
"#;

/// **Code Security** — a read-only security review that reports findings.
///
/// The read-only guarantee is a *world*, not a policy: `fs-readonly` refuses
/// mutations at the boundary, so it holds even if the approval row is
/// misconfigured or removed. The graph layer is mounted because reachability is
/// the whole job — a sink with no path to it is not a finding.
pub const CODE_SECURITY_SPEC: &str = r#"
[[patch]]
id = "fs"
name = "fs-readonly"

[[patch]]
id = "persona-coding"
disabled = true

[[insert]]
id = "persona"
name = "persona-security"

[[insert]]
name = "tool-report-finding"

# Reporting is not a mutation of anything outside the process, but a read-only
# policy has no way to know that — the tool declares no read-only hint. Allow it
# explicitly, or the auditor can find things and never file them.
[[patch]]
id = "permissions"
config = { allow = ["report_finding"], deny = [] }

[[patch]]
id = "tool-ast-grep"
disabled = false

[[patch]]
id = "code-graph"
disabled = false

[[patch]]
id = "tool-web"
disabled = false

# No shell and no delegation: an audit that can run commands is not an audit.
# The process world comes out too, not just the tool that used it — leaving
# `shell` and `subprocess` mounted would leave the capability one patch away
# instead of absent, and `--audit` reports them as dead weight either way.
[[patch]]
id = "tool-bash-world"
disabled = true

[[patch]]
id = "shell"
disabled = true

[[patch]]
id = "subprocess"
disabled = true

[[patch]]
id = "subagent-in-process"
disabled = true

[[patch]]
id = "approval"
disabled = false
config = { mode = "read-only" }

[[patch]]
id = "approval-interactive"
disabled = true

# Read-only means there is nothing to approve, so nothing to ask about. Leaving
# the asker mounted would be a provider nobody consumes.
[[patch]]
id = "user-questions-unattended"
name = "user-questions-unattended"
disabled = true

[[patch]]
id = "round-cap"
config = { max_rounds = 30, max_seconds = 0 }
"#;

/// **Code Review** — a read-only reviewer that reports findings.
///
/// The same read-only world as the security variant, a different prompt, and
/// the graph layer left off by default: a review reads a diff and its
/// surroundings, and a whole-repo index is usually more than that needs.
pub const CODE_REVIEW_SPEC: &str = r#"
[[patch]]
id = "fs"
name = "fs-readonly"

[[patch]]
id = "persona-coding"
disabled = true

[[insert]]
id = "persona"
name = "persona-review"

[[insert]]
name = "tool-report-finding"

# Reporting is not a mutation of anything outside the process, but a read-only
# policy has no way to know that — the tool declares no read-only hint. Allow it
# explicitly, or the auditor can find things and never file them.
[[patch]]
id = "permissions"
config = { allow = ["report_finding"], deny = [] }

[[patch]]
id = "tool-ast-grep"
disabled = false

[[patch]]
id = "tool-web"
disabled = false

[[patch]]
id = "tool-bash-world"
disabled = true

[[patch]]
id = "shell"
disabled = true

[[patch]]
id = "subprocess"
disabled = true

[[patch]]
id = "subagent-in-process"
disabled = true

[[patch]]
id = "approval"
disabled = false
config = { mode = "read-only" }

[[patch]]
id = "approval-interactive"
disabled = true

# Read-only means there is nothing to approve, so nothing to ask about. Leaving
# the asker mounted would be a provider nobody consumes.
[[patch]]
id = "user-questions-unattended"
name = "user-questions-unattended"
disabled = true

[[patch]]
id = "round-cap"
config = { max_rounds = 24, max_seconds = 0 }
"#;

/// Swap the front end without touching the specialization. `--ui <name>`.
///
/// It moves the asker with the screen: a question drawn on a plain terminal and
/// one drawn inside an alternate screen are different providers, and a front end
/// swap that left the wrong one behind would prompt into a screen nobody can see.
pub fn ui_overlay(name: &str) -> String {
    let asker = match name {
        "tui" => Some("user-questions-tui"),
        "repl" | "oneshot" => Some("user-questions-terminal"),
        // The web and JSON-RPC rows fill the slot themselves — a browser and a
        // client program are both someone who can answer. Only the quiet front
        // end has nobody, and it stands the asking policy down with it.
        _ => None,
    };
    // The `sdk` front end's row is named for what it speaks, not for the
    // profile that uses it.
    let plugin = match name {
        "sdk" => "jsonrpc",
        other => other,
    };
    let mut out = format!("[[patch]]\nid = \"ui\"\nname = \"ui-{plugin}\"\n");
    if name == "quiet" {
        // Nobody to ask and nothing to ask with: the asking policy must stand
        // down too, or it waits forever for a provider that will never mount.
        out.push_str(
            "\n[[patch]]\nid = \"user-questions-unattended\"\nname = \"user-questions-unattended\"\ndisabled = false\n\n\
             [[patch]]\nid = \"approval-interactive\"\ndisabled = true\n\n\
             [[patch]]\nid = \"approval\"\ndisabled = false\n",
        );
        return out;
    }
    match asker {
        Some(asker) => out.push_str(&format!(
            "\n[[patch]]\nid = \"user-questions-unattended\"\nname = \"{asker}\"\ndisabled = false\n"
        )),
        // Two providers for one slot is an error, so the standalone asker row
        // stands down wherever the front end supplies its own.
        None => out.push_str(
            "\n[[patch]]\nid = \"user-questions-unattended\"\nname = \"user-questions-unattended\"\ndisabled = true\n",
        ),
    }
    out
}

/// Front ends `--ui` accepts.
pub const UI_NAMES: &[&str] = &["oneshot", "repl", "tui", "web", "sdk", "quiet"];
