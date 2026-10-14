# Nonblocking startup update plan

Keep local apply_pending_upgrade and its validation/re-exec/failure circuit breaker. Delete the pre-TUI synchronous manifest/download path and its argument/config gates and duplicate-check flag. The existing detached worker remains the single network preparation path, honoring auto_update, dev, backup, package-managed and offline settings; manual upgrade is unchanged. Prepared updates are applied on the next launch.

Baseline: release/v5.2.4; pending hook/TUI changes are preserved. Verify CLI/updater tests, compile/fmt gates, and a rebuilt release launch against an unresponsive proxy with isolated product directories. No version or dependency changes.

Validation: 292 CLI library tests passed with serialized execution, 146 CLI binary tests and 51 updater tests passed. An initial concurrent CLI library run hit an ACP fixture's global-home race; serialized execution passed without changing ACP code. Compile/fmt/diff gates passed and target/release/atomcode was rebuilt. With auto_update=true, no --dev flag, isolated product directories and a proxy that accepts connections but never responds, the first TUI frame appeared in 0.462 seconds; pending_upgrade=0 ms, runtime_start=193 ms, tui_enter=232 ms. The detached update worker existed and connected to the stalled proxy, proving checks still run in the background. Test processes were stopped and their temporary state removed.

Audit: the CLI/updater tests and compile/fmt/diff gates are repeated in an isolated worktree containing only this startup change; staged hook/TUI changes are excluded. Local update validation, apply retry limits and re-exec failure behavior are unchanged.
