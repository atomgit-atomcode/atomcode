# Windows PowerShell tool

The existing Bash tool keeps its shell selection and command syntax. The local
shell rows additionally expose `pwsh` and `pwsh_start` on Windows when a working
PowerShell executable is detected. This tool name also covers Windows PowerShell
5.1: its model-facing description reports the actual detected version.

Detection follows deepseek-harness's local executor: the PowerShell 7 installation
under ProgramFiles, absolute PATH entries containing pwsh.exe (including quoted
entries), then SystemRoot/System32/WindowsPowerShell/v1.0/powershell.exe. Each
candidate must execute a version probe successfully. Probes run off the async
worker, with a three-second timeout; their result is cached for the process.
Installations made after startup require a restart. PowerShell 6 is not selected.

Commands start a fresh process with NoLogo, NoProfile and NonInteractive. Scripts
are passed using PowerShell's UTF-16LE EncodedCommand form, avoiding an outer
Bash/cmd quoting layer. Output encoding is explicitly UTF-8. Windows native paths
and registry keys are not rewritten. Use single quotes for literals and LiteralPath
for file cmdlets. PowerShell 5.1 does not support PowerShell 7-only operators such
as &&, || and ??. File encodings should be specified explicitly when using shell
cmdlets; dedicated file tools remain preferable for reading and editing files.

Native exit failures and explicit `exit` values remain failures. For intentionally
handled native errors, clear LASTEXITCODE or explicitly exit with the desired
status. Output-filtering/status-printing wrappers must not mask build failures.

`pwsh_start` shares the existing job registry, completion notifications,
`bash_poll` and `bash_kill`. Cancellation, timeouts, process-tree cleanup and output
artifacts use the same execution machinery as Bash.

PowerShell is not analyzed as Bash. Calls are risky and serialized; they do not
inherit allow-all Bash grants, skill preauthorizations or default headless Bash
auto-approval. Workspace analysis asks without caching a grant. Bash deny rules
also constrain this additional execution route. Under shell_guard_policy=strict,
PowerShell is refused because credential analysis currently supports Bash only.
With the default prompt credential policy, unattended children cannot authorize
opaque PowerShell calls themselves. Explicitly disabling guards is not a sandbox.

Remote/sandbox shell worlds must explicitly supply a PowerShell executor. A local
PowerShell must never be substituted for an unsupported remote executor.

## Windows verification

Run the PowerShell-specific capabilities tests on Windows. Verify machines with
only 5.1 and machines with both 5.1 and 7: detection, literal registry paths,
Chinese output, quoted paths, explicit/nonzero native exits, runtime errors,
timeout/cancel, background completion and kill. Registry reads should not need a
nested Bash -> PowerShell invocation. No registry writes are needed for testing.
