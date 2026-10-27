//! Adapter for lead-only model tools; all writes remain in Service.
use crate::service::{Mutation, Service};
use atomcode_capabilities::tools::automation::AutomationService;
use atomcode_config::schedule::ScheduleTask;
use serde_json::{json, Value};
use std::path::Path;
fn scoped(cwd: &str, working_dir: &Path) -> anyhow::Result<()> {
    if std::fs::canonicalize(cwd)? != std::fs::canonicalize(working_dir)? {
        anyhow::bail!("automation belongs to another project");
    }
    Ok(())
}
impl AutomationService for Service {
    fn call(
        &self,
        operation: &str,
        args: Value,
        working_dir: &Path,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Value, String> {
        self.model_call(operation, args, working_dir, cancel)
            .map_err(|e| format!("{e:#}"))
    }
}
impl Service {
    fn model_call(
        &self,
        operation: &str,
        args: Value,
        working_dir: &Path,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<Value> {
        if operation == "schedule_list" {
            let entries = self
                .catalog()?
                .into_iter()
                .filter(|entry| scoped(&entry.definition.task.cwd, working_dir).is_ok())
                .collect::<Vec<_>>();
            return Ok(json!({"host_time":chrono::Local::now().to_rfc3339(),"tasks":entries}));
        }
        let mutation = match operation {
            "schedule_create" => {
                let mut task: ScheduleTask = serde_json::from_value(json!({
                    "id":uuid::Uuid::new_v4().to_string(),"title":args["title"],"prompt":args["prompt"],"schedule":args["schedule"],
                    "cwd":working_dir,"permission_mode":args.get("permission_mode").cloned().unwrap_or(json!("plan"))
                }))?;
                task.cwd = std::fs::canonicalize(working_dir)?
                    .to_string_lossy()
                    .into_owned();
                if task.permission_mode == "auto" {
                    anyhow::bail!("choose plan or accept_edits for a new automation");
                }
                Mutation::Create { task }
            }
            "schedule_update" => {
                let task: ScheduleTask = serde_json::from_value(args["task"].clone())?;
                scoped(&task.cwd, working_dir)?;
                scoped(&self.store.get(&task.id)?.task.cwd, working_dir)?;
                Mutation::Update {
                    task,
                    expected: args["expected"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("expected revision is required"))?
                        .into(),
                }
            }
            "schedule_delete" => {
                let id = args["id"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("task id is required"))?;
                scoped(&self.store.get(id)?.task.cwd, working_dir)?;
                Mutation::Delete {
                    id: id.into(),
                    expected: args["expected"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("expected revision is required"))?
                        .into(),
                }
            }
            _ => anyhow::bail!("unknown automation operation"),
        };
        Ok(serde_json::to_value(self.apply_if(
            &uuid::Uuid::new_v4().to_string(),
            &mutation,
            || cancel.is_cancelled(),
        )?)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_tools_share_catalog_and_revisions_but_cannot_edit_another_project() {
        let root = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let service = Service::new(crate::Store::new(root.path()), None);
        let cancel = tokio_util::sync::CancellationToken::new();
        let created=service.call("schedule_create",json!({"title":"Review","prompt":"Review changes","schedule":{"kind":"daily","time":"09:00"}}),project.path(),&cancel).unwrap();
        let task = created["entry"]["task"].clone();
        let revision = created["entry"]["revision"].clone();
        assert_eq!(
            service
                .call("schedule_list", json!({}), project.path(), &cancel)
                .unwrap()["tasks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            service
                .call("schedule_list", json!({}), other.path(), &cancel)
                .unwrap()["tasks"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert!(service
            .call(
                "schedule_delete",
                json!({"id":task["id"],"expected":revision}),
                other.path(),
                &cancel
            )
            .is_err());
        let mut changed = task.clone();
        changed["title"] = json!("Edited");
        service
            .call(
                "schedule_update",
                json!({"task":changed,"expected":revision}),
                project.path(),
                &cancel,
            )
            .unwrap();
        assert!(service
            .call(
                "schedule_update",
                json!({"task":task,"expected":revision}),
                project.path(),
                &cancel
            )
            .is_err());
    }
}
