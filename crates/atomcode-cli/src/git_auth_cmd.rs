//! Local authentication diagnostics. Never retrieve credentials or contact a remote.
use std::{path::Path, process::Stdio, time::Duration};

use clap::Subcommand;
use serde::Serialize;
use tokio::process::Command;

#[derive(Subcommand)]
pub enum GitAuthCli {
    /// Inspect this process's SSH agent and Git helper settings without authenticating.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum AgentStatus {
    IdentitiesLoaded,
    NoIdentities,
    Unavailable,
    ProbeFailed,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum HelperStatus {
    SettingPresent,
    NoSetting,
    ProbeFailed,
}

#[derive(Serialize)]
struct Report {
    ssh_auth_sock_inherited: bool,
    ssh_agent: AgentStatus,
    credential_helper: HelperStatus,
    remote_authentication_verified: bool,
}

// Discard BOTH streams: helper configuration can contain embedded secrets and
// ssh-add prints key identities. Only exit status is needed. Dropping a timed-out
// child kills it; these probes do not invoke helpers, SSH connections or shells.
async fn probe(command: &mut Command) -> Option<i32> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    tokio::time::timeout(Duration::from_secs(5), command.status())
        .await
        .ok()?
        .ok()?
        .code()
}

fn agent_status(code: Option<i32>) -> AgentStatus {
    match code {
        Some(0) => AgentStatus::IdentitiesLoaded,
        Some(1) => AgentStatus::NoIdentities,
        Some(2) => AgentStatus::Unavailable,
        _ => AgentStatus::ProbeFailed,
    }
}

fn helper_status(code: Option<i32>) -> HelperStatus {
    match code {
        Some(0) => HelperStatus::SettingPresent,
        Some(1) => HelperStatus::NoSetting,
        _ => HelperStatus::ProbeFailed,
    }
}

async fn inspect(cwd: &Path) -> Report {
    let mut agent = Command::new("ssh-add");
    agent.arg("-l").current_dir(cwd);
    let mut git = Command::new("git");
    git.current_dir(cwd).args([
        "config",
        "--name-only",
        "--get-regexp",
        r"^credential(\..*)?\.helper$",
    ]);
    let (agent, helper) = tokio::join!(probe(&mut agent), probe(&mut git));
    Report {
        ssh_auth_sock_inherited: std::env::var_os("SSH_AUTH_SOCK").is_some(),
        ssh_agent: agent_status(agent),
        credential_helper: helper_status(helper),
        remote_authentication_verified: false,
    }
}

pub async fn handle(command: GitAuthCli) -> anyhow::Result<()> {
    match command {
        GitAuthCli::Status { json } => {
            let report = inspect(&std::env::current_dir()?).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("SSH agent: {:?}", report.ssh_agent);
                println!(
                    "SSH_AUTH_SOCK inherited: {}",
                    report.ssh_auth_sock_inherited
                );
                println!("Git credential helper: {:?}", report.credential_helper);
                println!("Local checks only; remote authentication and push permission are NOT verified.");
                println!("A helper setting may be empty/reset or scoped to another remote; its presence does not prove a usable credential.");
                println!("For unattended SSH, load the intended key into your agent in an external terminal before starting AtomCode. Ensure the daemon/container sees that agent too; a confirmation-required key can still need a person.");
                println!("For HTTPS, configure/unlock the remote's credential helper externally. Never put passwords or tokens in chat or command arguments.");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_do_not_confuse_missing_agents_with_empty_agents() {
        assert!(matches!(
            agent_status(Some(0)),
            AgentStatus::IdentitiesLoaded
        ));
        assert!(matches!(agent_status(Some(1)), AgentStatus::NoIdentities));
        assert!(matches!(agent_status(Some(2)), AgentStatus::Unavailable));
        assert!(matches!(agent_status(None), AgentStatus::ProbeFailed));
        assert!(matches!(
            helper_status(Some(0)),
            HelperStatus::SettingPresent
        ));
        assert!(matches!(helper_status(Some(1)), HelperStatus::NoSetting));
        assert!(matches!(
            helper_status(Some(128)),
            HelperStatus::ProbeFailed
        ));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn probes_report_spawn_failure() {
        let mut missing = Command::new("atomcode-auth-probe-nonexistent-command");
        assert_eq!(probe(&mut missing).await, None);
    }
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_stalled_probe_has_a_deadline() {
        let start = tokio::time::Instant::now();
        let mut command = Command::new("sleep");
        command.arg("30");
        assert_eq!(probe(&mut command).await, None);
        assert!(start.elapsed() >= Duration::from_secs(5));
    }

    #[test]
    fn status_is_not_a_remote_authentication_claim() {
        let report = Report {
            ssh_auth_sock_inherited: true,
            ssh_agent: AgentStatus::IdentitiesLoaded,
            credential_helper: HelperStatus::SettingPresent,
            remote_authentication_verified: false,
        };
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["remote_authentication_verified"], false);
        assert_eq!(value["credential_helper"], "setting_present");
    }
}
