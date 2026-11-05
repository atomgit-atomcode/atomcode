//! Resolve optional mode preferences without changing the global model default.
use crate::{
    runtime::{RuntimeError, RuntimeMode},
    CodingAgentConfig,
};
use atomcode_kernel::provider::ReasoningEffort;

#[derive(Clone)]
pub(crate) struct SavedModeModel {
    pub owner: (Option<String>, std::path::PathBuf),
    pub selection: String,
    pub effort: Option<ReasoningEffort>,
}

pub(crate) fn next_config(
    base: &CodingAgentConfig,
    before: RuntimeMode,
    mode: RuntimeMode,
    saved: Option<&SavedModeModel>,
) -> Result<Option<CodingAgentConfig>, RuntimeError> {
    if before == mode {
        return Ok(None);
    }
    let Some(registry) = base.subagent_config.as_ref() else {
        return Ok(None);
    };
    let binding = match mode {
        RuntimeMode::Plan => Some(&registry.modes.plan),
        RuntimeMode::Build => Some(&registry.modes.build),
        _ => None,
    };
    let binding = binding.filter(|b| !b.is_empty());
    let restore = if mode == RuntimeMode::Build
        && before == RuntimeMode::Plan
        && binding
            .and_then(|binding| binding.model.as_deref())
            .is_none()
    {
        saved
    } else {
        None
    };
    if binding.is_none() && restore.is_none() {
        return Ok(None);
    }
    let selection = binding
        .and_then(|b| b.model.as_deref())
        .or_else(|| restore.map(|r| r.selection.as_str()));
    let mut next = if let Some(selection) = selection {
        let resolved = registry.resolve_model(Some(selection)).map_err(|error| {
            RuntimeError::ReconfigureFailed(format!("Mode model {selection}: {error}"))
        })?;
        let mut next = crate::provider_factory::derive_tier_config_from_resolved(base, &resolved);
        // Never inherit a credential or URL across provider boundaries.
        next.base_url = resolved.base_url.clone().unwrap_or_default();
        next.subagent_config = base.subagent_config.clone();
        next.subagent_fast_provider = base.subagent_fast_provider.clone();
        next.subagent_capable_provider = base.subagent_capable_provider.clone();
        next.subagent_model_providers = base.subagent_model_providers.clone();
        next
    } else {
        base.clone()
    };
    if let Some(saved) = restore {
        next.chat_options.reasoning_effort = saved.effort;
    }
    if let Some(effort) = binding.and_then(|b| b.reasoning_effort.as_deref()) {
        if !next.supports_reasoning_effort
            || (effort != "default" && !next.effort_levels.iter().any(|v| v == effort))
        {
            return Err(RuntimeError::ReconfigureFailed(format!(
                "Model {} does not support reasoning effort {effort}",
                next.provider_name
            )));
        }
        next.chat_options.reasoning_effort = ReasoningEffort::from_config(Some(effort));
    }
    if let Some(effort) = next.chat_options.reasoning_effort {
        if !next.supports_reasoning_effort
            || (!next.effort_levels.is_empty()
                && !next.effort_levels.iter().any(|v| v == effort.as_str()))
        {
            return Err(RuntimeError::ReconfigureFailed(format!(
                "Model {} does not support restored reasoning effort {}",
                next.provider_name,
                effort.as_str()
            )));
        }
    }
    Ok(Some(next))
}

/// A preferences-only reload must not reset a temporary model or interrupt a turn.
pub(crate) fn only_preferences_changed(old: &CodingAgentConfig, new: &CodingAgentConfig) -> bool {
    let (Some(old), Some(new)) = (old.subagent_config.as_ref(), new.subagent_config.as_ref())
    else {
        return false;
    };
    if old.modes == new.modes {
        return false;
    }
    let mut old = (**old).clone();
    let mut new = (**new).clone();
    old.modes = Default::default();
    new.modes = Default::default();
    matches!((serde_json::to_value(old),serde_json::to_value(new)),(Ok(a),Ok(b)) if a==b)
}
