//! Editable VL fallback selection shared by terminal and web settings.
use crate::Config;

#[derive(Debug, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VisionPreprocessorStatus {
    Unconfigured,
    Resolved,
    Unresolvable,
    Unsupported,
}

pub fn vision_preprocessor_status(config: &Config) -> VisionPreprocessorStatus {
    let Some(id) = config
        .vision_preprocessor_provider
        .as_deref()
        .filter(|id| !id.is_empty())
    else {
        return VisionPreprocessorStatus::Unconfigured;
    };
    match config.provider_config_for_selection(id) {
        Some(model) if model.accepts_images() => VisionPreprocessorStatus::Resolved,
        Some(_) => VisionPreprocessorStatus::Unsupported,
        None => VisionPreprocessorStatus::Unresolvable,
    }
}

pub fn select_vision_preprocessor(config: &mut Config, selection: &str) -> anyhow::Result<()> {
    let selection = selection.trim();
    if !selection.is_empty() {
        let model = config
            .provider_config_for_selection(selection)
            .ok_or_else(|| anyhow::anyhow!("Vision preprocessing model does not resolve"))?;
        anyhow::ensure!(
            model.accepts_images(),
            "Vision preprocessing model must support image input"
        );
    }
    config.vision_preprocessor_provider = (!selection.is_empty()).then(|| selection.to_owned());
    Ok(())
}

pub const SETTING_ID: &str = "vision_preprocessor_provider";

pub fn vision_preprocessor_choices(config: &Config) -> Vec<String> {
    let mut ids: Vec<_> = config
        .logical_models()
        .into_keys()
        .filter(|id| {
            config
                .provider_config_for_selection(id)
                .is_some_and(|model| model.accepts_images())
        })
        .collect();
    ids.sort();
    ids.insert(0, String::new());
    ids
}

/// Validate against the document under ConfigStore's write lock, preserving
/// unrelated keys and comments rather than rewriting the whole configuration.
pub fn patch_setting(document: &mut toml_edit::DocumentMut, selection: &str) -> anyhow::Result<()> {
    let mut config: Config = toml::from_str(&document.to_string())?;
    select_vision_preprocessor(&mut config, selection)?;
    match config.vision_preprocessor_provider {
        Some(id) => {
            document[SETTING_ID] = toml_edit::value(id);
        }
        None => {
            document.remove(SETTING_ID);
        }
    }
    Ok(())
}

pub fn setting_label(config: &Config) -> String {
    let zh = crate::i18n::current_locale() == crate::locale::Locale::ZhCn;
    let label = if zh {
        "视觉预处理模型（VL，空值关闭）"
    } else {
        "Vision preprocessing model (VL; unset: off)"
    };
    let warning = match vision_preprocessor_status(config) {
        VisionPreprocessorStatus::Unresolvable => {
            if zh {
                " — 无法解析，请更换或清空"
            } else {
                " — unresolved; replace or clear"
            }
        }
        VisionPreprocessorStatus::Unsupported => {
            if zh {
                " — 不支持图片，请更换或清空"
            } else {
                " — no image input; replace or clear"
            }
        }
        _ => "",
    };
    format!("{label}{warning}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
# unrelated preferences survive editing
language = "en"
[providers.legacy]
type = "openai"
model = "custom-image-model"
supports_vision = true
[provider_accounts.test]
provider = "openai"
base_url = "https://example.invalid/v1"
[models."test/vision"]
account = "test"
model = "custom-vl"
supports_vision = true
[models."test/text"]
account = "test"
model = "gpt-4o"
supports_vision = false
"#,
        )
        .unwrap()
    }

    #[test]
    fn only_resolved_image_capable_models_can_be_selected() {
        let mut config = config();
        assert_eq!(
            vision_preprocessor_status(&config),
            VisionPreprocessorStatus::Unconfigured
        );
        assert_eq!(
            vision_preprocessor_choices(&config),
            vec!["", "legacy", "test/vision"]
        );
        for valid in ["legacy", "test/vision"] {
            select_vision_preprocessor(&mut config, valid).unwrap();
            assert_eq!(
                vision_preprocessor_status(&config),
                VisionPreprocessorStatus::Resolved
            );
            for invalid in ["missing", "test/text"] {
                assert!(select_vision_preprocessor(&mut config, invalid).is_err());
                assert_eq!(config.vision_preprocessor_provider.as_deref(), Some(valid));
            }
        }
        config.vision_preprocessor_provider = Some("missing".into());
        assert_eq!(
            vision_preprocessor_status(&config),
            VisionPreprocessorStatus::Unresolvable
        );
        config.vision_preprocessor_provider = Some("test/text".into());
        assert_eq!(
            vision_preprocessor_status(&config),
            VisionPreprocessorStatus::Unsupported
        );
        select_vision_preprocessor(&mut config, " ").unwrap();
        assert!(config.vision_preprocessor_provider.is_none());
    }

    #[test]
    fn document_patch_preserves_other_settings_and_rejects_without_mutation() {
        let mut document: toml_edit::DocumentMut =
            toml::to_string(&config()).unwrap().parse().unwrap();
        document.decor_mut().set_prefix("# keep comment\n");
        patch_setting(&mut document, "test/vision").unwrap();
        assert!(document.to_string().contains("# keep comment"));
        let before = document.to_string();
        assert!(patch_setting(&mut document, "test/text").is_err());
        assert_eq!(document.to_string(), before);
        patch_setting(&mut document, "").unwrap();
        assert!(document.get(SETTING_ID).is_none());
        let saved: Config = toml::from_str(&document.to_string()).unwrap();
        assert_eq!(
            saved.logical_models().len(),
            config().logical_models().len()
        );
        assert_eq!(saved.language, config().language);
    }
}
