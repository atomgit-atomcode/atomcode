# Workbench content maximize implementation plan

Baseline: release/v5.2.6 e6af903ce071f744500b5e1d73c3504c1654ee8b, existing unrelated work and fetch diagnostics preserved.

Use Workbench-owned ephemeral presentation state. Content maximize uses existing viewport workspace geometry, hiding directory/change lists through CSS without unmounting previews/editors/diffs. Escape restores content before exiting existing workspace fullscreen; tab/session changes restore layout. Add file and Git diff controls, retain selected path in the toolbar. No server, runtime or persistence changes.

Validate with typecheck, WebUI unit tests/build and browser scenarios for preview, editor draft, diff, Escape and existing fullscreen restoration. Rust unchanged by this feature.

Validation: 391 WebUI tests passed. Typecheck and production build passed. Browser regression verified preview scroll preservation, unsaved editor draft preservation, Git diff list collapse, and Escape restoring content before existing workspace fullscreen. Screenshot /tmp/atomcode-workbench-content-maximize.png inspected. Prior fetch-diagnostics workspace compile gate passed; no Rust implementation changes in this feature.

Pre-submit audit (baseline d4287e31d): reviewed shared JSON parsing, fail-closed relay pairing, daemon fallback boundaries, stale directory responses, file/image toolbar behavior, tab/last-file restoration and nested fullscreen Escape. WebUI 391 tests and daemon 388 tests passed; both browser regression scripts passed; typecheck/build, formatting and whitespace checks passed. No Windows real-device reproduction; this change improves diagnostics rather than claiming the reported network root cause is confirmed. Concurrent cd-recent cleanup changes are excluded.
