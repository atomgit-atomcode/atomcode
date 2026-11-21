# Schedule CLI review fixes

Baseline: release/v5.2.8 at 291d6709c. Preserve unrelated WebUI script edits.
Owner: schedule Store/Service remains authoritative for validation, CAS and run leases. No permission, persistence or lifecycle changes.

1. Share time normalization with the CLI; constrain mode/notify values in clap. Keep hidden --cron only for actionable unsupported-feature errors.
2. Distinguish notification policies using the existing global duration threshold (important), zero threshold (all), and disabled (off), always preserving the global master switch.
3. Preserve original admission/execution errors when ledger writes fail; replace Sync debug dumps with specific diagnostics.
4. Label UTC next-run timestamps explicitly, trim truncated slugs, improve cwd/canonicalize context.
5. Add regression coverage for these boundaries, including disabled tasks with request IDs; update automation documentation. Run affected crate nextest, compile gate and formatting. Commit scope is limited to these schedule fixes and their documentation.

Validation: 80 schedule CLI/service tests passed. Full CLI run found pre-existing shell_script_matches_rust manifest drift: scripts/uninstall.sh lacks auth/ssh-keys.json and auth/ssh-keys.lock present in the Rust manifest. Neither manifest was changed. Remaining CLI regression: 690 passed, 16 skipped (including the excluded parity cases). Compile gate, cargo fmt check, diff check and lockfile checks passed. Existing unrelated WebUI changes were preserved.
