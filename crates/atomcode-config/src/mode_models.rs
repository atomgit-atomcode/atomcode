//! Optional execution-mode model preferences. Selection IDs, never credentials.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use toml_edit::{value, DocumentMut};

use crate::Config;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeModelBinding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Absent inherits the model profile; `default` requests the API default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}
impl ModeModelBinding {
    pub fn is_empty(&self) -> bool {
        self.model.is_none() && self.reasoning_effort.is_none()
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeModels {
    #[serde(default, skip_serializing_if = "ModeModelBinding::is_empty")]
    pub plan: ModeModelBinding,
    #[serde(default, skip_serializing_if = "ModeModelBinding::is_empty")]
    pub build: ModeModelBinding,
}
impl ModeModels {
    pub fn is_empty(&self) -> bool {
        self.plan.is_empty() && self.build.is_empty()
    }
    pub fn get(&self, mode: &str) -> Option<&ModeModelBinding> {
        match mode {
            "plan" => Some(&self.plan),
            "build" => Some(&self.build),
            _ => None,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct ModeModelChoice {
    pub id: String,
    pub label: String,
    pub effort_levels: Vec<String>,
}
pub fn model_choices(config: &Config) -> Vec<ModeModelChoice> {
    let mut choices: Vec<_> = config
        .logical_models()
        .into_keys()
        .filter_map(|id| {
            let resolved = config.resolve_model(Some(&id)).ok()?;
            Some(ModeModelChoice {
                label: format!("{} · {}", resolved.model, id),
                id,
                effort_levels: supported_efforts(&resolved),
            })
        })
        .collect();
    choices.sort_by(|a, b| a.id.cmp(&b.id));
    choices
}
pub fn supported_efforts(resolved: &crate::config::provider::ResolvedModelConfig) -> Vec<String> {
    crate::config::declared_effort_levels(resolved.reasoning_effort_levels.as_deref())
        .into_iter()
        .map(str::to_string)
        .collect()
}
pub fn validate_binding(config: &Config, binding: &ModeModelBinding) -> Result<()> {
    let resolved = match binding.model.as_deref() {
        Some(id) if id.trim().is_empty() => bail!("Mode model selection cannot be empty"),
        Some(id) => Some(config.resolve_model(Some(id))?),
        None => None,
    };
    if let Some(effort) = binding.reasoning_effort.as_deref() {
        if effort != "default" && !crate::config::REASONING_EFFORT_LEVELS.contains(&effort) {
            bail!("Unknown reasoning effort: {effort}");
        }
        if let Some(resolved) = resolved {
            let levels = supported_efforts(&resolved);
            if !crate::config::endpoint_supports_reasoning_effort(
                resolved.reasoning_effort.as_deref(),
                resolved.reasoning_effort_levels.as_deref(),
            ) || (effort != "default" && !levels.iter().any(|level| level == effort))
            {
                bail!(
                    "Model {} does not support reasoning effort {effort}",
                    resolved.selection_id
                );
            }
        }
    }
    Ok(())
}
/// Patch one binding field; changing a model resets an incompatible old effort.
/// Explicit effort edits are rejected instead of silently clamped.
pub fn patch_setting(document: &mut DocumentMut, id: &str, input: Option<&str>) -> Result<()> {
    let bits: Vec<_> = id.split('.').collect();
    if bits.len() != 3
        || bits[0] != "modes"
        || !["plan", "build"].contains(&bits[1])
        || !["model", "reasoning_effort"].contains(&bits[2])
    {
        bail!("Unknown mode setting: {id}");
    }
    let mut candidate = document.clone();
    if let Some(input) = input.filter(|s| !s.trim().is_empty()) {
        candidate["modes"][bits[1]][bits[2]] = value(input);
    } else if let Some(table) = candidate
        .get_mut("modes")
        .and_then(|v| v.get_mut(bits[1]))
        .and_then(|v| v.as_table_like_mut())
    {
        table.remove(bits[2]);
    }
    let mut parsed: Config = toml::from_str(&candidate.to_string())?;
    if bits[2] == "model" && validate_binding(&parsed, parsed.modes.get(bits[1]).unwrap()).is_err()
    {
        // Ensure the selection itself is valid before resetting an old effort.
        if let Some(model) = parsed.modes.get(bits[1]).unwrap().model.as_deref() {
            parsed.resolve_model(Some(model))?;
        }
        if let Some(table) = candidate
            .get_mut("modes")
            .and_then(|v| v.get_mut(bits[1]))
            .and_then(|v| v.as_table_like_mut())
        {
            table.remove("reasoning_effort");
        }
        parsed = toml::from_str(&candidate.to_string())?;
    }
    validate_binding(&parsed, parsed.modes.get(bits[1]).unwrap())?;
    *document = candidate;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct ModeSettingRow {
    pub id: String,
    pub label: String,
    pub value: String,
    pub options: Vec<String>,
}
pub fn setting_rows(config: &Config) -> Vec<ModeSettingRow> {
    let zh = crate::i18n::current_locale() == crate::locale::Locale::ZhCn;
    let choices = model_choices(config);
    let mut rows = Vec::new();
    for (mode, binding) in [("plan", &config.modes.plan), ("build", &config.modes.build)] {
        let model_label = match (mode, zh) {
            ("plan", true) => "Plan 模型（留空跟随当前）",
            (_, true) => "Build 模型（留空恢复进入 Plan 前的选择）",
            ("plan", false) => "Plan model (unset: follow current)",
            _ => "Build model (unset: restore pre-Plan selection)",
        };
        rows.push(ModeSettingRow {
            id: format!("modes.{mode}.model"),
            label: format!(
                "{} · {model_label}",
                if zh { "模式与模型" } else { "Mode models" }
            ),
            value: binding.model.clone().unwrap_or_default(),
            options: std::iter::once(String::new())
                .chain(choices.iter().map(|c| c.id.clone()))
                .collect(),
        });
        let levels = binding
            .model
            .as_ref()
            .and_then(|id| choices.iter().find(|c| &c.id == id))
            .map(|c| c.effort_levels.clone())
            .unwrap_or_else(|| {
                crate::config::REASONING_EFFORT_LEVELS
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
            });
        let mut options = vec![String::new()];
        if !levels.is_empty() {
            options.push("default".into());
            options.extend(levels);
        }
        rows.push(ModeSettingRow {
            id: format!("modes.{mode}.reasoning_effort"),
            label: if zh {
                format!("模式与模型 · {mode} 思考强度（留空使用模型配置）")
            } else {
                format!("{mode} effort (unset: model profile)")
            },
            value: binding.reasoning_effort.clone().unwrap_or_default(),
            options,
        });
    }
    rows
}
pub fn patch_bindings(document: &mut DocumentMut, bindings: &ModeModels) -> Result<()> {
    let mut next = document.clone();
    for (mode, binding) in [("plan", &bindings.plan), ("build", &bindings.build)] {
        patch_setting(
            &mut next,
            &format!("modes.{mode}.model"),
            binding.model.as_deref(),
        )?;
        patch_setting(
            &mut next,
            &format!("modes.{mode}.reasoning_effort"),
            binding.reasoning_effort.as_deref(),
        )?;
    }
    *document = next;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeSelection {
    pub model: String,
    pub reasoning_effort: Option<String>,
}
/// Resolve a stateless driver's next selection. Live agents perform this inside
/// CodingRuntime instead; no driver may patch a running agent independently.
pub fn resolve_driver_selection(
    config: &Config,
    mode: &str,
    leaving_plan: bool,
    current: &ModeSelection,
    saved: Option<&ModeSelection>,
) -> Result<Option<ModeSelection>> {
    let Some(binding) = config.modes.get(mode) else {
        return Ok(None);
    };
    let restore = if mode == "build" && leaving_plan && binding.model.is_none() {
        saved
    } else {
        None
    };
    if binding.is_empty() && restore.is_none() {
        return Ok(None);
    }
    let id = binding
        .model
        .as_deref()
        .or_else(|| restore.map(|saved| saved.model.as_str()))
        .unwrap_or(&current.model);
    let resolved = config.resolve_model(Some(id))?;
    let effort = if let Some(effort) = &binding.reasoning_effort {
        Some(effort.clone())
    } else if let Some(saved) = restore {
        saved.reasoning_effort.clone()
    } else if binding.model.is_some() {
        resolved.reasoning_effort.clone()
    } else {
        current.reasoning_effort.clone()
    };
    let explicit = ModeModelBinding {
        model: Some(id.into()),
        reasoning_effort: effort.clone(),
    };
    // A fixed profile default remains valid even when the endpoint does not
    // declare switchable levels. Explicit overrides still require declaration.
    if effort.is_some()
        && (binding.reasoning_effort.is_some() || effort != resolved.reasoning_effort)
    {
        validate_binding(config, &explicit)?;
    }
    Ok(Some(ModeSelection {
        model: id.into(),
        reasoning_effort: effort.filter(|value| value != "default" && value != "auto"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn document() -> DocumentMut {
        r#"
# keep this comment
default_provider = "fast"
[providers.fast]
type = "openai"
api_key = "private-fast"
base_url = "https://fast.example/v1"
model = "fast-wire"
reasoning_effort_levels = ["low"]
[providers.deep]
type = "openai"
api_key = "private-deep"
base_url = "https://deep.example/v1"
model = "deep-wire"
reasoning_effort_levels = ["low", "high"]
"#
        .parse()
        .unwrap()
    }
    #[test]
    fn modes_default_to_empty_and_roundtrip_without_rewriting_global_selection() {
        let mut doc = document();
        patch_setting(&mut doc, "modes.plan.model", Some("deep")).unwrap();
        patch_setting(&mut doc, "modes.plan.reasoning_effort", Some("high")).unwrap();
        let config: Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(config.modes.plan.model.as_deref(), Some("deep"));
        assert_eq!(config.default_provider, "fast");
        assert!(doc.to_string().contains("# keep this comment"));
        let encoded = toml::to_string(&config).unwrap();
        assert_eq!(
            toml::from_str::<Config>(&encoded).unwrap().modes,
            config.modes
        );
        assert!(toml::from_str::<Config>("").unwrap().modes.is_empty());
    }
    #[test]
    fn invalid_edits_leave_the_document_unchanged_and_model_changes_reset_old_effort() {
        let mut doc = document();
        let before = doc.to_string();
        assert!(patch_setting(&mut doc, "modes.plan.model", Some("missing")).is_err());
        assert_eq!(doc.to_string(), before);
        patch_setting(&mut doc, "modes.plan.model", Some("deep")).unwrap();
        patch_setting(&mut doc, "modes.plan.reasoning_effort", Some("high")).unwrap();
        patch_setting(&mut doc, "modes.plan.model", Some("fast")).unwrap();
        let before = doc.to_string();
        assert!(patch_setting(&mut doc, "modes.plan.reasoning_effort", Some("high")).is_err());
        assert_eq!(doc.to_string(), before);
        let cfg: Config = toml::from_str(&before).unwrap();
        assert_eq!(cfg.modes.plan.reasoning_effort, None);
        assert_eq!(setting_rows(&cfg)[1].options, vec!["", "default", "low"]);
    }
    #[test]
    fn fixed_profile_effort_is_preserved_without_inventing_switchable_levels() {
        let mut doc = document();
        doc["providers"]["deep"]["reasoning_effort_levels"] = toml_edit::Item::None;
        doc["providers"]["deep"]["reasoning_effort"] = value("high");
        patch_setting(&mut doc, "modes.plan.model", Some("deep")).unwrap();
        let config: Config = toml::from_str(&doc.to_string()).unwrap();
        assert!(model_choices(&config)
            .iter()
            .find(|m| m.id == "deep")
            .unwrap()
            .effort_levels
            .is_empty());
        assert!(patch_setting(&mut doc, "modes.plan.reasoning_effort", Some("low")).is_err());
        let prior = ModeSelection {
            model: "fast".into(),
            reasoning_effort: Some("low".into()),
        };
        let plan = resolve_driver_selection(&config, "plan", false, &prior, None)
            .unwrap()
            .unwrap();
        assert_eq!(plan.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            resolve_driver_selection(&config, "build", true, &prior, Some(&plan)).unwrap(),
            Some(plan)
        );
    }

    #[test]
    fn whole_binding_edits_do_not_persist_a_partial_plan_when_build_is_invalid() {
        let mut doc = document();
        let original = doc.to_string();
        let bindings = ModeModels {
            plan: ModeModelBinding {
                model: Some("deep".into()),
                reasoning_effort: Some("high".into()),
            },
            build: ModeModelBinding {
                model: Some("missing".into()),
                reasoning_effort: None,
            },
        };
        assert!(patch_bindings(&mut doc, &bindings).is_err());
        assert_eq!(doc.to_string(), original);
    }
    #[test]
    fn driver_resolution_restores_prior_selection_and_choices_are_sanitized() {
        let mut doc = document();
        patch_setting(&mut doc, "modes.plan.model", Some("deep")).unwrap();
        patch_setting(&mut doc, "modes.plan.reasoning_effort", Some("high")).unwrap();
        let cfg: Config = toml::from_str(&doc.to_string()).unwrap();
        let prior = ModeSelection {
            model: "fast".into(),
            reasoning_effort: Some("low".into()),
        };
        let plan = resolve_driver_selection(&cfg, "plan", false, &prior, None)
            .unwrap()
            .unwrap();
        assert_eq!(plan.model, "deep");
        assert_eq!(plan.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            resolve_driver_selection(&cfg, "build", true, &plan, Some(&prior)).unwrap(),
            Some(prior)
        );
        let json = serde_json::to_string(&model_choices(&cfg)).unwrap();
        assert!(!json.contains("private-"));
        assert!(!json.contains("api_key"));
    }
}
