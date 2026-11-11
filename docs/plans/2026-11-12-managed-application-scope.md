# Managed application scope implementation plan

**Goal:** Let people explicitly configure exact native application access from CLI and /computer, without weakening saved policy on ordinary enable.

**Architecture:** Host-local setup owns MCP configuration. The backend owns execution policy; generic transport requirements enforce its declared protocol on every connection. No kernel/runtime or conversation storage changes. Existing Enable wire variant stays compatible; additive scoped/unrestricted variants carry explicit intents. Configuration commit uses the pre-probe document snapshot, setup lock and optimistic write checks; failure leaves config untouched. Existing reload remains explicit and its established failure semantics are retained.

**Tech stack:** Rust, clap, serde, existing setup lock/atomic writes and MCP connection contract.

1. Extend computer_cmd.rs with mutually exclusive --allow-app (repeatable), --deny-all-apps and --unrestricted-apps. Validate exact bundle IDs with the backend's bounded grammar; normalize/deduplicate. Omitted flags preserve existing scope and capability requirements. Preserve unrelated managed entry/environment properties, continue forcing existing safe approval/foreground settings. Reject ambiguous duplicate entries.
2. Require doctor application_scope.version=1 for scoped setup and persist requiredExperimentalCapabilities atomcode/application-scope=1 alongside the environment policy. Reconnect enforces this independently of doctor. Never infer OS grants from the capability. Reject failed/malformed probes without product writes; reject changed config after probe.
3. Add host-api explicit scoped/unrestricted enable actions, CLI host mapping, TUI parser/help and wire roundtrip tests. Keep old Enable serialization unchanged; use existing Reload command only on explicit reload.
4. Test preservation, deny-all, explicit unrestriction, invalid IDs, unsupported backend, config races, CLI process behavior and slash routing. Run affected nextest suites and full compile gate, fmt/diff/lock checks. Document command behavior, optional patched backend requirement and limits. Real GUI/other-platform acceptance remains unproven until run on authorized hosts.

Baseline: release/v5.2.7 2957787c4; clean main and isolated worktree. Current enable replaces managed env/requirements via generic merge, so preserving the original entry is necessary. Reviewed callers and recent setup/doctor Git history before changes.

## Delivery and validation

Implemented all four tasks above. Existing Enable wire value remains unchanged; additional scoped/unrestricted actions round-trip through the host API. Bundle ID checks match the backend grammar, count and 64-KiB serialized policy bounds. Case normalization/deduplication is deterministic. Preserved generic capability requirements cannot be overwritten by plain enable, even when no application-scope environment is present; unsupported existing scope versions are refused rather than silently downgraded. Ambiguous duplicate managed entries and duplicate capability field aliases are refused. Unrelated environment, allowedTools and extension metadata survive re-enable, while the established safe foreground/approval defaults are still applied.

Verification in the isolated checkout:

- Four affected crates (`atomcode`, `atomcode-host-api`, `atomcode-tui`, `atomcode-i18n`): 2720 passed, 5 skipped. This includes real CLI process setup and addressed host operations with no implicit runtime rebuild.
- After adding the final policy-size bound, 25 focused tests passed (including the oversized-ID-array check, CLI process, host scope actions, slash routing and every-host-variant wire round-trip). Full compile gate then passed: workspace/all-targets, feature-gated targets and both negative controls.
- `cargo clippy -p atomcode --lib` completed without errors; the CLI reports 24 existing warnings, none in the added computer_cmd implementation. Layer and product-directory gates passed (zero product-directory literals). Format and diff checks passed; dependency manifests and Cargo.lock unchanged, normal H lock index status and zero forbidden packages.
- Actual current experimental App (`application-scope-delivery/Open Computer Use (AtomCode Lease).app`) invoked through the built CLI with --allow-app: refused missing OS permissions and created no product tree. Evidence: `/tmp/atomcode-computer-use-acceptance/managed-scope-refusal.json`. No GUI capture/input was performed.

These checks prove setup, persistence, routing and refusal behavior, not allowed-window GUI effects or a signed production install. Configuration is intentionally not live activation; project overrides and explicit reload still apply. The overall audit retains unaccepted native GUI, origin policy, other-platform execution, remote execution-host acceptance and final package-size requirements.
