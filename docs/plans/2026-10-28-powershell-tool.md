# PowerShell tool implementation plan

Goal: add an independent PowerShell command tool on Windows, preferring PowerShell
7 and falling back to Windows PowerShell 5.1, without changing Bash semantics.

Baseline: release/v5.2.5, 7b8baa38a. Other session-reference and changelog changes
are concurrent work and must be preserved. Reference checkout: deepseek-harness
687f282e86, packages/shell/pwsh-local and tool-pwsh.

Architecture: use the existing Shell/Process world and Bash foreground/background
execution machinery; expose pwsh and pwsh_start with independent conservative
approval semantics. Poll/kill continue to use the existing background registry.
Remote worlds must opt in: never silently run a local PowerShell alongside a
remote/sandbox shell. Keep CodingRuntime ownership and kernel unchanged.

Steps:
1. Add deterministic executable candidates (ProgramFiles PowerShell/7, PATH
   pwsh, SystemRoot WindowsPowerShell/v1.0 fallback) and bounded version probing.
   Describe the actual version; handle missing/broken executables explicitly.
2. Share local process spawning/reaping with Bash, passing one script argument
   directly to PowerShell. Disable profile and interactive input, set UTF-8,
   propagate native-command failures, retain user exit codes.
3. Add foreground/background tools sharing timeout, output, cancellation and
   artifact machinery. Keep PowerShell opaque to Bash syntax classifiers: risky,
   no read-only/parallel hint, no inherited all-Bash grant. Preserve deny rules and
   force explicit handling for unresolvable workspace operations.
4. Register from existing shell rows only when their execution world offers it.
   No new runtime lifecycle or separate job registry.
5. Test candidate ordering, 5.1/7 prompts, quoting/unicode, exits, policy and world
   opt-in. Run affected crate tests, compile gate and formatting. Windows 5.1/7
   live validation must be reported accurately if unavailable on this Mac.

This is a fresh nonpersistent PowerShell process per invocation, matching DSH's
nonpersistent executor. It does not introduce DSH's persistent-shell or sandbox
implementation.

Implementation notes: DSH passes one -Command argument; this implementation uses
PowerShell's EncodedCommand to avoid Windows argv quote stripping. UTF-8 output
and native exit propagation are added without changing user scripts' paths.
Detection probes actual versions on a blocking worker and caches the result.
Strict credential policy refuses the opaque dialect; default unattended children
fail closed. Explicit Bash deny rules constrain the new route; Bash allow rules,
skill grants, and default headless Bash authorization do not preauthorize it.

Windows cross-check attempted with x86_64-pc-windows-gnu: blocked before checking
this crate by the missing x86_64-w64-mingw32-gcc required by tree-sitter-bash. No
Windows or installed PowerShell executable is available on this Mac, so live 5.1/7
validation is not claimed. A Windows-only executable integration test is supplied.
