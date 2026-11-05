# Schedule approval policy implementation plan

Goal: expose `permissions.automation_approval = "always" | "inherit"` in config.toml and the shared `/config` catalog. Default remains per-change human consent. Inherit preauthorizes only schedule mutations while the runtime's current mode is Auto. Plan prohibition, deny rules, project scope, revisions, cancellation and scheduled execution permissions remain authoritative.

Baseline: release/v5.2.6 at 79a39d040. Existing unrelated daemon/lib.rs edits are preserved. CodingRuntime owns live mode flags. The automation capability owns its explicit approval request; host assembly supplies a predicate reading the runtime-owned bypass flag and the explicit configured policy. Standalone automation_tools callers retain the always-ask default. No changes to the generic requires_explicit_approval bypass are necessary, preventing unrelated mandatory approvals from being relaxed.

1. Config: typed strict enum with conservative default, correct nonempty/serialization behavior, bilingual catalog choice and reset/invalid-value tests. CapabilityReprepare applies it through the existing `/config` lifecycle rather than pretending a saved file changes existing tools immediately.
2. Coding: carry the field through Config -> CodingRuntimeConfig -> CodingAgentConfig (including daemon assembly); build the automation predicate from configured policy and the same shared bypass flag runtime mode switches already update.
3. Capability: default-preserving constructor plus explicit host-policy constructor. Only mutating schedule tools consult the predicate; reading remains approval-free. Cancellation still blocks backend calls. Descriptions disclose the conditional approval policy.
4. Tests: default denial without an approver, inherit/Auto automatic calls, mode downgrade requiring approval again, cancellation with preauthorization, parsing/reset and config propagation, actual mounted runtime mode/policy combinations, existing Plan and generic explicit-approval guards.
5. Verification: affected nextest suites, compile gate, rustfmt, lock and diff checks. Document usage and application timing. Do not commit/push without a further request.

## Verification

- `cargo nextest run -p atomcode-config -p atomcode-capabilities -p atomcode-coding`: 3719 passed, 10 skipped.
- Focused configuration propagation and prepared runtime-mode tests: 2 passed.
- `bash gates/compile.sh`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed.
- Cargo.lock contains none of hkdf, hmac, zeroize_derive.
