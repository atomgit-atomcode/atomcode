//! Tool plugins built on AtomCode's production tool implementations.
//!
//! `tool-fs` is the **alternative** to `tool-fs-world`: same four tool names,
//! full production behaviour, but it talks to the local disk directly instead of
//! going through the `fs` seam. Exactly one of the two may be mounted — the tool
//! catalog refuses a duplicate name — which is the honest way to express "you
//! can have sandbox portability or you can have the battle-tested local
//! implementation, and the config picks".
//!
//! `tool-search` is orthogonal to both: grep and glob are search, not a view of
//! one execution world, so they mount alongside either.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::{
    AstGrepTool, BashTool, EditFileTool, GlobTool, GrepTool, ListDirTool, ReadFileTool,
    WriteFileTool,
};

use atomcode_kernel::tool::Tool;
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

use crate::seams::{SystemPromptSvc, ToolBox, ToolsSvc};

/// Register `tools` into the catalog and file the matching removal with the
/// fiber. Shared by every tool plugin — the one place that knows a tool mount
/// is two halves.
///
/// **Public across crates on purpose.** Rows live outside this crate too — the
/// coding runtime mounts its own tools from `atomcode-coding` — and a door a
/// caller cannot reach is a door that gets copied. Every copy is a chance to
/// forget the second half and leave a tool in the catalog after its row is
/// gone; five rows had copied it by hand before this was public — `docs/adr/0019`
/// counted four of them in 2026-09-14, and the fifth arrived after.
///
/// One caller is deliberately not here: `publish_mcp` in `atomcode-coding`
/// republishes a server's tools many times over one row's life and files a
/// single removal for the lot when the row leaves, so a per-tool effect each
/// time would pile up. That shape is the exception the guard in
/// `tests/tool_mounts.rs` names.
pub fn mount(ctx: &Context, tools: Vec<Arc<dyn Tool>>) -> Result<(), String> {
    let toolbox = ctx.require::<ToolsSvc>().map_err(|e| e.to_string())?;
    mount_into(ctx, &toolbox, tools)
}

/// [`mount`] for a row whose tool half is optional: no catalog is not an error,
/// it is one less half to do.
///
/// The rows that say who the agent is mount this way — they must still answer
/// in a tree that mounts no tools at all, an eval harness say.
pub fn mount_optional(ctx: &Context, tools: Vec<Arc<dyn Tool>>) -> Result<(), String> {
    let Some(toolbox) = ctx.service::<ToolsSvc>() else {
        return Ok(());
    };
    mount_into(ctx, &toolbox, tools)
}

/// The two halves themselves, so the two doors cannot drift apart.
fn mount_into(
    ctx: &Context,
    toolbox: &Arc<ToolBox>,
    tools: Vec<Arc<dyn Tool>>,
) -> Result<(), String> {
    let owner = ctx.entry();
    for tool in tools {
        let name = tool.name().to_string();
        // The row's own id travels with the tool: a host narrowing the catalog
        // can then say `tool-fs-world:read_file` and leave everyone else's
        // `read_file` alone (`ToolPolicy`).
        toolbox.register_from(&owner, tool)?;
        let toolbox = toolbox.clone();
        let _ = ctx.effect(move || toolbox.unregister(&name));
    }
    Ok(())
}

/// Contribute a prompt fragment that disappears with its plugin.
///
/// Two halves again, and the second one is the whole point: `PromptRegistry`
/// keys by id and has no idea who contributed what, so the `ctx.effect` filed
/// here is the only thing that takes a row's text out of the prompt when the
/// row goes. A row that writes the halves by hand can write one and stop —
/// two product rows had, and a live patch that disabled them left their text
/// in front of the model (`atomcode-coding/tests/prompt_fragments.rs`).
///
/// **Public across crates**, for the same reason [`mount`] is.
///
/// The exception, as with [`mount`]: `reload_skills_live` in `atomcode-coding`
/// re-contributes under a live row's id and removes it by hand when the catalog
/// empties. It is not mounting anything, so there is no row for it to leave
/// with.
pub fn contribute_prompt(ctx: &Context, id: &str, rank: i32, text: &str) {
    let Some(prompts) = ctx.service::<SystemPromptSvc>() else {
        return;
    };
    prompts.contribute(id, rank, text);
    let id = id.to_string();
    let prompts = prompts.clone();
    let _ = ctx.effect(move || prompts.remove(&id));
}

#[derive(Debug, Deserialize, Default)]
struct FsRow {
    /// Whether `read_file` may hand an image back as a picture. A model-capability
    /// decision, so it is config, not code.
    #[serde(default)]
    vision: bool,
}

pub struct FsToolsPlugin;

#[async_trait]
impl Plugin for FsToolsPlugin {
    fn name(&self) -> &'static str {
        "tool-fs"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["product-dirs", "tools"]
    }
    fn description(&self) -> &'static str {
        "read/write/edit/list against the local disk (the alternative to tool-fs-world)"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: FsRow = if config.is_null() {
            FsRow::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        mount(
            ctx,
            vec![
                Arc::new(ReadFileTool::new(row.vision)),
                Arc::new(WriteFileTool::default()),
                Arc::new(EditFileTool::default()),
                Arc::new(ListDirTool::new((*crate::product_dirs(ctx)?).clone())),
            ],
        )?;
        contribute_prompt(
            ctx,
            "tool-fs",
            50,
            "Read a file before editing it. Prefer `edit_file` over rewriting a whole file with \
             `write_file`, and prefer `grep`/`glob` over listing directories by hand.",
        );
        Ok(())
    }
}

pub struct BashToolPlugin;

pub(super) fn shell_output_provider(
    ctx: &Context,
) -> atomcode_capabilities::tools::output_artifact::ArtifactStoreProvider {
    let ctx = ctx.clone();
    Arc::new(move || ctx.service::<crate::seams::OutputArtifactsSvc>())
}

#[async_trait]
impl Plugin for BashToolPlugin {
    fn name(&self) -> &'static str {
        "tool-bash"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["product-dirs", "tools"]
    }
    fn uses(&self) -> &'static [&'static str] {
        &["output-artifacts"]
    }
    fn description(&self) -> &'static str {
        "shell execution"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let sensitive =
            atomcode_capabilities::tools::SensitivePaths::of(&*crate::product_dirs(ctx)?);
        let provider = shell_output_provider(ctx);
        mount(
            ctx,
            atomcode_capabilities::tools::powershell::PowerShellTool::tools(
                Arc::new(atomcode_capabilities::world::LocalShell),
                sensitive.clone(),
                provider.clone(),
            )
            .await,
        )?;
        let bash = BashTool::new(sensitive.clone()).with_output_preview_provider(provider.clone());
        let poll = Arc::new(
            atomcode_capabilities::tools::bash::BashPollTool::with_output_preview_provider(
                provider.clone(),
            ),
        );
        // The background trio beside it: a dev server, a watcher or anything
        // else meant to keep running goes through `bash_start`, not `bash`.
        mount(
            ctx,
            vec![
                Arc::new(bash),
                Arc::new(
                    atomcode_capabilities::tools::bash::BashStartTool::new(sensitive)
                        .with_output_preview_provider(provider),
                ),
                poll,
                Arc::new(atomcode_capabilities::tools::bash::BashKillTool),
            ],
        )?;
        contribute_prompt(
            ctx,
            "tool-bash",
            51,
            "Use `bash` for builds, tests, and git. Quote paths that contain spaces. \
             Anything meant to keep running — a dev server, a watcher, `npm run dev` — goes through \
             `bash_start`: read its output with `bash_poll` and stop it with `bash_kill`. \
             Long builds/tests also use `bash_start`, then `bash_poll` with wait_ms up to 60000 \
             (returns early on output or exit), instead of shell sleep or duplicate builds. Do not \
             background it inside `bash` (`&`, `nohup`, `start`); a `bash` call is for a command that \
             finishes. Preserve the original build/test exit status; output-filtering or \
             status-printing wrappers can hide failures.",
        );
        Ok(())
    }
}

/// Structural search. Separate from grep because it is a different question:
/// grep finds text, `ast_grep` finds shapes, and an audit asks the second kind.
pub struct AstGrepPlugin;

#[async_trait]
impl Plugin for AstGrepPlugin {
    fn name(&self) -> &'static str {
        "tool-ast-grep"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "structural (AST) search"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        mount(ctx, vec![Arc::new(AstGrepTool) as Arc<dyn Tool>])?;
        contribute_prompt(
            ctx,
            "tool-ast-grep",
            54,
            "Use `ast_grep` when you are looking for a code *shape* rather than a string — \
             a call pattern, a construct, an idiom.",
        );
        Ok(())
    }
}

pub struct SearchToolsPlugin;

#[async_trait]
impl Plugin for SearchToolsPlugin {
    fn name(&self) -> &'static str {
        "tool-search"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["product-dirs", "tools"]
    }
    fn description(&self) -> &'static str {
        "grep and glob over the working directory"
    }
    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let options: atomcode_capabilities::world::WalkOptions = if config.is_null() {
            Default::default()
        } else {
            serde_json::from_value(config.clone())
                .map_err(|e| format!("bad tool-search config: {e}"))?
        };
        mount(ctx, {
            let dirs = (*crate::product_dirs(ctx)?).clone();
            vec![
                Arc::new(GrepTool::new(dirs.clone()).with_options(options.clone())),
                Arc::new(GlobTool::new(dirs).with_options(options)),
            ]
        })?;
        contribute_prompt(
            ctx,
            "tool-search",
            52,
            "For a known file path, call `read_file` directly: ignored files can still be read. \
             Use `grep`/`glob` to discover unknown paths. An empty filtered search does not prove absence. \
             If needed, explicitly scope the provided directory and choose respect_gitignore=false, \
             include_hidden=true, or follow_symlinks=true independently; do not expand to the whole disk.",
        );
        Ok(())
    }
}
