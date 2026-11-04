//! Offline model evaluation catalogue shared by read-only product viewers.
//! No provider configuration, model routing or runtime state is owned here.
use serde_json::Value;
use std::sync::OnceLock;

pub const SNAPSHOT: &str = include_str!("../../../data/model-cards.json");

pub fn catalog() -> &'static Value {
    static DATA: OnceLock<Value> = OnceLock::new();
    DATA.get_or_init(|| serde_json::from_str(SNAPSHOT).expect("validated bundled model cards"))
}

/// Only reviewed version identities; never infer from profile or gateway names.
/// In particular, bare DeepSeek V4/Flash aliases can float to another checkpoint.
pub fn resolve_id(wire_model: &str) -> Option<&'static str> {
    catalog()["wire_model_bindings"][wire_model.trim().to_ascii_lowercase()].as_str()
}

/// Missing public evidence and unverified aliases are distinct read-only states.
/// Neither state disables model use or claims that the model does not exist.
pub fn status(wire_model: &str) -> &'static str {
    if resolve_id(wire_model).is_some() {
        "available"
    } else if catalog()["no_public_evaluation_models"]
        .get(wire_model.trim().to_ascii_lowercase())
        .is_some()
    {
        "no_public_data"
    } else {
        "unverified"
    }
}

pub fn score_label(score: f64, benchmark: &Value) -> String {
    match benchmark["kind"].as_str() {
        Some("elo") => format!("{score:.1}"),
        Some("linear") => format!("{score:.4} {}", benchmark["unit"].as_str().unwrap_or("")),
        _ => match (
            benchmark["raw_scale_min"].as_f64(),
            benchmark["raw_scale_max"].as_f64(),
        ) {
            (Some(min), Some(max)) => format!("{:.2} / {max}", min + score * (max - min)),
            _ => format!("{:.1}%", score * 100.0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn verified_names_resolve_but_floating_and_private_aliases_do_not() {
        for name in ["GLM-5.3-Flash", "qwen3.8-27b", "deepseek-v4.1-flash-1017"] {
            assert!(catalog()["models"][resolve_id(name).unwrap()].is_object());
        }
        for name in [
            "deepseek-flash",
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "glm5.3-flash-pro",
            "AtomGit-glm5.3-flash",
            "ark-code-latest",
        ] {
            assert_eq!(resolve_id(name), None);
        }
        assert_eq!(catalog()["models"].as_object().unwrap().len(), 12);
    }
    #[test]
    fn known_missing_evidence_is_not_an_unverified_alias_or_a_zero_score() {
        assert_eq!(status("lumen-alpha"), "no_public_data");
        assert_eq!(resolve_id("lumen-alpha"), None);
        assert!(catalog()["models"].get("lumen-alpha").is_none());
        assert_eq!(status("private-model"), "unverified");
        assert_eq!(status("GLM-5.3-Flash"), "available");
    }
    #[test]
    fn published_units_are_preserved() {
        assert_eq!(
            score_label(0.82, &serde_json::json!({"kind":"acc"})),
            "82.0%"
        );
        assert_eq!(
            score_label(1400.0, &serde_json::json!({"kind":"elo"})),
            "1400.0"
        );
        assert_eq!(
            score_label(
                0.8,
                &serde_json::json!({"kind":"acc","raw_scale_min":0,"raw_scale_max":10})
            ),
            "8.00 / 10"
        );
    }
}
