use std::collections::HashMap;
use std::sync::RwLock;

use super::types::SubAgentDefinition;

/// Subagent registry. Uses std::sync::RwLock consistent with SkillRegistry.
pub struct SubAgentRegistry {
    agents: RwLock<HashMap<String, SubAgentDefinition>>,
}

impl SubAgentRegistry {
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
        }
    }

    /// Register a subagent definition. Returns Err on duplicate name.
    pub fn register(&self, def: SubAgentDefinition) -> Result<(), String> {
        let mut agents = self.agents.write().map_err(|e| format!("lock: {}", e))?;
        if agents.contains_key(&def.name) {
            return Err(format!("subagent '{}' already registered", def.name));
        }
        agents.insert(def.name.clone(), def);
        Ok(())
    }

    /// Find by name
    pub fn find(&self, name: &str) -> Option<SubAgentDefinition> {
        let agents = self.agents.read().ok()?;
        agents.get(name).cloned()
    }

    /// List all registered subagents
    pub fn list(&self) -> Vec<SubAgentDefinition> {
        let agents = match self.agents.read() {
            Ok(g) => g,
            Err(_) => return vec![],
        };
        agents.values().cloned().collect()
    }
}

impl Default for SubAgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}
