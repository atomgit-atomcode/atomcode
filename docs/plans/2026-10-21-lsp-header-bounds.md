# Bounded LSP message headers

Baseline: release/v5.2.4, dda5e950c300fe1d3ef2e0b7853baace55bd548b. Existing unrelated untracked files remain untouched.

Capabilities LSP jsonrpc owns framing; the client owns the existing reader lifecycle. Keep the 16 MiB body limit. Bound each header line to 8 KiB and the entire block to 64 KiB, counting newline bytes. Read with fill_buf/consume and validate remaining budget before appending, including incomplete lines without terminators. Accept exact limits, reject duplicate/case-varied Content-Length, invalid decimal lengths and overflow, and reject excessive declared length immediately. Do not add resynchronization, another reader owner or process lifecycle.

Verify byte-by-byte chunks, exact/over limits, nonterminating long headers, cumulative overflow, bad/duplicate lengths, truncated streams, UTF-8 bodies and back-to-back messages. Client integration must show pending requests fail when oversized headers terminate the existing reader. Run relevant LSP tests and affected capabilities tests with lsp feature, compile gate and fmt/lock audit. No commit/push requested for this phase.

## Result

Replaced unbounded read_line with a bounded fill_buf/consume helper. Counts CR/LF within exact 8 KiB line and 64 KiB block budgets before append; malformed/duplicate lengths and oversized declarations fail immediately. Client production lifecycle is unchanged, with a duplex test showing that an open malicious peer's unterminated header wakes an outstanding request and later requests fail closed.

Validation: the new line-limit regression first failed against the old implementation; final LSP suite passed 37 cases and the entire capabilities lib with lsp passed 1,155 cases. All four compile-gate checks, fmt/diff and lock audits passed. Cargo.lock unchanged. No commit/push.

## Submission audit

User authorized commit/push after review. Audited byte-limit arithmetic, exact CR/LF boundaries, pre-append validation, early length validation and all production consumers of read_message. No blocking findings. Re-ran 37 LSP tests and all four compile-gate checks successfully; prior full capabilities lsp suite passed 1,155 unchanged-patch cases. Final rustfmt, diff and Cargo.lock private-dependency checks passed. Stage only the two LSP source/test files and this plan; unrelated skills and TUI changes remain separate.
