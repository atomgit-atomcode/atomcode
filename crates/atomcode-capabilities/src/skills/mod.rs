//! Skills capability (L1): a markdown/frontmatter skill loader + the `use_skill` /
//! `list_skills` tools. A *skill* is a reusable prompt/workflow template (Claude-Code
//! compatible: flat `*.md` commands or `*/SKILL.md` directories with frontmatter).
//!
//! NEUTRAL boundary: this crate loads + expands skills from directories the CALLER
//! supplies; the standard `~/.claude/skills` etc. precedence is a driver concern (use
//! [`standard_skill_dirs`] or pass your own). Skill CONTENT is user-authored and trusted
//! — `expand` runs a skill's `` !`cmd` `` blocks through a shell by design.
//!
//! Behind the opt-in `skills` cargo feature (no extra dependencies).

use atomcode_kernel::tool::{ToolRegistry, ToolResult};
use std::sync::Arc;

pub mod catalog_hook;
pub mod registry;
pub mod render;
pub mod skill;
pub mod use_skill;

pub use catalog_hook::SkillCatalogHook;
pub use registry::{standard_skill_dirs, SkillRegistry};
pub use render::{render_skill_catalog, CatalogEntry};
pub use skill::Skill;
pub use use_skill::{ListSkillsTool, UseSkillTool};

/// Names of the skill tools — pass to [`ToolRegistry::mount`](atomcode_kernel::tool::ToolRegistry::mount).
pub fn skill_tool_names() -> &'static [&'static str] {
    &["use_skill", "list_skills"]
}

/// Register `use_skill` + `list_skills` over a (caller-built) [`SkillRegistry`].
pub fn register_skill_tools(reg: &mut ToolRegistry, registry: Arc<SkillRegistry>) {
    reg.register(Arc::new(UseSkillTool::new(registry.clone())));
    reg.register(Arc::new(ListSkillsTool::new(registry)));
}

pub(crate) fn ok(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: false,
        images: vec![],
    }
}
pub(crate) fn err(content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        content: content.into(),
        is_error: true,
        images: vec![],
    }
}
