//! Opt-in quality policy. The row schedules ordinary tool calls; it never runs
//! a second shell executor or treats the model's assertions as verification.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use atomcode_capabilities::tools::bash::{ShellExecutionObserver, ShellExecutionReceipt};
use atomcode_config::config::{QualityConfig, QualityGate};
use atomcode_harness::events::{
    AgentRequest, ModelRequest, ModelResponse, RequestError, ToolExec, ToolsExecute, TurnFinishing,
};
use atomcode_harness::seams::{SessionSvc, TurnOutcome};
use atomcode_kernel::message::Role;
use atomcode_kernel::session::SessionEvent;
use atomcode_kernel::tool::{ToolCall, ToolResult};
use atomcode_plexus::{Context, Listener, Next, Plugin, Waterfall};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QualityStatus {
    Unverified,
    Verifying,
    Passed,
    Failed,
    Cancelled,
    Stale,
    NotRequired,
    Waived,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceEvidence {
    pub fingerprint: String,
    pub repository: PathBuf,
    pub files: Vec<(String, String)>,
    pub diff: String,
    pub untracked: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GateEvidence {
    pub name: String,
    pub command: String,
    pub timeout_secs: u64,
    pub started_at_ms: u64,
    pub duration_ms: u64,
    /// Only a matching tool-start fact and a successful result certify zero.
    pub exit_code: Option<i32>,
    pub passed: bool,
    pub output: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QualityEvidence {
    pub id: String,
    pub session: String,
    pub turn: u64,
    pub status: QualityStatus,
    pub reason: String,
    pub before: WorkspaceEvidence,
    pub after: Option<WorkspaceEvidence>,
    pub gates: Vec<GateEvidence>,
    pub configured_gates: Vec<QualityGate>,
    pub evidence_file: Option<PathBuf>,
}

struct Run {
    evidence: QualityEvidence,
    pending: Option<(ToolCall, u64)>,
    receipt: Option<ShellExecutionReceipt>,
    handoff: String,
    input_signature: String,
    finished: bool,
}
struct State {
    enabled: bool,
    enabled_override: Option<bool>,
    run: Option<Run>,
    last: Option<QualityEvidence>,
}

// Session-scoped service supplied by the owning row.
atomcode_plexus::plexus_service!(QualitySvc => QualityControl, "quality", Core, "Session quality policy");

pub struct QualityControl {
    config: QualityConfig,
    state: Mutex<State>,
}
impl QualityControl {
    pub fn new(config: QualityConfig) -> Self {
        Self {
            state: Mutex::new(State {
                enabled: config.enabled,
                enabled_override: None,
                run: None,
                last: None,
            }),
            config,
        }
    }
    pub fn enabled(&self) -> bool {
        self.state.lock().unwrap().enabled
    }

    pub(crate) fn inherit(&self, previous: &Self) {
        let previous = previous.state.lock().unwrap();
        let mut state = self.state.lock().unwrap();
        state.enabled_override = previous.enabled_override;
        state.enabled = previous.enabled_override.unwrap_or(self.config.enabled);
        state.last = previous.last.clone();
    }

    pub(crate) fn restore_evidence(&self, session: &str, root: &Path) -> Result<(), String> {
        {
            let state = self.state.lock().unwrap();
            if state.last.is_some() || state.run.is_some() {
                return Ok(());
            }
        }
        let owner = format!("{:x}", Sha256::digest(session.as_bytes()));
        let directory = root.join(owner);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        let mut latest = None;
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.path().extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let metadata = entry.metadata().map_err(|e| e.to_string())?;
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata.modified().map_err(|e| e.to_string())?;
            if latest.as_ref().is_none_or(|(time, _, _)| modified > *time) {
                latest = Some((modified, entry.path(), metadata.len()));
            }
        }
        if let Some((_, path, size)) = latest {
            if size > 128 * 1024 * 1024 {
                return Err("Quality evidence exceeds the read limit.".into());
            }
            let mut evidence: QualityEvidence =
                serde_json::from_slice(&std::fs::read(&path).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            if evidence.session != session {
                return Err("Quality evidence belongs to a different session.".into());
            }
            evidence.evidence_file = Some(path);
            if matches!(
                evidence.status,
                QualityStatus::Unverified | QualityStatus::Verifying
            ) {
                evidence.status = QualityStatus::Failed;
                evidence.reason =
                    "The previous process ended without completed verification.".into();
                save(&evidence)?;
            }
            self.state.lock().unwrap().last = Some(evidence);
        }
        Ok(())
    }

    pub async fn command(&self, args: &str, cwd: &Path) -> Result<String, String> {
        if let Some(reason) = args
            .trim()
            .strip_prefix("waive ")
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
        {
            let after = workspace(cwd).await?;
            let mut state = self.state.lock().unwrap();
            let mut evidence = state
                .last
                .clone()
                .ok_or("No completed quality attempt to waive.")?;
            if matches!(
                evidence.status,
                QualityStatus::Passed | QualityStatus::NotRequired | QualityStatus::Waived
            ) {
                return Err(
                    "Only an unverified, failed or stale result can be explicitly waived.".into(),
                );
            }
            let id = uuid::Uuid::new_v4().to_string();
            evidence.id = id.clone();
            if let Some(path) = evidence.evidence_file.as_mut() {
                path.set_file_name(format!("{}-{id}.json", evidence.turn));
            }
            evidence.configured_gates = self.config.gates.clone();
            evidence.status = QualityStatus::Waived;
            evidence.reason = format!(
                "Human waiver: {reason}. Previous result: {}",
                evidence.reason
            );
            evidence.after = Some(after);
            save(&evidence)?;
            let text = summary(&evidence);
            state.last = Some(evidence);
            return Ok(text);
        }
        match args.trim() {
            "on" => {
                self.config.validate()?;
                workspace(cwd).await?;
                {
                    let mut state = self.state.lock().unwrap();
                    state.enabled = true;
                    state.enabled_override = Some(true);
                }
                Ok("Quality enabled for ordinary tasks. Configured checks use normal tool approvals; failures preserve your files.".into())
            }
            "off" => {
                {
                    let mut state = self.state.lock().unwrap();
                    state.enabled = false;
                    state.enabled_override = Some(false);
                }
                Ok("Quality disabled. Existing evidence is retained; disabling is not a passing verification.".into())
            }
            "" | "status" | "evidence" => {
                let last = self.state.lock().unwrap().last.clone();
                if let Some(evidence) = last.filter(|e| {
                    matches!(
                        e.status,
                        QualityStatus::Passed | QualityStatus::NotRequired | QualityStatus::Waived
                    )
                }) {
                    let current = workspace(cwd).await;
                    let same_gates = evidence.configured_gates == self.config.gates;
                    if !same_gates
                        || !matches!(current, Ok(ref now) if evidence.after.as_ref().is_some_and(|after| after.fingerprint == now.fingerprint))
                    {
                        let mut state = self.state.lock().unwrap();
                        if let Some(last) =
                            state.last.as_mut().filter(|last| last.id == evidence.id)
                        {
                            last.status = QualityStatus::Stale;
                            last.reason =
                                "Workspace or configured gates changed, or the workspace could not be read after verification.".into();
                            save(last)?;
                        }
                    }
                }
                let state = self.state.lock().unwrap();
                let evidence = state
                    .run
                    .as_ref()
                    .filter(|r| !r.finished)
                    .map(|r| &r.evidence)
                    .or(state.last.as_ref());
                Ok(format!(
                    "Quality: {}\nConfigured gates: {}\n{}",
                    if state.enabled { "on" } else { "off" },
                    self.config
                        .gates
                        .iter()
                        .map(|g| g.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    evidence
                        .map(|e| if args.trim() == "evidence" {
                            serde_json::to_string_pretty(e).unwrap()
                        } else {
                            summary(e)
                        })
                        .unwrap_or_else(|| "No quality evidence yet.".into())
                ))
            }
            _ => Err("Usage: /quality on|off|status|evidence|waive <reason>".into()),
        }
    }
}

fn quality_error(message: impl Into<String>) -> RequestError {
    RequestError {
        policy_denied: true,
        ..RequestError::message(message)
    }
}

fn summary(e: &QualityEvidence) -> String {
    format!(
        "Quality {:?}: {}\n{}",
        e.status,
        e.reason,
        e.evidence_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "Evidence was not saved.".into())
    )
}
fn input_signature(messages: &[atomcode_kernel::message::Message]) -> String {
    let mut hash = Sha256::new();
    for message in messages
        .iter()
        .filter(|m| m.role == Role::User && !m.synthetic)
    {
        hash.update((message.text.len() as u64).to_le_bytes());
        hash.update(message.text.as_bytes());
        for image in &message.images {
            hash.update((image.data.len() as u64).to_le_bytes());
            hash.update(image.data.as_bytes());
        }
    }
    format!("{:x}", hash.finalize())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Fixed read-only Git calls: no shell, textconv, external diff, or optional locks.
async fn git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| "Quality Git snapshot timed out.".to_string())?
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err("Quality requires a readable Git worktree with an initial commit.".into());
    }
    if output.stdout.len() > 32 * 1024 * 1024 {
        return Err("Quality snapshot exceeds the 32 MiB limit.".into());
    }
    Ok(output.stdout)
}

pub async fn workspace(cwd: &Path) -> Result<WorkspaceEvidence, String> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"]).await?;
    let root = PathBuf::from(
        String::from_utf8(root)
            .map_err(|_| "Quality requires UTF-8 Git paths.")?
            .trim_end_matches(['\r', '\n']),
    );
    // A read-only snapshot must not invoke repository-configured clean/process
    // filters. Disable every configured driver for this call, as well as diff helpers.
    let config_keys = git(&root, &["config", "--list", "--name-only", "-z"]).await?;
    let mut diff_args = Vec::<String>::new();
    for key in config_keys.split(|b| *b == 0).filter(|key| !key.is_empty()) {
        let key =
            std::str::from_utf8(key).map_err(|_| "Quality requires UTF-8 Git config keys.")?;
        if key.starts_with("filter.")
            && (key.ends_with(".clean") || key.ends_with(".process") || key.ends_with(".required"))
        {
            diff_args.push("-c".into());
            diff_args.push(format!(
                "{key}={}",
                if key.ends_with(".required") {
                    "false"
                } else {
                    ""
                }
            ));
        }
    }
    diff_args.extend(
        [
            "diff",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=none",
            "HEAD",
            "--",
        ]
        .map(String::from),
    );
    let diff = git(
        &root,
        &diff_args.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await?;
    let head = git(&root, &["rev-parse", "HEAD"]).await?;
    let files = git(
        &root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )
    .await?;
    let untracked_names = git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?;
    tokio::task::spawn_blocking(move || {
        let mut hash = Sha256::new();
        hash.update(&head);
        hash.update(&diff);
        let mut untracked = Vec::new();
        let mut manifest = Vec::new();
        let mut bytes = 0u64;
        let untracked_set: std::collections::BTreeSet<&[u8]> =
            untracked_names.split(|b| *b == 0).collect();
        let mut seen = std::collections::BTreeSet::new();
        for name in files.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            if !seen.insert(name) {
                continue;
            }
            let is_untracked = untracked_set.contains(name);
            let name =
                std::str::from_utf8(name).map_err(|_| "Quality requires UTF-8 Git paths.")?;
            let path = root.join(name);
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    hash.update(name.as_bytes());
                    hash.update(b"\0deleted\0");
                    manifest.push((name.into(), "deleted".into()));
                    continue;
                }
                Err(e) => return Err(e.to_string()),
            };
            bytes = bytes.saturating_add(meta.len());
            if bytes > 256 * 1024 * 1024 || meta.len() > 64 * 1024 * 1024 {
                return Err(
                    "Quality file snapshot exceeds 256 MiB total or 64 MiB per file.".into(),
                );
            }
            let data = if meta.file_type().is_symlink() {
                std::fs::read_link(&path)
                    .map_err(|e| e.to_string())?
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
            } else if meta.is_file() {
                use std::io::Read;
                let mut data = Vec::new();
                std::fs::File::open(&path)
                    .map_err(|e| e.to_string())?
                    .take(64 * 1024 * 1024 + 1)
                    .read_to_end(&mut data)
                    .map_err(|e| e.to_string())?;
                if data.len() > 64 * 1024 * 1024 {
                    return Err("Quality file grew beyond 64 MiB while reading.".into());
                }
                data
            } else {
                return Err(format!("Quality cannot snapshot {}", path.display()));
            };
            let mut file_hash = Sha256::new();
            file_hash.update(data);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file_hash.update(meta.permissions().mode().to_le_bytes());
            }
            #[cfg(not(unix))]
            {
                file_hash.update([u8::from(meta.permissions().readonly())]);
            }
            let digest = format!("{:x}", file_hash.finalize());
            hash.update(name.as_bytes());
            hash.update([0]);
            hash.update(digest.as_bytes());
            manifest.push((name.to_string(), digest.clone()));
            if is_untracked {
                untracked.push((name.to_string(), digest));
            }
        }
        Ok(WorkspaceEvidence {
            fingerprint: format!("{:x}", hash.finalize()),
            repository: root,
            files: manifest,
            diff: String::from_utf8_lossy(&diff).into_owned(),
            untracked,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

fn save(e: &QualityEvidence) -> Result<(), String> {
    use std::io::Write;
    let path = e
        .evidence_file
        .as_ref()
        .ok_or("Quality evidence path is unavailable.")?;
    let dir = path
        .parent()
        .ok_or("Quality evidence directory is unavailable.")?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|e| format!("Quality evidence could not be saved: {e}"))?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(|e| e.to_string())?;
    serde_json::to_writer_pretty(file.as_file_mut(), e).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    file.as_file().sync_all().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) struct QualityPlugin {
    pub control: Arc<QualityControl>,
    pub cwd: PathBuf,
    pub evidence_root: PathBuf,
}
struct QualityRow {
    control: Arc<QualityControl>,
    ctx: Context,
    cwd: PathBuf,
    evidence_root: PathBuf,
}
impl QualityRow {
    fn session(&self) -> Option<Arc<atomcode_harness::session::SessionLog>> {
        let scoped = atomcode_harness::agent::scoped(&self.ctx);
        let log = scoped.service::<SessionSvc>()?;
        // Members have their own logs; only the lead owns this policy.
        if log.header().member.is_some()
            || atomcode_harness::agent::current_member_name(&self.ctx).is_some()
        {
            return None;
        }
        Some(log)
    }
    fn finish(&self, status: QualityStatus, reason: String) -> Result<ModelResponse, RequestError> {
        let mut state = self.control.state.lock().unwrap();
        let run = state
            .run
            .as_mut()
            .ok_or_else(|| quality_error("Quality run is missing."))?;
        run.evidence.status = status;
        run.evidence.reason = reason;
        run.finished = true;
        if let Err(error) = save(&run.evidence) {
            run.evidence.status = QualityStatus::Failed;
            run.evidence.reason = format!("Evidence persistence failed: {error}");
        }
        let evidence = run.evidence.clone();
        let text = format!("{}\n\n{}", run.handoff, summary(&evidence));
        state.last = Some(evidence.clone());
        if matches!(
            evidence.status,
            QualityStatus::Failed | QualityStatus::Cancelled
        ) {
            Err(quality_error(summary(&evidence)))
        } else {
            Ok(ModelResponse {
                text,
                ..Default::default()
            })
        }
    }
    async fn model_handoff(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        let response = next.run(req).await?;
        if !response.tool_calls.is_empty() || response.truncated {
            return Ok(response);
        }
        let after = workspace(&self.cwd).await.map_err(quality_error)?;
        let changed = {
            let mut state = self.control.state.lock().unwrap();
            let run = state.run.as_mut().unwrap();
            let changed = after.fingerprint != run.evidence.before.fingerprint;
            run.evidence.after = Some(after);
            run.handoff = response.text.clone();
            run.input_signature = input_signature(&req.messages);
            changed
        };
        if !changed {
            return self.finish(
                QualityStatus::NotRequired,
                "No workspace changes since this task started; checks were not run.".into(),
            );
        }
        if crate::execution_policy::execution_policy_for_messages(&req.messages)
            .skips_verification()
        {
            return self.finish(QualityStatus::Failed, "The user's execution limits prohibit verification. No checks ran; quality is not certified.".into());
        }
        self.next_gate()
    }

    fn next_gate(&self) -> Result<ModelResponse, RequestError> {
        let mut state = self.control.state.lock().unwrap();
        let run = state.run.as_mut().unwrap();
        let gate = &self.control.config.gates[run.evidence.gates.len()];
        let call = ToolCall {
            id: format!("quality_{}", uuid::Uuid::new_v4()),
            name: "bash".into(),
            arguments: serde_json::json!({"command":gate.command,"timeout":gate.timeout_secs})
                .to_string(),
        };
        run.evidence.status = QualityStatus::Verifying;
        run.pending = Some((call.clone(), now_ms()));
        run.receipt = None;
        save(&run.evidence).map_err(quality_error)?;
        Ok(ModelResponse {
            text: format!("Quality: checking {}.", gate.name),
            tool_calls: vec![call],
            ..Default::default()
        })
    }
}

#[async_trait]
impl Waterfall<AgentRequest> for QualityRow {
    async fn handle(
        &self,
        req: &mut ModelRequest,
        next: Next<'_, AgentRequest>,
    ) -> Result<ModelResponse, RequestError> {
        if !self.control.enabled() {
            return next.run(req).await;
        }
        let Some(log) = self.session() else {
            return next.run(req).await;
        };
        let fresh = self
            .control
            .state
            .lock()
            .unwrap()
            .run
            .as_ref()
            .is_none_or(|r| {
                r.finished || r.evidence.turn != req.turn || r.evidence.session != log.id()
            });
        if fresh {
            if self
                .ctx
                .service::<atomcode_harness::seams::AgentsSvc>()
                .is_some_and(|agents| agents.list().iter().any(|agent| agent.parent().is_some()))
            {
                return Err(quality_error(
                    "Stop delegated agents before starting an ordinary quality task.",
                ));
            }
            // Older evidence remains on disk, but must not represent this new task.
            if let Some(last) = self.control.state.lock().unwrap().last.as_mut() {
                last.status = QualityStatus::Unverified;
                last.reason = "A new task has started; it has not been verified.".into();
            }
            self.control.config.validate().map_err(quality_error)?;
            let before = workspace(&self.cwd).await.map_err(quality_error)?;
            if self.evidence_root.starts_with(&before.repository) {
                return Err(quality_error(
                    "Quality evidence storage must be outside the Git worktree.",
                ));
            }
            let id = uuid::Uuid::new_v4().to_string();
            // Hash the session id rather than interpreting it as a filesystem path.
            let owner = format!("{:x}", Sha256::digest(log.id().as_bytes()));
            let evidence = QualityEvidence {
                id: id.clone(),
                session: log.id().into(),
                turn: req.turn,
                status: QualityStatus::Unverified,
                reason: "No checks have completed for this task.".into(),
                before,
                after: None,
                gates: vec![],
                configured_gates: self.control.config.gates.clone(),
                evidence_file: Some(
                    self.evidence_root
                        .join(owner)
                        .join(format!("{}-{id}.json", req.turn)),
                ),
            };
            save(&evidence).map_err(quality_error)?;
            self.control.state.lock().unwrap().run = Some(Run {
                evidence,
                pending: None,
                receipt: None,
                handoff: String::new(),
                input_signature: String::new(),
                finished: false,
            });
        }
        let pending = self
            .control
            .state
            .lock()
            .unwrap()
            .run
            .as_ref()
            .and_then(|r| r.pending.clone());
        if let Some((call, started)) = pending {
            let new_input = self
                .control
                .state
                .lock()
                .unwrap()
                .run
                .as_ref()
                .is_some_and(|run| run.input_signature != input_signature(&req.messages));
            let result = req
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(&call.id));
            let executed = log.events().iter().any(|e| matches!(&e.event, SessionEvent::ToolStarted { call: actual, .. } if actual.id == call.id && actual.name == call.name && actual.arguments == call.arguments));
            let receipt = self
                .control
                .state
                .lock()
                .unwrap()
                .run
                .as_mut()
                .and_then(|run| run.receipt.take());
            let exit_code = receipt.as_ref().and_then(|r| r.exit_code);
            let passed = executed && exit_code == Some(0) && result.is_some_and(|m| !m.is_error);
            {
                let mut state = self.control.state.lock().unwrap();
                let run = state.run.as_mut().unwrap();
                let gate = &self.control.config.gates[run.evidence.gates.len()];
                run.evidence.gates.push(GateEvidence {
                    name: gate.name.clone(),
                    command: gate.command.clone(),
                    timeout_secs: gate.timeout_secs,
                    started_at_ms: started,
                    duration_ms: now_ms().saturating_sub(started),
                    exit_code,
                    passed,
                    output: receipt
                        .map(|r| format!("{}\n[stderr]\n{}", r.stdout, r.stderr))
                        .or_else(|| result.map(|m| m.text.clone()))
                        .unwrap_or_else(|| "No completed tool result.".into()),
                });
                run.pending = None;
            }
            if new_input {
                {
                    let mut state = self.control.state.lock().unwrap();
                    let run = state.run.as_mut().unwrap();
                    run.evidence.status = QualityStatus::Stale;
                    run.evidence.reason =
                        "New user input superseded this verification attempt.".into();
                    save(&run.evidence).map_err(quality_error)?;
                    let id = uuid::Uuid::new_v4().to_string();
                    run.evidence.id = id.clone();
                    if let Some(path) = run.evidence.evidence_file.as_mut() {
                        path.set_file_name(format!("{}-{id}.json", req.turn));
                    }
                    run.evidence.status = QualityStatus::Unverified;
                    run.evidence.reason =
                        "Continuing with updated user input; checks must run again.".into();
                    run.evidence.gates.clear();
                    run.evidence.after = None;
                    run.pending = None;
                    run.receipt = None;
                    save(&run.evidence).map_err(quality_error)?;
                }
                // The user's steer is actual work, not an automatic repair loop.
                return self.model_handoff(req, next).await;
            }
            if !passed {
                return self.finish(QualityStatus::Failed, "A configured check failed, was denied, or has no execution receipt. Files were retained.".into());
            }
            let current = workspace(&self.cwd).await.map_err(quality_error)?;
            let unchanged = self
                .control
                .state
                .lock()
                .unwrap()
                .run
                .as_ref()
                .unwrap()
                .evidence
                .after
                .as_ref()
                .is_some_and(|e| e.fingerprint == current.fingerprint);
            if !unchanged {
                return self.finish(QualityStatus::Failed, "Files changed while checks were running. Verification is invalid; files were retained.".into());
            }
            let complete = self
                .control
                .state
                .lock()
                .unwrap()
                .run
                .as_ref()
                .unwrap()
                .evidence
                .gates
                .len()
                == self.control.config.gates.len();
            if complete {
                return self.finish(
                    QualityStatus::Passed,
                    "All configured checks passed for the recorded workspace version.".into(),
                );
            }
            return self.next_gate();
        }
        self.model_handoff(req, next).await
    }
}

// Unsupported concurrent work is refused before approval; supported calls keep
// their normal pipeline. Only the shell's typed terminal receipt certifies an exit.
#[async_trait]
impl Waterfall<ToolsExecute> for QualityRow {
    async fn handle(&self, exec: &mut ToolExec, next: Next<'_, ToolsExecute>) -> ToolResult {
        let team_work = exec.call.name == "team"
            && serde_json::from_str::<serde_json::Value>(&exec.call.arguments)
                .ok()
                .is_none_or(|args| !matches!(args["action"].as_str(), Some("status" | "stop")));
        if self.control.enabled()
            && (team_work
                || matches!(
                    exec.call.name.as_str(),
                    "task" | "bash_start" | "pwsh_start"
                ))
        {
            return ToolResult { call_id: exec.call.id.clone(), content: "Quality currently supports foreground ordinary tasks only. Delegation and background execution require disabling quality while idle.".into(), is_error: true, images: vec![] };
        }
        next.run(exec).await
    }
}

struct QualityShellObserver(Arc<QualityControl>);
impl ShellExecutionObserver for QualityShellObserver {
    fn interested(&self, ctx: &atomcode_kernel::tool::ToolContext) -> bool {
        self.0
            .state
            .lock()
            .unwrap()
            .run
            .as_ref()
            .is_some_and(|run| {
                !run.finished
                    && run
                        .pending
                        .as_ref()
                        .is_some_and(|(call, _)| ctx.progress.source_id() == Some(call.id.as_str()))
            })
    }
    fn completed(&self, ctx: &atomcode_kernel::tool::ToolContext, receipt: &ShellExecutionReceipt) {
        let mut state = self.0.state.lock().unwrap();
        if let Some(run) = state.run.as_mut().filter(|run| {
            !run.finished
                && run.pending.as_ref().is_some_and(|(call, _)| {
                    ctx.progress.source_id() == Some(call.id.as_str())
                        && serde_json::from_str::<serde_json::Value>(&call.arguments)
                            .ok()
                            .is_some_and(|args| {
                                args["command"].as_str() == Some(receipt.command.as_str())
                            })
                })
        }) {
            run.receipt = Some(receipt.clone());
        }
    }
}

#[async_trait]
impl Listener<TurnFinishing> for QualityRow {
    async fn call(&self, outcome: &TurnOutcome) -> Option<()> {
        let mut state = self.control.state.lock().unwrap();
        let run = state.run.as_mut().filter(|r| {
            !r.finished
                && r.evidence.session == outcome.session_id
                && r.evidence.turn == outcome.turn
        })?;
        run.finished = true;
        run.evidence.status = if outcome.stop == atomcode_kernel::event::StopReason::Cancelled {
            QualityStatus::Cancelled
        } else {
            QualityStatus::Failed
        };
        run.evidence.reason = format!(
            "Turn ended before quality verification completed: {:?}",
            outcome.stop
        );
        if let Err(error) = save(&run.evidence) {
            run.evidence
                .reason
                .push_str(&format!("; evidence write failed: {error}"));
        }
        state.last = Some(run.evidence.clone());
        Some(())
    }
}

#[async_trait]
impl Plugin for QualityPlugin {
    fn name(&self) -> &'static str {
        "quality-host"
    }
    fn provides(&self) -> &'static [&'static str] {
        &["quality", "shell-execution-observer"]
    }

    fn description(&self) -> &'static str {
        "configured quality checks before ordinary coding handoff"
    }
    async fn apply(&self, ctx: &Context, _: &serde_json::Value) -> Result<(), String> {
        let _ = ctx
            .provide::<QualitySvc>(self.control.clone())
            .map_err(|e| e.to_string())?;
        let _ = ctx
            .provide::<atomcode_harness::seams::ShellExecutionSvc>(Arc::new(QualityShellObserver(
                self.control.clone(),
            )))
            .map_err(|e| e.to_string())?;
        let row = Arc::new(QualityRow {
            control: self.control.clone(),
            ctx: ctx.clone(),
            cwd: self.cwd.clone(),
            evidence_root: self.evidence_root.clone(),
        });
        let _ = ctx.on_waterfall::<AgentRequest>(row.clone(), true);
        let _ = ctx.on_waterfall::<ToolsExecute>(row.clone(), true);
        let _ = ctx.on_parallel::<TurnFinishing>(row);
        atomcode_harness::plugins::self_knowledge::describes_live(
            ctx,
            atomcode_harness::seams::Aspect::Operations,
            "quality",
            60,
            {
                let control = self.control.clone();
                move |_| {
                    Some(format!("Quality policy is {}. /quality on|off|status|evidence|waive <reason> is a human command, independent of approvals. When enabled, the host runs explicitly configured foreground checks before ordinary changed-task handoff. Do not claim quality passed yourself. Failures retain files; no automatic rollback or commit. Goal/loop integration is not supported yet.", if control.enabled() { "on" } else { "off" }))
                }
            },
        );
        Ok(())
    }
}
