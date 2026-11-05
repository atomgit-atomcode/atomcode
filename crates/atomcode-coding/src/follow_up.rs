//! Explicit user-authored follow-ups. Execution is delegated to the existing
//! automation worker; the foreground runtime keeps its session binding.
use crate::config::CodingAgentConfig;
use atomcode_config::schedule::{FollowUpTask, Schedule, ScheduleTask};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewSession {
    pub prompt: String,
    pub cwd: Option<PathBuf>,
    /// A configured selection ID, not an arbitrary provider model name.
    pub model: Option<String>,
    #[serde(default = "plan")]
    pub permission_mode: String,
    /// Only this explicitly supplied reference is transferred, never history.
    pub context: Option<String>,
    #[serde(skip)]
    pub id: String,
}
fn plan() -> String {
    "plan".into()
}
impl NewSession {
    pub fn parse(text: &str, config: &CodingAgentConfig) -> Result<Self, String> {
        let mut options: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if options.prompt.trim().is_empty() || options.prompt.len() > 16384 {
            return Err("follow-up prompt must contain 1..16384 UTF-8 bytes".into());
        }
        if !matches!(options.permission_mode.as_str(), "plan" | "accept_edits") {
            return Err("new-session follow-up mode must be plan or accept_edits".into());
        }
        if options
            .context
            .as_ref()
            .is_some_and(|text| text.len() > 65536)
        {
            return Err(
                "explicit follow-up context exceeds 65536 UTF-8 bytes; nothing was truncated"
                    .into(),
            );
        }
        let cwd = options.cwd.as_deref().unwrap_or(&config.working_dir);
        if cwd.to_str().is_none() {
            return Err("follow-up cwd must be valid Unicode; refusing a lossy path".into());
        }
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err("follow-up cwd must be an existing absolute directory".into());
        }
        let cwd = cwd.canonicalize().map_err(|e| e.to_string())?;
        if cwd.to_str().is_none() {
            return Err("follow-up cwd must be valid Unicode; refusing a lossy path".into());
        }
        options.cwd = Some(cwd);
        let model = options
            .model
            .get_or_insert_with(|| config.provider_name.clone());
        if model.trim().is_empty() {
            return Err("follow-up requires a configured model selection".into());
        }
        options.id = format!("followup-{}", uuid::Uuid::new_v4());
        Ok(options)
    }
    fn task(&self, source_session: &str, source_turn: u64) -> ScheduleTask {
        ScheduleTask {
            id: self.id.clone(),
            title: "One-shot follow-up / 一次性后续任务".into(),
            prompt: format!(
                "[Automatic follow-up / 自动后续任务 — user preset]\n{}",
                self.prompt
            ),
            cwd: self
                .cwd
                .as_ref()
                .expect("validated directory")
                .to_str()
                .expect("validated Unicode directory")
                .to_owned(),
            schedule: Schedule::Manual,
            follow_up: Some(FollowUpTask {
                source_session: source_session.into(),
                source_turn,
                model: self.model.clone().expect("validated selection"),
                context: self.context.clone(),
            }),
            permission_mode: self.permission_mode.clone(),
            notify: "off".into(),
            enabled: true,
            created_at: chrono::Utc::now().timestamp(),
            last_run_at: None,
            last_status: None,
        }
    }
    pub fn launch(
        &self,
        config: &CodingAgentConfig,
        source_session: &str,
        source_turn: u64,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<String, String> {
        let service = config
            .follow_up_service
            .as_ref()
            .ok_or("host does not support new-session follow-ups")?;
        let receipt = service.call(
            "follow_up_launch",
            serde_json::json!({"task":self.task(source_session, source_turn)}),
            &config.working_dir,
            cancel,
        )?;
        Ok(format!(
            "Follow-up / 后续任务 {}: {}{}",
            self.id,
            receipt["state"]
                .as_str()
                .ok_or("invalid follow-up admission response")?,
            receipt["error"]
                .as_str()
                .map(|error| format!(" — {error}"))
                .unwrap_or_default()
        ))
    }
}
pub(crate) fn history(config: &CodingAgentConfig, id: Option<&str>) -> Result<String, String> {
    let service = config
        .follow_up_service
        .as_ref()
        .ok_or("host does not support follow-up history")?;
    let value = service.call(
        "follow_up_history",
        serde_json::json!({"id":id}),
        &config.working_dir,
        &tokio_util::sync::CancellationToken::new(),
    )?;
    serde_json::to_string_pretty(&value).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_schedule::{service::Service, Store};
    fn config(root: &std::path::Path) -> CodingAgentConfig {
        let mut cfg = CodingAgentConfig::new(
            "key",
            "https://example.test",
            "test-selection",
            root,
            atomcode_capabilities::ProductDirs::new(
                root.join("user"),
                atomcode_config::distribution::PROJECT_DIR_NAME,
            ),
        );
        cfg.follow_up_service = Some(std::sync::Arc::new(Service::new(
            Store::new(cfg.dirs.user().join("followups")),
            None,
        )));
        cfg
    }
    #[test]
    fn explicit_options_do_not_copy_history_or_escalate_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        let options = NewSession::parse(r#"{"prompt":"review"}"#, &cfg).unwrap();
        assert!(options.context.is_none());
        assert_eq!(options.permission_mode, "plan");
        assert_eq!(options.model.as_deref(), Some("test-selection"));
        for json in [
            r#"{"prompt":"review","permission_mode":"auto"}"#,
            r#"{"prompt":"review","unknown":true}"#,
            r#"{"prompt":"review","cwd":"relative"}"#,
        ] {
            assert!(NewSession::parse(json, &cfg).is_err(), "{json}");
        }
        let json = serde_json::json!({"prompt":"review","context":"x".repeat(65537)}).to_string();
        assert!(NewSession::parse(&json, &cfg).is_err());
        let json = serde_json::json!({"prompt":"review","context":"explicit\n原文","permission_mode":"accept_edits"}).to_string();
        let options = NewSession::parse(&json, &cfg).unwrap();
        assert_eq!(options.context.as_deref(), Some("explicit\n原文"));
        assert_eq!(options.permission_mode, "accept_edits");
    }
    #[cfg(unix)]
    #[test]
    fn non_unicode_directory_is_rejected_instead_of_targeting_a_different_path() {
        use std::os::unix::ffi::OsStringExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp
            .path()
            .join(std::ffi::OsString::from_vec(b"project-\xff".to_vec()));
        let mut cfg = config(tmp.path());
        cfg.working_dir = path;
        let error = NewSession::parse(r#"{"prompt":"review"}"#, &cfg).unwrap_err();
        assert!(error.contains("lossy path"), "{error}");
    }

    #[test]
    fn new_session_definition_is_durable_claimed_once_and_links_to_native_session() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = config(tmp.path());
        let options = NewSession::parse(r#"{"prompt":"review"}"#, &cfg).unwrap();
        let task = options.task("source-session", 7);
        let store = Store::new(cfg.dirs.user().join("followups"));
        let definition = store.put(&task, None).unwrap();
        let lease = store
            .begin_revision(&task.id, Some(&task.id), Some(&definition.revision))
            .unwrap();
        lease.session("new-session").unwrap();
        lease.finish("success", None).unwrap();
        let reopened = Store::new(cfg.dirs.user().join("followups"));
        assert!(reopened
            .begin_revision(&task.id, Some(&task.id), Some(&definition.revision))
            .is_err());
        let text = history(&cfg, Some(&task.id)).unwrap();
        assert!(text.contains("new-session"));
        assert!(text.contains("source-session"));
        assert!(text.contains("success"));
        assert!(!cfg.dirs.user().join("schedules").exists());
        assert!(history(&cfg, Some("../escape")).is_err());
    }
}
