# Automatic Review Policy Implementation Plan

**Goal:** Allow users to disable the default pre-handoff review guidance independently, including through `/config`, while preserving explicit reviews.

**Architecture:** Keep the tool and `/review` mounted. Resolve `[coding].auto_review` and the environment in the existing host config path, pass the value through provider/agent configuration, and patch the existing review row. This is prompt policy, not a tool execution prohibition; no kernel or session format changes.

**Tech Stack:** Rust, serde/TOML, existing settings catalog and plexus row patches.

1. Add default-true `coding.auto_review` in `atomcode-config/src/config/mod.rs`; expose a boolean with AgentReassemble in `settings.rs`. Test defaults and TOML setting updates.
2. Carry resolved policy through `atomcode-coding/src/config.rs`; test valid/invalid environment values and provider-to-agent transfer. Patch `tool-code-review` in `on_harness.rs`; contribute manual-only or automatic guidance in `review_rows.rs`.
3. Add unrestricted CLI `--no-review`, applying the launch override before runtime configuration. Preserve it across deferred startup/reloads using the host's process environment; it must not edit config files. Test interactive argument parsing.
4. Add runtime criteria checking prompts and the continued presence of tool/command after disabling and rebuilding. Document configuration, precedence, `/config`, and the advisory nature of the policy.
5. Run affected nextest tests, `bash gates/compile.sh`, formatting and diff checks. Review all changes before delivery; do not commit unless requested.

Implementation finding: production disables the standalone `tool-code-review` row and mounts its prepared tool through `host-tools`. Both mount paths must receive the policy. The production default guidance already routes explicit review requests; retain it unchanged when enabled, and add the manual-only policy when disabled. Do not introduce new automatic review calls into that path.
