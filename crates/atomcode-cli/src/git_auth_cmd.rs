//! User-owned Git authentication setup and safe local diagnostics.
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use clap::Subcommand;
use serde::Serialize;
use tokio::process::Command;

#[derive(Subcommand)]
pub enum GitAuthCli {
    /// Save a key passphrase in the OS keyring after loading it into the existing agent.
    Remember {
        #[arg(long)]
        key: PathBuf,
        /// Optional agent lifetime in seconds; omitted follows the agent default.
        #[arg(long, value_parser = clap::value_parser!(u32).range(60..=86400))]
        lifetime: Option<u32>,
    },
    /// Load all registered keys from an already-unlocked OS keyring; never ask for passwords.
    Unlock,
    /// Disable automatic loading and delete the saved passphrase (loaded agent keys remain).
    Forget {
        #[arg(long)]
        key: PathBuf,
    },
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
    registered_keys: usize,
    pending_cleanup: usize,
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
        registered_keys: 0,
        pending_cleanup: 0,
    }
}

pub(crate) async fn status_text(root: &Path) -> anyhow::Result<String> {
    let report = inspect(&std::env::current_dir()?).await;
    let (active, pending) = atomcode_auth::git_ssh::registration_status(root)?;
    Ok(format!("SSH agent: {:?}\nGit helper: {:?}\n已登记密钥：{active}；待清理：{pending}\n仅检查本地设置，未验证远端认证或 push 权限。", report.ssh_agent, report.credential_helper))
}

pub async fn handle(command: GitAuthCli) -> anyhow::Result<()> {
    let root = atomcode_coding::config::product_dirs_from_env()
        .user()
        .to_path_buf();
    match command {
        GitAuthCli::Remember { key, lifetime } => {
            use std::io::IsTerminal;
            anyhow::ensure!(std::io::stdin().is_terminal() && std::io::stderr().is_terminal(), "Run remember yourself in an external terminal; passphrases are never accepted through arguments or piped stdin");
            let secret = zeroize::Zeroizing::new(rpassword::prompt_password(
                "SSH key passphrase (hidden): ",
            )?);
            tokio::task::spawn_blocking(move || {
                atomcode_auth::git_ssh::remember(&root, &key, lifetime, secret)
            })
            .await??;
            println!("Passphrase saved in the OS keyring. The key is loaded in the current agent with the requested lifetime; future hosts will attempt automatic loading.");
        }
        GitAuthCli::Unlock => {
            let executable = std::env::current_exe()?;
            let count = tokio::task::spawn_blocking(move || {
                atomcode_auth::git_ssh::unlock_registered(&root, &executable)
            })
            .await??;
            println!("Loaded {count} registered SSH keys into the existing agent.");
        }
        GitAuthCli::Forget { key } => {
            tokio::task::spawn_blocking(move || atomcode_auth::git_ssh::forget(&root, &key))
                .await??;
            println!("Saved passphrase removed and automatic loading disabled. Already-loaded keys remain usable until removed from each agent or their lifetime expires; remove this identity externally for immediate revocation.");
        }
        GitAuthCli::Status { json } => {
            let mut report = inspect(&std::env::current_dir()?).await;
            (report.registered_keys, report.pending_cleanup) =
                atomcode_auth::git_ssh::registration_status(&root)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("SSH agent: {:?}", report.ssh_agent);
                println!(
                    "Registered keys: {}; disabled registrations awaiting cleanup: {}",
                    report.registered_keys, report.pending_cleanup
                );
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
            registered_keys: 0,
            pending_cleanup: 0,
        };
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["remote_authentication_verified"], false);
        assert_eq!(value["credential_helper"], "setting_present");
    }
}
