//! The three registry plugins. Each one does nothing but own a slot — which is
//! exactly why they are separable: a deployment can swap the session store for a
//! persistent one without the tool catalog knowing.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

use crate::seams::{OperationsSvc, PromptRegistry, SystemPromptSvc, ToolBox, ToolsSvc};

pub struct ToolsPlugin;

#[async_trait]
impl Plugin for ToolsPlugin {
    fn name(&self) -> &'static str {
        "tools"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["tools"]
    }
    fn description(&self) -> &'static str {
        "the live tool catalog every tool plugin registers into"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<ToolsSvc>(Arc::new(ToolBox::new()))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

pub struct SystemPromptPlugin;

#[async_trait]
impl Plugin for SystemPromptPlugin {
    fn name(&self) -> &'static str {
        "system-prompt"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["system-prompt"]
    }
    fn description(&self) -> &'static str {
        "ordered prompt fragments, contributed by whoever owns the behaviour"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<SystemPromptSvc>(Arc::new(PromptRegistry::new()))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}

pub struct OperationsPlugin;

#[async_trait]
impl Plugin for OperationsPlugin {
    fn name(&self) -> &'static str {
        "operations"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["operations"]
    }
    fn description(&self) -> &'static str {
        "where each row describes its own knobs, for `describe_self` to answer with"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<OperationsSvc>(Arc::new(PromptRegistry::new()))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
