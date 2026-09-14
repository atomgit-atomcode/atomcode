//! AtomCode's remaining production capabilities, each as a row.
//!
//! None of these is reimplemented — they are the same `atomcode-capabilities`
//! types the shipped agent uses. What changes is that each arrives through a
//! plugin that can be disabled, reconfigured, or replaced, and that takes its
//! tools *and* its prompt guidance with it when it goes.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::codeintel::{
    BlastRadiusTool, CodeIndex, FileDependenciesTool, FindReferencesTool, ListSymbolsTool,
    ReadSymbolTool, TraceCalleesTool, TraceCallersTool, TraceChainTool,
};
use atomcode_capabilities::mcp::{McpRegistry, McpToolAdapter};
use atomcode_capabilities::memory::MemoryStore;
use atomcode_capabilities::skills::{
    runtime_skill_dirs, ListSkillsTool, SkillRegistry, UseSkillTool,
};
use atomcode_capabilities::tools::{WebFetchTool, WebSearchTool};
use atomcode_kernel::tool::Tool;
use atomcode_plexus::{Context, Plugin};
use atomcode_review::{ReviewTool, ReviewToolConfig, SharedReviewProvider};
use serde::Deserialize;
use serde_json::Value;

use crate::events::{TurnStart, TurnStarted};
use crate::seams::{CodeIndexSvc, McpSvc, SessionSvc, SkillsSvc};
use crate::session::{InjectionOrigin, SessionEvent};

use super::tools::{contribute_prompt, mount};

fn parse<T: for<'de> Deserialize<'de> + Default>(config: &Value) -> Result<T, String> {
    if config.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))
}

// ---- skills -------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct SkillsRow {
    /// Extra directories to load skills from, on top of the standard ones.
    #[serde(default)]
    dirs: Vec<String>,
    /// Project root the standard per-project skill directories resolve against.
    #[serde(default)]
    project_root: Option<String>,
    /// User home the standard per-user skill directories resolve against
    /// (`~/.claude/skills` and friends). Configurable so a deployment — or a
    /// test — can point the catalog somewhere other than the invoking user.
    #[serde(default)]
    home: Option<String>,
}

pub struct SkillsPlugin;

#[async_trait]
impl Plugin for SkillsPlugin {
    fn name(&self) -> &'static str {
        "skills"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // `use_skill` / `list_skills` hold the registry this row provides.
        &["skills", "operations"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["skills"]
    }
    fn description(&self) -> &'static str {
        "markdown skill catalog + use_skill / list_skills"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: SkillsRow = parse(config)?;
        let project = row
            .project_root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        // A named resolver, not a bare environment read: which directory a
        // person's skills live under is a decision, and it is made in
        // `model_source` beside the other ones about where things live.
        let home = row
            .home
            .map(PathBuf::from)
            .or_else(crate::model_source::user_home)
            .unwrap_or_else(|| PathBuf::from("."));
        let mut dirs = runtime_skill_dirs(&home, &project);
        dirs.extend(row.dirs.iter().map(PathBuf::from));
        let registry = Arc::new(SkillRegistry::load(&dirs));
        let count = registry.len();

        let _ = ctx
            .provide::<SkillsSvc>(registry.clone())
            .map_err(|e| e.to_string())?;
        mount(
            ctx,
            vec![
                Arc::new(UseSkillTool::new(registry.clone())),
                Arc::new(ListSkillsTool::new(registry)),
            ],
        )?;
        // Only advertise skills when some exist: a catalog line promising
        // capabilities that resolve to nothing is worse than no line.
        if count > 0 {
            contribute_prompt(
                ctx,
                "skills",
                60,
                &format!(
                    "{count} skill(s) are available. Call `list_skills` to see them and \
                     `use_skill` to load one before doing work it covers."
                ),
            );
        }
        crate::plugins::self_knowledge::describes(
            ctx,
            "skills",
            13,
            format!(
                "SKILLS — {count} loaded. Markdown files, read from the standard \
                 per-project and per-user skill directories plus anything the \
                 row's `dirs` config adds. To add one, drop a markdown file in \
                 a skill directory and restart; nothing needs recompiling. \
                 `list_skills` shows what is loaded now."
            ),
        );
        Ok(())
    }
}

// ---- codeintel ----------------------------------------------------------

pub struct CodeIntelPlugin;

#[async_trait]
impl Plugin for CodeIntelPlugin {
    fn name(&self) -> &'static str {
        "codeintel"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "stateless tree-sitter symbol tools (the symbol layer)"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        mount(
            ctx,
            vec![
                Arc::new(ListSymbolsTool) as Arc<dyn Tool>,
                Arc::new(ReadSymbolTool),
                Arc::new(FindReferencesTool),
            ],
        )?;
        contribute_prompt(
            ctx,
            "codeintel",
            55,
            "Use `list_symbols` / `read_symbol` to read code by symbol instead of by line range, \
             and `find_references` before changing something shared.",
        );
        Ok(())
    }
}

/// The graph layer, split from the symbol layer because it has a different cost
/// profile: it builds and caches a whole-repo index, so a deployment that only
/// wants symbol reads should not pay for it.
pub struct CodeGraphPlugin;

#[async_trait]
impl Plugin for CodeGraphPlugin {
    fn name(&self) -> &'static str {
        "code-graph"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // The graph tools hold the index this row provides.
        &["code-index"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["code-index"]
    }
    fn description(&self) -> &'static str {
        "cross-file call graph: references, call chains, blast radius"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        // One shared, lazily-built index behind a service: every graph tool
        // reads the same cache, and a different indexer could fill the slot.
        let index = Arc::new(CodeIndex::new());
        let _ = ctx
            .provide::<CodeIndexSvc>(index.clone())
            .map_err(|e| e.to_string())?;
        mount(
            ctx,
            vec![
                Arc::new(TraceCallersTool::new(index.clone())) as Arc<dyn Tool>,
                Arc::new(TraceCalleesTool::new(index.clone())),
                Arc::new(TraceChainTool::new(index.clone())),
                Arc::new(BlastRadiusTool::new(index.clone())),
                Arc::new(FileDependenciesTool::new(index)),
            ],
        )?;
        contribute_prompt(
            ctx,
            "code-graph",
            56,
            "Before changing a shared function, use `trace_callers` or `blast_radius` to see \
             what depends on it.",
        );
        Ok(())
    }
}

// ---- web ----------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct WebRow {
    /// Which search backend answers `web_search`. Unset falls back to the
    /// `ATOMCODE_WEB_SEARCH_PROVIDER` environment knob, and then to the tool's
    /// own default; an unknown name maps to that default rather than failing.
    #[serde(default)]
    provider: Option<String>,
}

pub struct WebPlugin;

#[async_trait]
impl Plugin for WebPlugin {
    fn name(&self) -> &'static str {
        "tool-web"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "web search and fetch"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: WebRow = parse(config)?;
        // Told the process is offline, this row mounts nothing. The tools
        // themselves do not refuse — they would try, fail, and tell the model
        // the internet is broken — and a persona that has already said there is
        // no public network while two tools sit in the catalog promising one is
        // worse than either. Same judgement the hand-written chain makes, at the
        // same place: whether to OFFER the capability.
        if atomcode_config::config::offline::is_offline_active() {
            return Ok(());
        }
        let provider = row
            .provider
            .clone()
            .filter(|p| !p.trim().is_empty())
            .or_else(crate::model_source::web_search_provider);
        let search = match &provider {
            Some(name) => WebSearchTool::with_provider(name),
            None => WebSearchTool::new(),
        };
        mount(
            ctx,
            vec![Arc::new(search) as Arc<dyn Tool>, Arc::new(WebFetchTool)],
        )?;
        contribute_prompt(
            ctx,
            "tool-web",
            61,
            "`web_search` and `web_fetch` reach the public internet. Prefer them over guessing \
             at an API you cannot read locally.",
        );
        crate::plugins::self_knowledge::describes(
            ctx,
            "web",
            61,
            format!(
                "WEB — `web_search` and `web_fetch` reach the public internet. The search \
                 backend is {backend}; set it with this row's `provider` config or the \
                 `ATOMCODE_WEB_SEARCH_PROVIDER` environment variable, and an unknown name \
                 falls back to the default rather than failing. When the process is in \
                 offline mode this row mounts nothing at all, so neither tool appears in \
                 the catalog.",
                backend = provider.as_deref().unwrap_or("the default"),
            ),
        );
        Ok(())
    }
}

// ---- review, as a capability of the agent --------------------------------

#[derive(Debug, Deserialize, Default)]
struct ReviewRow {
    /// What the child reviewer is told it is running.
    ///
    /// Config rather than "just read the seam", and that is the whole design of
    /// this row: `App::patch` remounts only rows whose OWN entry changed, and
    /// `Fibers::unload` cascades to children rather than to consumers — so a
    /// tree that swaps `llm` would leave this row holding the provider it was
    /// given at mount. A tree that swaps models patches this row's `model` in
    /// the same layer (the coding tree does, beside its persona), and the
    /// remount picks up the new provider with the new name. Left unset it is
    /// whatever the `llm` seam answers at mount.
    #[serde(default)]
    model: Option<String>,
    /// Unset ⇒ the provider's own window, else the tool's default.
    #[serde(default)]
    context_window: Option<u32>,
    /// Per-language review rules. Unset ⇒ the built-in ones only.
    #[serde(default)]
    rules_dir: Option<String>,
}

/// The `code_review` tool: a read-only child reviewer over the current changes.
///
/// Not a second agent product — the reviewer is one tool inside THIS agent, the
/// way `task` is. It reuses the host's provider on purpose: a reviewer that
/// built its own would miss a signing gateway and fail where the conversation
/// around it works.
pub struct ReviewToolPlugin;

#[async_trait]
impl Plugin for ReviewToolPlugin {
    fn name(&self) -> &'static str {
        "tool-code-review"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "llm"]
    }
    fn description(&self) -> &'static str {
        "the `code_review` tool: a read-only reviewer over the current changes, \
         running its own rounds on the host's provider"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: ReviewRow = parse(config)?;
        let provider = ctx
            .service::<crate::seams::LlmSvc>()
            .ok_or("the `llm` seam must be filled before `tool-code-review`")?;
        let defaults = ReviewToolConfig::default();
        let cfg = ReviewToolConfig {
            model: row
                .model
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| provider.model_name().to_string()),
            context_window: row
                .context_window
                .or_else(|| Some(provider.context_window()))
                .filter(|w| *w > 0)
                .unwrap_or(defaults.context_window),
            rules_dir: row.rules_dir.map(PathBuf::from),
            ..defaults
        };
        let slot: SharedReviewProvider = Arc::new(std::sync::RwLock::new(Some(provider)));
        mount(
            ctx,
            vec![Arc::new(ReviewTool::new(slot, cfg)) as Arc<dyn Tool>],
        )?;
        contribute_prompt(
            ctx,
            "tool-code-review",
            58,
            "`code_review` runs a reviewer over the current changes and reports what it \
             finds. It reads; it never edits. Use it before handing work back — not \
             instead of running the tests.",
        );
        Ok(())
    }
}

// ---- memory -------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct MemoryRow {
    #[serde(default)]
    project_root: Option<String>,
}

/// User memory, injected as a **logged fact** rather than a hidden prepend.
///
/// The production stack injects memory through a lifecycle hook, which means the
/// text reaches the model without being a session event. Here it is appended to
/// the log with its provenance, so the invariant holds and a transcript shows
/// exactly what the model was told and why.
pub struct MemoryPlugin;

#[async_trait]
impl Plugin for MemoryPlugin {
    fn name(&self) -> &'static str {
        "memory"
    }
    fn uses(&self) -> &'static [&'static str] {
        // The tool half is optional: a tree with no catalog still gets the
        // injection, which is the half that works with no model cooperation.
        &["tools", "operations"]
    }
    fn description(&self) -> &'static str {
        "inject memory.md on the first turn, and let the agent write it back"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: MemoryRow = parse(config)?;
        let project = row
            .project_root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));
        let project_name = project
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into());
        let merged = MemoryStore::merged_for_prompt(
            &MemoryStore::global(),
            &MemoryStore::project(&project),
            &MemoryStore::local(&project),
            &project_name,
        );
        // The write half. Until this the agent could read the user's memory but
        // never add to it, which made "remember that I prefer X" a request only
        // a human could carry out — in a system whose whole point is that the
        // agent carries things out.
        if let Some(toolbox) = ctx.service::<crate::seams::ToolsSvc>() {
            toolbox.register(Arc::new(atomcode_capabilities::tools::MemoryTool))?;
            let toolbox = toolbox.clone();
            let _ = ctx.effect(move || toolbox.unregister("memory"));
        }

        crate::plugins::self_knowledge::describes(
            ctx,
            "memory",
            11,
            format!(
                "MEMORY — three tiers, merged on the first turn of every session:\n\
                 \u{20}\u{20}global   {}   (every project)\n\
                 \u{20}\u{20}project  {}   (this repo, committed)\n\
                 \u{20}\u{20}local    {}   (this repo on this machine only)\n\
                 Write with the `memory` tool: \
                 `{{action: remember|forget|list, content, scope: project|local|global}}`. \
                 Memory is what someone chose to state; for everything that was \
                 merely *said*, use `recall` instead.",
                MemoryStore::global().path().display(),
                MemoryStore::project(&project).path().display(),
                MemoryStore::local(&project).path().display(),
            ),
        );

        if merged.is_empty() {
            return Ok(());
        }

        let ctx = ctx.clone();
        let _ = ctx
            .clone()
            .on_emit::<TurnStart>(move |started: &TurnStarted| {
                // First turn only: a resumed log already carries the injection, and
                // repeating it every turn would break the cacheable prefix.
                if started.turn != 1 {
                    return;
                }
                let Some(session) = crate::agent::scoped(&ctx).service::<SessionSvc>() else {
                    return;
                };
                crate::session::commit(
                    &ctx,
                    &session,
                    SessionEvent::Injected {
                        turn: started.turn,
                        text: merged.clone(),
                        origin: InjectionOrigin::Memory,
                    },
                );
            });
        Ok(())
    }
}

// ---- mcp ----------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct McpRow {
    /// Project root whose `.mcp.json` / configured servers are loaded.
    #[serde(default)]
    project_root: Option<String>,
    /// How long to wait for the initial connections before mounting whatever
    /// answered. Servers that arrive later are simply not in this session's
    /// catalog — a slow server must not hold up a turn.
    #[serde(default = "default_connect_ms")]
    connect_timeout_ms: u64,
}

fn default_connect_ms() -> u64 {
    10_000
}

/// External MCP servers, discovered and mounted as ordinary tools.
///
/// This is the row that makes "plugins are compiled in" a smaller limitation
/// than it sounds: an MCP server is a process someone else wrote, in a language
/// this binary knows nothing about, and it arrives through the same tool catalog
/// as everything else.
pub struct McpPlugin;

#[async_trait]
impl Plugin for McpPlugin {
    fn name(&self) -> &'static str {
        "mcp"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // Every mounted adapter calls back into the registry this row provides.
        &["mcp", "operations"]
    }
    fn provides(&self) -> &'static [&'static str] {
        &["mcp"]
    }
    fn description(&self) -> &'static str {
        "connect configured MCP servers and mount their tools"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: McpRow = parse(config)?;
        let project = row
            .project_root
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        let registry = Arc::new(McpRegistry::from_config(&project).await);
        registry
            .wait_for_initial_connections(std::time::Duration::from_millis(row.connect_timeout_ms))
            .await;

        let infos = registry.list_all_tools().await;
        let mut mounted = Vec::new();
        let mut adapters: Vec<Arc<dyn Tool>> = Vec::new();
        for info in infos {
            // A server that collides with an approved name fails closed rather
            // than shadowing it; skip that tool and keep the rest.
            match McpToolAdapter::new(registry.clone(), info) {
                Ok(adapter) => {
                    mounted.push(adapter.name().to_string());
                    adapters.push(Arc::new(adapter));
                }
                Err(reason) => eprintln!("mcp: skipping a tool: {reason}"),
            }
        }
        let _ = ctx
            .provide::<McpSvc>(registry.clone())
            .map_err(|e| e.to_string())?;
        if adapters.is_empty() {
            return Ok(());
        }
        mount(ctx, adapters)?;
        if let Some(instructions) = registry.instructions_for_mounted_tools(&mounted) {
            contribute_prompt(ctx, "mcp", 62, &instructions);
        }
        {
            crate::plugins::self_knowledge::describes(
                ctx,
                "mcp",
                14,
                "MCP — how third-party tools get in without recompiling. Each \
                 server is an entry on the `mcp` row's config (`command`/`args` \
                 for stdio, `url` for HTTP); its tools join the catalog under \
                 their own names and go through the same approval as everything \
                 else. This is the closest thing to installing a plugin at \
                 runtime; compiled rows require a build.",
            );
        }
        Ok(())
    }
}
