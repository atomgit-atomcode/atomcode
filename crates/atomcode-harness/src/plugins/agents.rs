//! The agent registry row.
//!
//! One line of behaviour and a service, which is the point: a UI, a scheduler,
//! a supervisor or another agent finds the live agents by asking the registry
//! rather than by being handed a reference at construction.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

use crate::agent::Agents;
use crate::seams::AgentsSvc;

pub struct AgentsPlugin;

#[async_trait]
impl Plugin for AgentsPlugin {
    fn name(&self) -> &'static str {
        "agents"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["agents"]
    }
    fn description(&self) -> &'static str {
        "the live agent registry"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let _ = ctx
            .provide::<AgentsSvc>(Arc::new(Agents::new()))
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
