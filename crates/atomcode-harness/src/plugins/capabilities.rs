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

/// `skills-advert`: the one sentence that tells the model skills exist.
///
/// Its own row rather than a paragraph inside [`SkillsPlugin`], because a
/// product that says something better — coding lists the whole catalog — has to
/// be able to turn this off, and the only honest way to turn a fragment off is
/// to not mount the row that writes it. Contributing under a shared id instead
/// made the override invisible in the row list and, worse, tied the two rows'
/// lifetimes together: whichever left first took the other's text with it
/// (`docs/adr/0019`, `atomcode-coding/tests/prompt_fragments.rs`).
pub struct SkillsAdvertPlugin;

#[async_trait]
impl Plugin for SkillsAdvertPlugin {
    fn name(&self) -> &'static str {
        "skills-advert"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["skills", "system-prompt"]
    }
    fn description(&self) -> &'static str {
        "tell the model how many skills are installed and how to load one"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let Some(skills) = ctx.service::<SkillsSvc>() else {
            return Ok(());
        };
        // Only advertise skills when some exist: a catalog line promising
        // capabilities that resolve to nothing is worse than no line.
        let count = skills.len();
        if count > 0 {
            contribute_prompt(
                ctx,
                "skills-advert",
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
        // `use_skill` / `list_skills` hold the registry this row provides;
        // `commands` is where the `/skills` listing is registered.
        &["skills", "operations", "commands"]
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
                Arc::new(ListSkillsTool::new(registry.clone())),
            ],
        )?;
        // And a command, so a person can see what is installed without asking
        // the model to call a tool on their behalf
        // (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` B1,
        // `docs/adr/0021` §10). The row that owns the capability registers it;
        // nothing in the screen knows this command exists.
        //
        // The same call a host's own skills row makes — a row that replaces this
        // one has to take over what it did, and this is where that is stated
        // once (`register_skill_commands`).
        register_skill_commands(ctx, &registry)?;
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

/// `tool-web`, and `tool-web-keyed` when a host has an Exa key to give it.
///
/// The key is a field of the plugin instance, never row config: a config tree is
/// data that gets printed (`--dump-config` renders every row's config verbatim),
/// and a credential in it is a credential on somebody's screen. A host that holds
/// a key registers [`WebPlugin::with_api_key`] and swaps the row onto it; the
/// row keeps its id, so a `provider` patch still lands.
pub struct WebPlugin {
    api_key: Option<String>,
}

impl WebPlugin {
    /// The catalog's `tool-web`: the key, if any, comes from `EXA_API_KEY`.
    pub const fn new() -> Self {
        Self { api_key: None }
    }

    /// `tool-web-keyed`: a key the host read from somewhere a row cannot see.
    /// `EXA_API_KEY` still wins when it is set.
    pub fn with_api_key(api_key: String) -> Self {
        Self {
            api_key: Some(api_key),
        }
    }
}

#[async_trait]
impl Plugin for WebPlugin {
    fn name(&self) -> &'static str {
        if self.api_key.is_some() {
            "tool-web-keyed"
        } else {
            "tool-web"
        }
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
            Some(name) => WebSearchTool::with_provider_and_key(name, self.api_key.clone()),
            None => WebSearchTool::with_provider_and_key("exa", self.api_key.clone()),
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
                 backend is {backend}; when nothing configured this \
                 row, the `ATOMCODE_WEB_SEARCH_PROVIDER` environment variable picks it, and \
                 an unknown name falls back to the default rather than failing. An Exa key \
                 comes from `EXA_API_KEY`, else from the host, else Exa runs keyless. \
                 When the process is in offline mode this row mounts nothing at all, so \
                 neither tool appears in the catalog.",
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
/// `/review` over a `code_review` tool, whoever mounted it.
///
/// Public for the same reason as [`register_skill_commands`]: a product that
/// mounts its own reviewer — `atomcode-coding` disables `tool-code-review` and
/// mounts the product's `code_review`, with the product's limits and provider
/// slot — owns the command as well. It once mounted the tool and not this, so
/// on the product `/review` did not exist.
pub fn register_review_command(ctx: &Context, tool: Arc<dyn Tool>) -> Result<(), String> {
    crate::commands::register(ctx, Arc::new(ReviewCommand(tool)))
}

/// `review`: the reviewer over the current changes, run by a person.
///
/// The same tool the row mounted, with the same rules and the same provider
/// slot. What "the current changes" means — staged, a base, a range — is the
/// tool's own argument, passed through as typed.
struct ReviewCommand(Arc<dyn Tool>);

#[async_trait]
impl crate::commands::CatalogCommand for ReviewCommand {
    fn describe(&self) -> atomcode_kernel::agent::CommandDescription {
        atomcode_kernel::agent::CommandDescription {
            name: "review".into(),
            usage: Some("[staged | <base>]".into()),
            summary: "让评审员看一遍现在的改动;只读,不改".into(),
            target: atomcode_kernel::agent::CommandTarget::Session,
        }
    }

    async fn run(&self, agent: Arc<crate::agent::Agent>, args: &str) -> Result<String, String> {
        use atomcode_kernel::tool::ToolContext;
        let scope = args.trim();
        let mut call = serde_json::Map::new();
        if !scope.is_empty() {
            call.insert("scope".into(), serde_json::Value::String(scope.to_string()));
        }
        let result = self
            .0
            .execute(
                &serde_json::Value::Object(call).to_string(),
                &ToolContext {
                    working_dir: agent
                        .ctx()
                        .service::<crate::seams::FsSvc>()
                        .map(|fs| fs.root())
                        .unwrap_or_else(|| PathBuf::from(".")),
                    cancel: Default::default(),
                    progress: atomcode_kernel::tool::ProgressSink::noop(),
                    requester: None,
                },
            )
            .await;
        if result.is_error {
            return Err(result.content);
        }
        Ok(result.content)
    }
}

pub struct ReviewToolPlugin;

#[async_trait]
impl Plugin for ReviewToolPlugin {
    fn name(&self) -> &'static str {
        "tool-code-review"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools", "llm"]
    }
    fn uses(&self) -> &'static [&'static str] {
        // `commands` carries the `/review` a person runs; `fs` says which
        // directory the reviewer reads.
        &["commands", "fs"]
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
        let tool = Arc::new(ReviewTool::new(slot, cfg));
        mount(ctx, vec![tool.clone() as Arc<dyn Tool>])?;
        // And as a command a person runs, through the same tool
        // (`docs/adr/0021` §10,
        // `docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` B1):
        // "review what I changed" is a thing a person asks for directly, and
        // asking the model to call a tool on their behalf spends a turn to
        // reach the same reviewer.
        register_review_command(ctx, tool)?;
        // This row's guidance for this row's tool. It lived in the coding persona as
        // `## CODE REVIEW`, which described the tool on BOTH assemblies — and stayed describing
        // it after this row was patched out of the tree.
        contribute_prompt(
            ctx,
            "tool-code-review",
            58,
            "`code_review` runs a reviewer over the current changes and reports what it \
             finds. It reads; it never edits. Use it before handing work back — not \
             instead of running the tests. When the person asks to review code, a diff, staged \
             changes, a commit, or a branch range, call it before writing the review and pass \
             the requested scope and path filters straight to it rather than pre-reading the \
             diff with ordinary read/search tools, which is what the scoped reviewer is for. It \
             may report findings you did not find; weigh them. Do not claim it fixed files or \
             posted comments — it does neither.",
        );
        Ok(())
    }
}

// ---- memory -------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MemoryRow {
    #[serde(default)]
    project_root: Option<String>,
    /// Put memory in front of the model on the first turn. Off leaves the
    /// `memory` tool mounted — writing a memory and being shown the stored ones
    /// are separate decisions, and a host that turns off the second (a one-shot
    /// run that must not depend on someone's notes) still wants the first.
    #[serde(default = "yes")]
    inject: bool,
    /// The global tier's file. Defaults to `$ATOMCODE_HOME/memory.md`.
    ///
    /// A file rather than a home directory, and deliberately not called `home`:
    /// the `skills` row's `home` is the person's `$HOME`, while this file lives
    /// under AtomCode's own home. One word meaning two directories in two rows is
    /// how a product built on the harness ends up with its users' memory in the
    /// wrong place.
    #[serde(default)]
    global: Option<String>,
}

impl Default for MemoryRow {
    fn default() -> Self {
        Self {
            project_root: None,
            inject: true,
            global: None,
        }
    }
}

fn yes() -> bool {
    true
}

/// 把一份 skill 目录变成命令：`/skills`，以及每个可被调用的 skill 一条。
///
/// **一行一次，而不是每个装配各写一遍。** 这件事原本只住在 [`SkillsPlugin`] 里，
/// 而 coding 装配把那一行换成了自己的 `skills-host`——于是那套装配里一条 skill 命令
/// 都没有（`/setup` 因此回了 `NotFound`，任何人写在 `.atomcode/skills/` 里的 skill
/// 也一样进不了目录）。要换掉一行就得接过它该说的话；把一个机制留在某个具体行里，
/// 换行的人看不见它，只能靠踩一次才知道。
///
/// 两件事都在这里：`/skills`（人能看装了什么，不必让模型替他调工具）和
/// **每个 `user_invocable` 的 skill 一条**——`/init`、`/setup` 和任何人自己写的
/// 那条，都不必由这一行或屏幕知道它们的名字。
pub fn register_skill_commands(ctx: &Context, registry: &Arc<SkillRegistry>) -> Result<(), String> {
    crate::commands::register(ctx, Arc::new(ListSkills(registry.clone())))?;
    for skill in registry.user_invocable() {
        // A cheap early skip when the name is already taken — a skill called
        // `compact` must not take the host's `/compact`. The catalog is the
        // real arbiter, not this line: `RunSkill::is_skill()` makes a skill
        // yield to a built-in of the same name in either registration order and
        // never error (see `CommandCatalog::register`), so a skill that raced
        // ahead of its built-in — the `/memory` case, where skills mount before
        // the memory row — is still evicted rather than crashing the tree. This
        // skip only saves registering-then-yielding when the built-in got there
        // first.
        if catalog_has(ctx, &bare_name(&skill.name)) {
            continue;
        }
        crate::commands::register(ctx, Arc::new(RunSkill(skill)))?;
    }
    Ok(())
}

/// The name a person types for a skill: `skills:init` is typed `/init`.
///
/// The namespace is how the registry keeps two skills of the same name apart;
/// it is not something anyone wants to type.
fn bare_name(name: &str) -> String {
    name.rsplit(':').next().unwrap_or(name).to_string()
}

/// Whether the catalog already offers this name.
fn catalog_has(ctx: &Context, name: &str) -> bool {
    ctx.service::<crate::seams::CommandsSvc>()
        .is_some_and(|catalog| catalog.has(name))
}

/// One skill, as a command a person runs.
///
/// Running it queues the expanded skill as **the person's own message**: a
/// skill is a prompt someone wrote to send, and sending it is a turn like any
/// other — logged as theirs, answerable, undoable.
struct RunSkill(Arc<atomcode_capabilities::skills::Skill>);

#[async_trait]
impl crate::commands::CatalogCommand for RunSkill {
    fn describe(&self) -> atomcode_kernel::agent::CommandDescription {
        atomcode_kernel::agent::CommandDescription {
            name: bare_name(&self.0.name),
            usage: Some("[给它的话]".into()),
            summary: self.0.description.clone(),
            target: atomcode_kernel::agent::CommandTarget::Session,
        }
    }

    async fn run(&self, agent: Arc<crate::agent::Agent>, args: &str) -> Result<String, String> {
        let text = self.0.expand(args.trim(), agent.session_id());
        if text.trim().is_empty() {
            return Err(format!("`{}` 展开之后是空的", bare_name(&self.0.name)));
        }
        agent.send(text);
        Ok(format!("按 `{}` 开始", bare_name(&self.0.name)))
    }

    // Generated from a SKILL.md on disk: it yields its name to any built-in of
    // the same name so a skill called `memory` cannot take `/memory` or break
    // the assembly. Still reachable through `use_skill`.
    fn is_skill(&self) -> bool {
        true
    }
}

/// `skills`: what is installed, for a person rather than for the model.
///
/// The catalog the row already holds, listed by name and by what each is for.
/// Not the tool: `list_skills` answers the *model*, and a person asking "what
/// do I have" should not have to spend a turn to find out.
struct ListSkills(Arc<SkillRegistry>);

#[async_trait]
impl crate::commands::CatalogCommand for ListSkills {
    fn describe(&self) -> atomcode_kernel::agent::CommandDescription {
        atomcode_kernel::agent::CommandDescription {
            name: "skills".into(),
            usage: None,
            summary: "装了哪些 skill,各是干什么的".into(),
            target: atomcode_kernel::agent::CommandTarget::Session,
        }
    }

    async fn run(&self, _agent: Arc<crate::agent::Agent>, _args: &str) -> Result<String, String> {
        let listed = self.0.list();
        if listed.is_empty() {
            return Ok("一个 skill 都没装".into());
        }
        Ok(listed
            .into_iter()
            .map(|(name, about)| format!("{name} · {about}"))
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

/// One of the memory actions, as a command a person runs.
///
/// Carried out by the `memory` tool the row already mounted: which tier a
/// remembered line goes in, and how a forget matches, are decisions that exist
/// once. A command that reimplemented them would agree with the tool until one
/// of the two changed.
struct MemoryCommand {
    name: &'static str,
    usage: Option<&'static str>,
    summary: &'static str,
    action: &'static str,
    project: PathBuf,
    /// The global tier's file, as the row resolved it.
    ///
    /// Carried rather than looked up, for the same reason the tool takes one: a
    /// product that keeps its users' memory outside `$ATOMCODE_HOME` names the
    /// file on the row, and a command that went back to `MemoryStore::global()`
    /// would write somewhere else than the tool the model uses — the two doors
    /// onto one memory disagreeing about where it is.
    global: PathBuf,
}

#[async_trait]
impl crate::commands::CatalogCommand for MemoryCommand {
    fn describe(&self) -> atomcode_kernel::agent::CommandDescription {
        atomcode_kernel::agent::CommandDescription {
            name: self.name.into(),
            usage: self.usage.map(str::to_string),
            summary: self.summary.into(),
            target: atomcode_kernel::agent::CommandTarget::Session,
        }
    }

    async fn run(&self, _agent: Arc<crate::agent::Agent>, args: &str) -> Result<String, String> {
        use atomcode_kernel::tool::ToolContext;
        let content = args.trim();
        if self.action != "list" && content.is_empty() {
            return Err(format!("要有话可{}", self.summary));
        }
        let mut call = serde_json::json!({ "action": self.action });
        if !content.is_empty() {
            // Under the name this action reads it by. `forget` takes a
            // `keyword` and `remember` a `content`, and sending one as the
            // other is refused by the tool — which is what `/forget` did from
            // the day it was added until a criterion ran it
            // (`a_product_can_keep_its_users_state_out_of_the_atomcode_home`).
            let key = if self.action == "forget" {
                "keyword"
            } else {
                "content"
            };
            call[key] = serde_json::Value::String(content.to_string());
        }
        let result = atomcode_capabilities::tools::MemoryTool::with_global(self.global.clone())
            .execute(
                &call.to_string(),
                &ToolContext {
                    working_dir: self.project.clone(),
                    cancel: Default::default(),
                    progress: atomcode_kernel::tool::ProgressSink::noop(),
                    requester: None,
                },
            )
            .await;
        if result.is_error {
            return Err(result.content);
        }
        Ok(result.content)
    }
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
        // `commands` carries the three a person runs.
        &["tools", "operations", "commands"]
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
        // Resolved once, and handed to both halves: the injection that reads the
        // global tier and the tool that writes it must agree on which file it is.
        let global = row
            .global
            .map(PathBuf::from)
            .unwrap_or_else(|| MemoryStore::global().path().to_path_buf());
        let merged = MemoryStore::merged_for_prompt(
            &MemoryStore::new(global.clone()),
            &MemoryStore::project(&project),
            &MemoryStore::local(&project),
            &project_name,
        );
        // The write half. Until this the agent could read the user's memory but
        // never add to it, which made "remember that I prefer X" a request only
        // a human could carry out — in a system whose whole point is that the
        // agent carries things out.
        super::tools::mount_optional(
            ctx,
            vec![Arc::new(
                atomcode_capabilities::tools::MemoryTool::with_global(global.clone()),
            )],
        )?;

        // The same three actions as commands a person runs. Through the tool
        // rather than beside it: what "remember" means — which tier it writes,
        // how it dedupes — is decided once, and a second implementation would
        // agree until one of them changed
        // (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md` B1).
        for command in [
            Arc::new(MemoryCommand {
                name: "memory",
                usage: None,
                summary: "存下来的那些话",
                action: "list",
                project: project.clone(),
                global: global.clone(),
            }) as Arc<dyn crate::commands::CatalogCommand>,
            Arc::new(MemoryCommand {
                name: "remember",
                usage: Some("<要记住的话>"),
                summary: "记住一句话,以后每个会话都带着",
                action: "remember",
                project: project.clone(),
                global: global.clone(),
            }),
            Arc::new(MemoryCommand {
                name: "forget",
                usage: Some("<要忘掉的话>"),
                summary: "把记住的某句话删掉",
                action: "forget",
                project: project.clone(),
                global: global.clone(),
            }),
        ] {
            crate::commands::register(ctx, command)?;
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
                global.display(),
                MemoryStore::project(&project).path().display(),
                MemoryStore::local(&project).path().display(),
            ),
        );

        if merged.is_empty() || !row.inject {
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
