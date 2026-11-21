# Tool result image presentation

Baseline 613996b3e, clean isolated checkout. SessionEvent::ToolResultLogged already owns images; Transcript currently ignores them. No event schema, runtime or persistence changes. Emit one immutable image block per returned image, in result order, including orphan/error results. Blocks keep bytes out of text, stay individually visible and stop tool grouping. Click reuses the existing local image opener, independently of composer attachment numbering. Resume/replay rebuilds the same blocks from the authoritative events.

Replace numeric preview cache lookup with exact image-content matching and private randomly named temporary files retained by the TUI. No tool-controlled filename or new dependency. Reject unsupported media types and oversized/invalid base64 before opening. This adds explicit clickable previews, not inline raster painting; the current terminal graphics implementation is a static mascot slot and cannot safely accept arbitrary dynamic images.

Verify projection/order/error/orphan/replay, grouping boundaries, click hit-testing and exact preview identity with a fake opener. Run TUI and i18n suites, format and compile gate.

## Verification

2041 TUI/i18n tests passed, including exact bytes through two real click targets with a fake desktop opener, event replay/order/error/orphan handling, preview identity/privacy/cleanup. Final compile gate, format and diff checks passed. Cargo.lock unchanged; forbidden private package names all zero; no dependencies or binary assets added. Native GUI acceptance is not implied: a fresh diagnostic of the exact experimental App still reports Accessibility=false and Screen Recording=false.
