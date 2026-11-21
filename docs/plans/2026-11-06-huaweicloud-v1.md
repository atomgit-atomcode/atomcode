# Huawei Cloud V1 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Provide an opt-in, bundled Huawei Cloud sandbox MCP integration with existing discovery and approval.

**Architecture:** The CLI owns installation, diagnostics and user config edits through ProductDirs. A versioned Node adapter exposes a restricted upstream tool catalog; coding runtime remains the sole live owner and existing MCP search/approval remain authoritative. No cloud operations or credentials are required to install or test.

**Tech Stack:** Rust CLI, Node ESM stdio MCP, vendored Apache-2.0 HuaweiCloud Devkit at 3ab9162868c17534544183c667abbd37113eb32e.

## Task 1: Bundle and constrain the integration

Create `crates/atomcode-cli/assets/huaweicloud/` with upstream runtime, license, provenance, adapter, routing skill and offline protocol tests. Expose sandbox preview, help and diagnostics only; exclude credential reads/writes and general cloud writes. Return MCP tool failures as errors and preserve tool approval defaults. Test missing tool, malformed messages, disabled credential tools, tool pairing and redaction using fake upstream calls.

## Task 2: CLI lifecycle

Create `crates/atomcode-cli/src/cloud_cmd.rs`; register `cloud` in main.rs. Implement enable/status/disable/doctor. Enable checks Node, stages bundled files and dependency installation before publishing MCP config; refuse to replace a user-owned server or skill. Preserve unrelated MCP entries and settings. Disable changes only this server's disabled flag. Use ProductDirs and existing MCP JSON editing functions; no API keys in config.

## Task 3: Verification and handoff

Run Node protocol tests, CLI nextest, cargo fmt and gates/compile.sh. Test enabling/disabling in temporary product directories and failed install rollback. Document local credential setup, opt-in lifecycle, search, approval, prerequisites and preview limitations. Bring only this feature's diff back to the original worktree, preserving ongoing DAP changes. Do not deploy cloud resources, commit or push as part of implementation.
