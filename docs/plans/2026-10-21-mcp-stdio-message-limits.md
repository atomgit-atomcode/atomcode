# MCP stdio message limits implementation plan

**Goal:** Reject oversized stdio frames before unbounded buffering or body allocation.

**Architecture:** StdioClient remains the transport/lifecycle owner. Shared bounded line reads cover startup, NDJSON and headers. Content-Length bodies are checked before allocation. Errors use the existing protocol-error recovery path; tools/call is never replayed after sending.

**Tech stack:** Rust, Tokio AsyncBufRead, existing MCP subprocess fixture and nextest.

1. Add parser tests for exact limits, missing newlines, split/coalesced reads, invalid/duplicate/overflow lengths and header budgets. Use small in-memory streams rather than allocating a malicious declared body.
2. Implement 10 MiB message and 16 KiB header budgets, including startup reads, without new dependencies or driver changes.
3. Extend the subprocess fixture to send an oversized declaration after tools/call; verify failure, no replay and recovery onto a fresh stream.
4. Run focused tests, capabilities tests with mcp enabled, cargo fmt and gates/compile.sh. Audit the diff and Cargo.lock before delivery.

## Validation

- `cargo nextest run -p atomcode-capabilities --features mcp`: 1240 passed in the development checkout.
- Parser tests cover bounded fragmented/coalesced reads, missing newlines, exact body/header limits, numeric overflow and invalid/duplicate lengths.
- Real subprocess tests cover oversized startup declarations, oversized tool-response declarations and NDJSON, no hidden replay and a usable replacement connection.
- `bash gates/compile.sh`: passed.
- `cargo fmt --all`, `cargo fmt --all -- --check`, and `git diff --check`: passed.
- Cargo.lock unchanged; private dependency marker count is zero.

## Compatibility

Responses larger than 10 MiB are rejected. Valid NDJSON and Content-Length framing remain supported. Existing connection ownership and recovery semantics are retained; this change adds no dependencies or configuration.
