# Default execution mode implementation plan

Goal: configure permissions.default_mode in config.toml and /config, preserving Build when unset and explicit driver mode overrides.

Architecture: atomcode-config owns a typed persisted mode; CodingRuntimeConfig maps it to the neutral runtime mode (explicit -y wins). CodingRuntime initializes its existing shared mode flags before mount. No second live mode owner or session format change. Runtime rebuilds retain live mode, and temporary switches do not write config. /config uses NextStartup semantics.

Baseline: release/v5.2.5 at 69c88b063; tracked worktree clean. Existing untracked user files remain untouched.

1. Add typed optional permissions.default_mode, omission/validation tests, and /config choice catalog entry.
2. Map configured mode into CodingAgentConfig through shared CLI and daemon assembly. Initialize existing mode flags before mount; share flag setter with SetMode. Verify mode survives reload and deny rules still win over Auto.
3. Check TUI/headless and ACP initial projections; explicit schedule, background, daemon request modes remain overrides. No new config environment variable or persisted live-mode snapshot.
4. Run affected config/coding and driver tests, compile gate, fmt and diff checks. Record actual coverage and limitations.

Implementation results:

- Added typed optional build/plan/accept-edits/auto default; invalid values fail parsing and an unset permissions table remains omitted.
- /config exposes permissions.default_mode as a choice with NextStartup application. Saving/resetting preserves allow/deny; temporary switches stay runtime-only.
- The existing runtime flags are initialized before mount. Explicit -y is resolved by shared config assembly; the CLI no longer sends a second startup SetMode. Reloads preserve live flags.
- ACP reports the configured startup mode and reads the runtime-owned mode for its session state. Standalone daemon seeds its initial UI/request mode; embedded TUI binding remains authoritative. Explicit request/schedule modes continue to override the default.
- JSON permission imports retain their allow/deny-only DTO so unrelated defaultMode/default_mode fields cannot change live mode or introduce new parse failures.

Validation: config library 329 passed; coding library 623 passed, 8 skipped; runtime criteria 106 passed (including four startup modes, live view, reload preservation and Auto + deny); ACP 88 passed; daemon configuration 1 passed. bash gates/compile.sh passed. Cargo.lock unchanged and hkdf/hmac/zeroize_derive absent. Windows native UI was not exercised; no paid model requests were made.

Scope: /new within an existing runtime and capability/model reloads retain its temporary mode, as before; restarting creates a runtime with the saved default. This is a startup preference, not session-mode persistence or automatic saving of Shift+Tab changes.
