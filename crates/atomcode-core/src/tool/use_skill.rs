use std::sync::{Arc, RwLock};

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};
use crate::skill::SkillRegistry;

/// Tool that loads a skill's instruction template into the conversation context.
///
/// This is the LLM-facing equivalent of the user typing `/skill-name args`.
/// The skill content is returned as the tool result, which the LLM reads and
/// then follows using its regular tools — no sub-agent needed.
pub struct UseSkillTool {
    pub registry: Arc<RwLock<SkillRegistry>>,
}

#[derive(Deserialize)]
struct UseSkillArgs {
    name: String,
    #[serde(default)]
    arguments: String,
}

#[async_trait]
impl Tool for UseSkillTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "use_skill",
            description: "Load a skill's instruction template into context. \
                Use this when a task matches a skill's purpose — the skill provides \
                detailed, reusable instructions that guide how to complete the task. \
                Available skills are listed in the system prompt under 'Available Skills'. \
                Returns the expanded skill content for you to follow."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill name (without leading slash)"
                    },
                    "arguments": {
                        "type": "string",
                        "description": "Arguments passed to the skill. Replaces $ARGUMENTS in the template."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: UseSkillArgs = serde_json::from_str(args)?;

        let expanded = {
            let registry = self
                .registry
                .read()
                .map_err(|e| anyhow::anyhow!("registry lock: {}", e))?;
            let skill = registry.get(&parsed.name).or_else(|| {
                if parsed.name.contains(':') {
                    None
                } else {
                    registry.get(&format!("skills:{}", parsed.name))
                }
            });

            match skill {
                Some(skill) => {
                    if skill.disable_model_invocation {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: format!(
                                "Skill '{}' cannot be invoked automatically. Ask the user to run `/{}`.",
                                parsed.name, parsed.name
                            ),
                            success: false,
                        });
                    }
                    skill.expand(&parsed.arguments, "")
                }
                None => {
                    let available: Vec<String> = registry
                        .invocable_by_llm()
                        .map(|s| s.name.clone())
                        .collect();
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!(
                            "Skill '{}' not found. Available skills: {}",
                            parsed.name,
                            if available.is_empty() {
                                "(none)".to_string()
                            } else {
                                available.join(", ")
                            }
                        ),
                        success: false,
                    });
                }
            }
        };

        if expanded.trim().is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Skill '{}' has an empty template.", parsed.name),
                success: false,
            });
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: expanded,
            success: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::Skill;
    use std::path::PathBuf;

    fn test_skill(name: &str, template: &str) -> Skill {
        Skill {
            name: name.into(),
            description: "test skill".into(),
            template: template.into(),
            disable_model_invocation: false,
            user_invocable: true,
            argument_hint: None,
            allowed_tools: vec![],
            skill_dir: PathBuf::new(),
            source_path: PathBuf::new(),
        }
    }

    fn tool_with_skills(skills: Vec<Skill>) -> UseSkillTool {
        let mut registry = SkillRegistry::new();
        for skill in skills {
            registry.register(skill);
        }
        UseSkillTool {
            registry: Arc::new(RwLock::new(registry)),
        }
    }

    #[tokio::test]
    async fn resolves_bare_name_to_loose_skills_namespace() {
        let tool = tool_with_skills(vec![test_skill("skills:brainstorming", "Do $ARGUMENTS")]);
        let ctx = ToolContext::new(PathBuf::from("/tmp"));

        let result = tool
            .execute(r#"{"name":"brainstorming","arguments":"ideas"}"#, &ctx)
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output, "Do ideas");
    }

    #[tokio::test]
    async fn keeps_explicit_namespace_lookup_working() {
        let tool = tool_with_skills(vec![test_skill("skills:brainstorming", "Namespaced")]);
        let ctx = ToolContext::new(PathBuf::from("/tmp"));

        let result = tool
            .execute(r#"{"name":"skills:brainstorming"}"#, &ctx)
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output, "Namespaced");
    }

    #[tokio::test]
    async fn does_not_fallback_for_other_namespaces() {
        let tool = tool_with_skills(vec![test_skill("skills:brainstorming", "Namespaced")]);
        let ctx = ToolContext::new(PathBuf::from("/tmp"));

        let result = tool
            .execute(r#"{"name":"plugin:brainstorming"}"#, &ctx)
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result
            .output
            .contains("Skill 'plugin:brainstorming' not found"));
    }
}
