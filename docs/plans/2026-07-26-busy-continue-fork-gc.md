# Busy Continue Fork GC Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Persist automatic busy-continue fork lineage, remove abandoned zero-turn forks, and collapse retained fork branches into one logical conversation row.

**Architecture:** `SessionMeta` gains an additive, optional `ForkInfo` field independent from legacy `ImportInfo`. Before publishing a new busy-continue fork, `SessionManager` deletes an older automatic fork only when it can lease it and prove no turn or transcript was added after creation. Forks with content remain durable because the current fork operation does not clone the parent's `.jsonl`; catalog presentation collapses those retained aggregates by root lineage while exact-ID loading remains available. Logical deletion preserves `.lease` and `.meta.lock`.

**Tech Stack:** Rust, serde-compatible native session metadata, `SessionManager`, fs2 leases, native session catalog.

---

### Task 1: Add durable fork lineage

- Add `ForkInfo { root_id, parent_id, forked_at_ms, base_message_count, base_turn_count }`.
- Add additive `#[serde(default)] SessionMeta::fork_info`.
- Validate IDs and timestamps without changing `META_VERSION`.
- Preserve compatibility with metadata written before this field existed.

### Task 2: Reap only proven abandoned forks

- Scan only the current project bucket.
- Acquire and revalidate a candidate's lease and metadata.
- Delete only when message/turn counts and timestamp still equal the fork baseline and JSONL is absent or empty.
- Retain active, progressed, corrupt, pre-lineage, and transcript-owning sessions.

### Task 3: Collapse retained forks in catalog presentation

- Carry optional root lineage in native `CatalogEntry`.
- Keep the most recently updated entry per `(project_bucket, root_id)`.
- Apply collapse only to list/latest presentation surfaces.
- Keep the raw catalog and exact-ID loading unchanged.

### Task 4: Verify

- Run focused session lineage, GC, and catalog tests.
- Run `cargo test -p atomcode-capabilities --features session`.
- Run downstream CLI/daemon compilation and tests.
- Audit that no cleanup removes `.lease` or `.meta.lock`.

