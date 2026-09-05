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
        &["skills"]
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
        let home = row
            .home
            .map(PathBuf::from)
            .or_else(|| std::env::var("HOME").ok().map(PathBuf::from))
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
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        mount(
            ctx,
            vec![
                Arc::new(WebSearchTool::new()) as Arc<dyn Tool>,
                Arc::new(WebFetchTool),
            ],
        )?;
        contribute_prompt(
            ctx,
            "tool-web",
            61,
            "`web_search` and `web_fetch` reach the public internet. Prefer them over guessing \
             at an API you cannot read locally.",
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
    fn inject(&self) -> &'static [&'static str] {
        &["sessions"]
    }
    fn description(&self) -> &'static str {
        "inject the user's memory.md as a logged fact on the first turn"
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
                let Some(session) = ctx.service::<SessionSvc>() else {
                    return;
                };
                session.append(SessionEvent::Injected {
                    turn: started.turn,
                    text: merged.clone(),
                    origin: InjectionOrigin::Memory,
                });
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
        &["mcp"]
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
        Ok(())
    }
}
