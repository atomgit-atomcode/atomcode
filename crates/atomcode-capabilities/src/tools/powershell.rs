//! Independent PowerShell dialect. Reuses local process ownership and shell jobs;
//! never interprets PowerShell with Bash's read-only classifier.
use super::{bash::BashStartTool, BashTool, SensitivePaths};
use crate::world::Shell;
#[cfg(any(windows, test))]
use crate::world::{Process, SpawnError, SpawnOptions};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
#[cfg(any(windows, test))]
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const STRICT_POLICY_REASON: &str = "PowerShell is unavailable under shell_guard_policy=strict because credential analysis currently supports Bash only. Use dedicated file tools or Bash; do not retry through another PowerShell spelling.";

pub fn is_powershell_tool(name: &str) -> bool {
    matches!(name, "pwsh" | "pwsh_start")
}

/// DSH resolution order: known PS7 installation, quoted/unquoted PATH entries,
/// then the OS installation of Windows PowerShell. No cwd executable lookup.
#[cfg(any(windows, test))]
fn candidates(program_files: &Path, system_root: &Path, paths: &str) -> Vec<PathBuf> {
    let mut paths_out = vec![program_files.join("PowerShell/7/pwsh.exe")];
    for entry in paths
        .split(';')
        .map(|s| s.trim().trim_matches('"'))
        .filter(|s| !s.is_empty())
    {
        let path = PathBuf::from(entry);
        if path.is_absolute() {
            paths_out.push(path.join("pwsh.exe"));
        }
    }
    paths_out.push(system_root.join("System32/WindowsPowerShell/v1.0/powershell.exe"));
    paths_out.dedup();
    paths_out
}

#[cfg(any(windows, test))]
#[derive(Clone)]
struct PowerShell {
    path: PathBuf,
    version: String,
}
#[cfg(any(windows, test))]
impl PowerShell {
    fn command(&self, script: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.path);
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
        ]);
        // One argv element, no outer Bash/cmd. A newline protects the footer
        // from a final line comment. Native failures must not appear as success.
        let script = format!("[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; $ErrorActionPreference = 'Stop'; $LASTEXITCODE = 0;\n{script}\n${{__atomcode_success}} = $?; if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}; if (-not ${{__atomcode_success}}) {{ exit 1 }}");
        use base64::Engine;
        let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        command.arg(base64::engine::general_purpose::STANDARD.encode(bytes));
        command
    }
}
#[cfg(any(windows, test))]
#[async_trait]
impl Shell for PowerShell {
    fn describe(&self) -> String {
        format!("PowerShell {} on this machine", self.version)
    }
    async fn spawn(
        &self,
        command: &str,
        options: &SpawnOptions,
    ) -> Result<Arc<dyn Process>, SpawnError> {
        super::bash::spawn_local_command(self.command(command), options).await
    }
    fn decode(&self, bytes: &[u8]) -> String {
        super::bash::decode_output(bytes)
    }
}

/// Probe the real executable and version, not merely the presence of a filename.
/// A file-backed stdout avoids waiting on a descendant keeping a pipe open.
#[cfg(windows)]
fn probe(path: &Path) -> Option<PowerShell> {
    use std::os::windows::process::CommandExt;
    use std::{
        io::Read,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let output = tempfile::NamedTempFile::new().ok()?;
    let mut child = Command::new(path)
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Console]::Write($PSVersionTable.PSVersion.ToString())",
        ])
        .stdin(Stdio::null())
        .stdout(output.reopen().ok()?)
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut version = String::new();
    output
        .reopen()
        .ok()?
        .take(128)
        .read_to_string(&mut version)
        .ok()?;
    let version = version.trim().to_string();
    let major = version.split('.').next()?.parse::<u32>().ok()?;
    if major < 5 || major == 6 || (major == 5 && !version.starts_with("5.1.")) {
        return None;
    }
    Some(PowerShell {
        path: path.into(),
        version,
    })
}

pub(crate) fn detected_shell() -> Option<Arc<dyn Shell>> {
    #[cfg(windows)]
    {
        static DETECTED: std::sync::OnceLock<Option<PowerShell>> = std::sync::OnceLock::new();
        DETECTED
            .get_or_init(|| {
                candidates(
                    &PathBuf::from(
                        std::env::var_os("ProgramFiles")
                            .unwrap_or_else(|| "C:\\Program Files".into()),
                    ),
                    &PathBuf::from(
                        std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
                    ),
                    &std::env::var("PATH").unwrap_or_default(),
                )
                .into_iter()
                .filter(|path| std::fs::symlink_metadata(path).is_ok())
                .find_map(|path| probe(&path))
            })
            .clone()
            .map(|shell| Arc::new(shell) as Arc<dyn Shell>)
    }
    #[cfg(not(windows))]
    {
        None
    }
}

pub struct PowerShellTool {
    inner: Arc<dyn Tool>,
    name: &'static str,
    description: String,
}
impl PowerShellTool {
    pub async fn tools(
        world: Arc<dyn Shell>,
        sensitive: SensitivePaths,
        preview: super::output_artifact::ArtifactStoreProvider,
    ) -> Vec<Arc<dyn Tool>> {
        let Some(shell) = world.powershell().await else {
            return Vec::new();
        };
        let description = format!("Execute a PowerShell command directly in {} (not Bash or cmd.exe). Each call starts a fresh noninteractive process with no profile. Use native Windows paths, single-quote literal paths/registry keys, and -LiteralPath for file cmdlets. Backslash is not PowerShell's escape character; backtick is. No automatic path rewriting. If the version is 5.1, do not use &&, ||, ?? or other PowerShell 7-only syntax. Prefer dedicated file tools for reading/editing files. UTF-8 stdout is requested; for file writes explicitly select encoding. Preserve native exit codes; do not mask failures with output-filtering wrappers. PowerShell is unavailable under shell_guard_policy=strict. Commands require conservative approval; Bash grants do not authorize PowerShell. Use pwsh_start for long jobs, then bash_poll/bash_kill for their job IDs.", shell.describe());
        vec![
            Arc::new(Self { inner: Arc::new(BashTool::with_world(shell.clone(), sensitive.clone()).with_label("pwsh").with_output_preview_provider(preview.clone())), name: "pwsh", description: description.clone() }),
            Arc::new(Self { inner: Arc::new(BashStartTool::with_world(shell, sensitive).with_label("pwsh_start").with_output_preview_provider(preview)), name: "pwsh_start", description: format!("Start a background PowerShell job; use bash_poll/bash_kill for its job ID. {description}") }),
        ]
    }
}
#[async_trait]
impl Tool for PowerShellTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }
    fn risk(&self, _: &str) -> RiskLevel {
        RiskLevel::Risky
    }
    fn self_bounds_output(&self) -> bool {
        self.inner.self_bounds_output()
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        self.inner.execute(args, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestWorld {
        offer: bool,
    }
    #[async_trait]
    impl Shell for TestWorld {
        fn describe(&self) -> String {
            "PowerShell 5.1.19041 in test world".into()
        }
        async fn powershell(&self) -> Option<Arc<dyn Shell>> {
            self.offer
                .then(|| Arc::new(Self { offer: false }) as Arc<dyn Shell>)
        }
        async fn spawn(&self, _: &str, _: &SpawnOptions) -> Result<Arc<dyn Process>, SpawnError> {
            Err(SpawnError::Failed("not executed".into()))
        }
    }
    #[tokio::test]
    async fn remote_worlds_opt_in_and_both_tools_are_opaque_and_command_scoped() {
        let preview = Arc::new(|| None) as super::super::output_artifact::ArtifactStoreProvider;
        let guard = super::super::sensitive_path::test_guard();
        assert!(PowerShellTool::tools(
            Arc::new(TestWorld { offer: false }),
            guard.clone(),
            preview.clone()
        )
        .await
        .is_empty());
        let tools =
            PowerShellTool::tools(Arc::new(TestWorld { offer: true }), guard.clone(), preview)
                .await;
        assert_eq!(
            tools.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            ["pwsh", "pwsh_start"]
        );
        for tool in tools {
            let args = r#"{"command":"Get-Content $env:USERPROFILE/.ssh/id_rsa"}"#;
            assert_eq!(tool.risk(args), RiskLevel::Risky);
            assert!(!tool.parallel_safe(args));
            assert!(!tool.read_only_hint());
            assert_eq!(tool.allow_all_group(args), None);
            assert_eq!(tool.always_grant_scope(args), args);
            assert!(tool.description().contains("5.1.19041"));
            assert!(matches!(
                super::super::bash_workspace_gate::bash_workspace_verdict(
                    &guard,
                    tool.name(),
                    args,
                    None,
                    "any"
                )
                .await,
                super::super::bash_workspace_gate::BashWorkspaceVerdict::Ask {
                    grantable: false,
                    ..
                }
            ));
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn all_bash_grants_do_not_authorize_powershell_and_strict_policy_refuses_it() {
        use super::super::approval::{
            ApprovalMiddleware, InMemoryPermissionStore, PermissionStore, APPROVAL_KIND,
            BASH_ALLOW_ALL_KEY,
        };
        use atomcode_kernel::{
            middleware::{BeforeOutcome, ToolMiddleware},
            request::RequestCtx,
            tool::ToolCall,
        };
        let guard = super::super::sensitive_path::test_guard();
        let tools = PowerShellTool::tools(
            Arc::new(TestWorld { offer: true }),
            guard.clone(),
            Arc::new(|| None),
        )
        .await;
        let all = Arc::new(InMemoryPermissionStore::new());
        all.grant(BASH_ALLOW_ALL_KEY);
        let gate = ApprovalMiddleware::with_allow_all_store(
            Arc::new(InMemoryPermissionStore::new()),
            all,
            APPROVAL_KIND.into(),
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = RequestCtx::new(tx, Some(std::time::Duration::from_millis(1)));
        for tool in tools {
            let mut call = ToolCall {
                id: "check".into(),
                name: tool.name().into(),
                arguments: r#"{"command":"Write-Output 'hello'"}"#.into(),
            };
            assert!(!matches!(
                gate.before(&mut call, &tool, &rt).await,
                BeforeOutcome::Proceed | BeforeOutcome::Allow { .. }
            ));
            assert_eq!(
                super::super::credential_bash_gate::credential_shell_verdict(
                    &guard,
                    super::super::CredentialShellPolicy::Prompt,
                    tool.name(),
                    &call.arguments,
                    false
                ),
                super::super::credential_bash_gate::CredentialShellVerdict::Deny
            );
            assert_eq!(
                super::super::credential_bash_gate::credential_shell_verdict(
                    &guard,
                    super::super::CredentialShellPolicy::Strict,
                    tool.name(),
                    &call.arguments,
                    true
                ),
                super::super::credential_bash_gate::CredentialShellVerdict::Deny
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_powershell_preserves_unicode_registry_literals_and_exit_codes() {
        let shell =
            detected_shell().expect("Windows PowerShell 5.1 or PowerShell 7 must be installed");
        for (command, code, expected) in [
            (
                "Write-Output '中文 HKCU\\Software\\Classes'",
                0,
                "中文 HKCU\\Software\\Classes",
            ),
            ("exit 7", 7, ""),
            ("cmd.exe /c exit 9", 9, ""),
            ("Write-Error 'failure'", 1, ""),
        ] {
            let process = shell
                .spawn(
                    command,
                    &SpawnOptions {
                        cwd: None,
                        env: vec![],
                    },
                )
                .await
                .unwrap();
            let result = process
                .collect_within(Some(std::time::Duration::from_secs(10)))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.exit.code, Some(code), "{command}");
            assert!(shell.decode(&result.stdout).contains(expected));
        }
    }

    #[test]
    fn resolution_prefers_seven_and_falls_back_to_system_five() {
        let root = tempfile::tempdir().unwrap();
        let p = root.path();
        let paths = candidates(
            &p.join("programs"),
            &p.join("windows"),
            &format!(
                "\"{}\";;relative;{}",
                p.join("store").display(),
                p.join("custom").display()
            ),
        );
        assert_eq!(
            paths,
            vec![
                p.join("programs/PowerShell/7/pwsh.exe"),
                p.join("store/pwsh.exe"),
                p.join("custom/pwsh.exe"),
                p.join("windows/System32/WindowsPowerShell/v1.0/powershell.exe")
            ]
        );
    }
    #[test]
    fn script_is_one_argument_and_paths_are_not_rewritten() {
        for version in ["5.1.19041", "7.4.0"] {
            let shell = PowerShell {
                path: "powershell.exe".into(),
                version: version.into(),
            };
            let cmd = shell.command("reg.exe query 'HKCU\\Software\\测试'; exit 7 # comment");
            let args: Vec<_> = cmd
                .as_std()
                .get_args()
                .map(|s| s.to_string_lossy().to_string())
                .collect();
            assert_eq!(args.len(), 5);
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&args[4])
                .unwrap();
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            let script = String::from_utf16(&units).unwrap();
            assert_eq!(args[3], "-EncodedCommand");
            assert!(script.contains("'HKCU\\Software\\测试'"));
            assert!(script.contains("exit 7 # comment\n"));
        }
    }
}
