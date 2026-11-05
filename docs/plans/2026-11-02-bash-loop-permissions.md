# Bash loop permission composition

Baseline: release/v5.2.6, 3441bc5224e468e4200d5b1589dea3d795e213fc, clean worktree.

The accepted design reuses project/user permissions.allow and permissions.deny.
Do not classify tests as read-only or change concurrency. Parse literal command
lists, pipelines and fixed numeric for loops; each executable segment must have
an allow rule or be strictly read-only, and at least one must be explicitly allowed.
Deny rules also inspect nested invocations. Unknown syntax, expansions, write
redirects and directory changes inside loops remain conservative.

The frontend/runtime owner and middleware ordering stay unchanged. Credential and
sensitive-path gates run before permission rules. Static analysis authorizes syntax,
not the filesystem/network effects of user-authorized programs; no sandbox guarantee
is added. Directory analysis may accept an outer literal cd around a loop with no
inner cd, but repeated/dynamic directory changes still require approval.

Verify unit bypass cases and native runtime permission routing, then affected crate
tests, compile gate, formatting and Cargo.lock. Reference implementations reviewed:
OpenCode shell scanning, Codex execution policies, oh-my-pi compound approval and
deepseek-harness execution sandboxing.

## Implementation notes

- Actual call-site review corrected the report's initial diagnosis: BashTool risk
  is determined by destructive-command detection, not the read-only allowlist.
  That allowlist also controls parallel execution and credential-search exemptions;
  it remains unchanged. Existing ordinary Bash approval defaults remain in force.
- The path gate no longer treats an outer cd around a for loop as repeated cd
  when the loop contains only concrete command names with no directory-changing
  builtins/wrappers. Relative cd, eval/source, dynamic/quoted executable names in
  loops and other loop/function structures retain conservative handling.
- Unknown syntax outside the loop also preserves the path-gate fallback. Shell
  binding/evaluation builtins (including printf, read, trap and directory-stack
  operations) do not earn composed grants: they can change PATH/CDPATH or arrange
  later execution without an assignment AST node. This first version deliberately
  leaves even harmless uses of those builtins to the ordinary approval flow.
- Permission composition supports literal numeric lists (up to 64 entries) and
  lowercase loop identifiers. It rejects expansions, assignments, background jobs,
  unknown structure and writing redirects. Exact full-script grants remain valid
  after structural validation; wildcard grants are matched segment by segment.
- Added unit regressions for the reported cargo command, deny priority, ungranted
  segments, redirects, variable expansion and wrapped/quoted cd. A native runtime
  test executes a project-local script three times and confirms a mixed deny
  prevents execution. User-facing examples are in both approvals pages.

## Verification

- Initial affected-crate run: `cargo nextest run -p atomcode-capabilities -p atomcode-coding --no-fail-fast`: 3,315 passed, 10 skipped (including two existing slow runtime tests).
- Final implementation: `cargo nextest run -p atomcode-capabilities -p atomcode-coding -E 'package(atomcode-capabilities) | binary(permission_rules_e2e)' --no-fail-fast`: 2,135 passed, 1 skipped. Includes final CDPATH and quoted/wrapped directory-change regressions.
- Formatting and diff checks passed; Cargo.lock is unchanged. No actual Ponent project or external model/provider was used: the runtime check uses a synthetic local executable and captured approvals.
- After the last shell-state hardening, all 22 permission/path unit checks passed,
  covering CDPATH assignments, printf variable binding, DEBUG traps and pushd in
  addition to the preceding broad suite. No read-only/concurrency policy changed.
- Final `bash gates/compile.sh`, `cargo fmt --all -- --check` and diff checks passed.
  Cargo.lock remains unchanged; hkdf, hmac and zeroize_derive counts are all zero.
