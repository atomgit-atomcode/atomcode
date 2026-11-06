# Huawei Cloud automatic preinstallation

Owner: CLI host local installation service; live server ownership remains in native MCP/coding.

Register the bundled server disabled at coding/server startup, prepare npm dependencies in an
independent process, and let enabling/reconnecting retry preparation via a Node launcher.
Preserve existing registrations, grants, customized skills and deliberate deletion. No credential
reads or cloud requests. Missing Node/network must not block startup. Serialize cache publication
with an advisory lock. Throttle background retries to one hour, but allow explicit retry immediately.

Validate disabled registration, preserved settings/deletion, installation failure and retry,
concurrent preparation, actual MCP initialize/tools/list, CLI tests and workspace compile gate.

Validation: isolated baseline a107d70c7, integrated onto release/v5.2.7 at 0911feaa4
without unrelated edits. CLI 622/622 tests and adapter 10/10 tests passed. Actual binary
smoke passed offline disabled registration, explicit preparation, removal/recovery of npm
dependencies through the MCP launcher, initialize/tools/list, and detached preparation after
terminating the ACP host. Existing enable/doctor/disable/re-enable smoke passed. Main-worktree
compile gate, format check and asset-index check passed. Lock forbidden-package counts are zero.
No real cloud operations or Windows execution tested. Node >=22.19 and npm remain prerequisites.

Pre-push review found that explicit `cloud enable` bypassed the repair launcher. Both automatic
registration and explicit enable now share launcher generation; the enable smoke deletes dependencies
and verifies tool discovery recovers them. Eight installation tests and both real-binary smoke suites
passed after this correction. Review preserves native MCP approval and runtime ownership.
