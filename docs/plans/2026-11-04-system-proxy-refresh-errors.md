# System proxy refresh errors Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Recover brief Windows proxy-setting transitions and explain persistent failures without bypassing the configured proxy.

**Architecture:** Config owns OS reads and a typed, redacted failure. Windows performs bounded snapshot rereads; capabilities maps failures into the existing provider retry contract. Runtime, client ownership and session state remain unchanged.

**Tech Stack:** Rust, winreg, existing provider errors, nextest.

---

Baseline: release/v5.2.6 @ 8a69534a52a4624fa4671234876ab475a71ca5cd.
Unrelated worktree changes are preserved, including the earlier /cd fix.

1. Add safe errors identifying the setting, failure class and OS code, never raw
   registry content or OS error strings. Keep the Option/best-effort entry points
   compatible; request refresh uses the checked entry point.
2. Share Windows read logic with injected readers for cross-platform tests. Read
   ProxyEnable, required ProxyServer, optional ProxyOverride; a disabled or absent
   enable value remains direct. Missing optional bypass values remain valid.
   Detect changes of ProxyEnable while reading an enabled snapshot.
3. Reread incomplete/transient snapshots at most three times with 25/75ms delays.
   Access denied and bad types fail immediately. Exhausted local retries are not
   reclassified into five provider retries. Do not retain a stale/direct fallback.
4. Preserve the failure in capabilities with /proxy guidance, using the existing
   ProviderError channel. Fixed/no-proxy modes do not read OS proxy settings.
5. Test transient recovery, permanent failures, redaction, disabled settings,
   exhausted retries and mode bypass. Run config/capabilities tests, compile and
   format gates. Windows native registry verification remains a manual check.

## Validation

- atomcode-config: 352 tests passed.
- atomcode-capabilities: 1207 tests passed, including route changes, cancellation,
  timeout, retry classification and credential-safe diagnostics.
- Windows config/test targets compile under x86_64-pc-windows-msvc.
- Workspace/feature compile gate passed on an isolated rerun; rustfmt and diff
  checks passed. Cargo.lock is unchanged and has none of the forbidden packages.
- Windows native registry/VPN transitions have not been executed on this Mac.
  Actual field/OS codes from remaining user failures are still needed to identify
  their individual causes. No user proxy settings were modified for tests.
