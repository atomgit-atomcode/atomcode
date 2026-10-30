# Local LSP symbols and bounded empty-result retry

Baseline: release/v5.2.6 c85a4da1b. Preserve concurrent Workbench changes.
The existing LspManager owns pooled clients/document synchronization. Add local
textDocument/documentSymbol through that owner; preserve feature off/auto/on.
Support hierarchical and flat symbol responses without losing Chinese names.

Server options retry_attempts (total attempts, default 1, range 1..5) and
retry_interval_ms (default 5000, range 0..10000) apply only to successful empty
hover/definition/references/document_symbols queries. No retries on protocol,
transport, capability or configuration failures. One 30-second query budget,
cancellable waits and requests, same client/document, no repeated didOpen.
CLI probe uses the model tool and gains document-symbols without positional args.
Tests use mock transports and paused Tokio time for retries/cancel/deadline.

Validation: 4388 tests passed (10 skipped) across capabilities/config/coding/CLI;
workspace/feature compile gate, its negative controls and formatting passed.
The protocol fixture verifies hierarchical capability advertisement, didOpen
before documentSymbol, and two queries on one client after an empty first result.
CLI regression keeps missing hover coordinates invalid while permitting outlines
without coordinates. Windows Cangjie SDK integration remains unverified locally.

Pre-push audit: narrowed the IDE description to its existing coordinate contract;
58 focused LSP/config/CLI tests and the compilation gate passed again. Cargo.lock
remains unchanged and contains none of hkdf/hmac/zeroize_derive.
