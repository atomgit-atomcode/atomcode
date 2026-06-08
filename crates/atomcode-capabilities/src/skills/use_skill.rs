//! `use_skill` (invoke a named skill, returns its expanded content) + `list_skills`.
//! Both `Safe` — skills are trusted, user-authored content.

use super::registry::SkillRegistry;
use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub struct UseSkillTool {
    registry: Arc<SkillRegistry>,
}

impl UseSkillTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[derive(Deserialize)]
struct Args {
    name: String,
    #[serde(default)]
    arguments: Option<String>,
}

#[async_trait]
impl Tool for UseSkillTool {
    fn name(&self) -> &str {
        "use_skill"
    }
    fn description(&self) -> &str {
        "Invoke a named skill (a reusable prompt/workflow template) and return its content \
         with your arguments substituted. Run list_skills first to see what's available."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Skill name (see list_skills)" },
                "arguments": { "type": "string", "description": "Arguments passed to the skill (optional)" }
            },
            "required": ["name"]
        })
    }
    // skills are trusted user content → risk() defaults to Safe.
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("use_skill: invalid arguments: {e}. Expected {{\"name\":\"<skill>\"}}.")),
        };
        let skill = match self.registry.get(&a.name) {
            Some(s) => s,
            None => {
                let names: Vec<String> = self.registry.list().into_iter().map(|(n, _)| n).collect();
                return err(format!(
                    "use_skill: skill '{}' not found. Available: {}",
                    a.name,
                    if names.is_empty() { "(none)".to_string() } else { names.join(", ") }
                ));
            }
        };
        let arguments = a.arguments.unwrap_or_default();
        // expand may run `!`cmd`` shell blocks → keep off the async runtime.
        match tokio::task::spawn_blocking(move || skill.expand(&arguments, "")).await {
            Ok(content) => ok(content),
            Err(_) => err("use_skill: expansion task failed"),
        }
    }
}

pub struct ListSkillsTool {
    registry: Arc<SkillRegistry>,
}

impl ListSkillsTool {
    pub fn new(registry: Arc<SkillRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for ListSkillsTool {
    fn name(&self) -> &str {
        "list_skills"
    }
    fn description(&self) -> &str {
        "List the available skills (name + description). Invoke one with use_skill."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        let skills = self.registry.list();
        if skills.is_empty() {
            return ok("No skills are loaded.".to_string());
        }
        let mut out = format!("Available skills ({}):\n", skills.len());
        for (name, desc) in &skills {
            if desc.is_empty() {
                out.push_str(&format!("- {name}\n"));
            } else {
                out.push_str(&format!("- {name}: {desc}\n"));
            }
        }
        ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext { working_dir: std::path::PathBuf::from("."), cancel: CancellationToken::new(), progress: atomcode_kernel::tool::ProgressSink::noop() }
    }
    fn registry_with(skills: &[(&str, &str)]) -> Arc<SkillRegistry> {
        let d = Box::leak(Box::new(tempfile::tempdir().unwrap())); // keep alive for the test
        for (name, body) in skills {
            std::fs::write(d.path().join(format!("{name}.md")), body).unwrap();
        }
        Arc::new(SkillRegistry::load(&[d.path().to_path_buf()]))
    }

    #[tokio::test]
    async fn use_skill_expands() {
        let tool = UseSkillTool::new(registry_with(&[("greet", "Hello $ARGUMENTS!")]));
        let r = tool.execute(r#"{"name":"greet","arguments":"world"}"#, &ctx()).await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(r.content, "Hello world!");
    }

    #[tokio::test]
    async fn use_skill_not_found_lists_available() {
        let tool = UseSkillTool::new(registry_with(&[("a", "x"), ("b", "y")]));
        let r = tool.execute(r#"{"name":"nope"}"#, &ctx()).await;
        assert!(r.is_error);
        assert!(r.content.contains("not found"), "{}", r.content);
        assert!(r.content.contains("a") && r.content.contains("b"), "{}", r.content);
    }

    #[tokio::test]
    async fn list_skills_formats() {
        let tool = ListSkillsTool::new(registry_with(&[("greet", "---\ndescription: say hi\n---\nHello")]));
        let r = tool.execute("{}", &ctx()).await;
        assert!(r.content.contains("Available skills (1)"), "{}", r.content);
        assert!(r.content.contains("- greet: say hi"), "{}", r.content);
    }

    #[tokio::test]
    async fn list_skills_empty() {
        let tool = ListSkillsTool::new(Arc::new(SkillRegistry::new()));
        let r = tool.execute("{}", &ctx()).await;
        assert!(r.content.contains("No skills"), "{}", r.content);
    }
}
