# Compact WebUI pairing links

Baseline: release/v5.2.5 at dd9886c01d. Preserve unrelated dirty files.

The CLI URL producer encodes lowercase hexadecimal route/token bytes using
unpadded Base64URL, with fragment keys `r` and `t` identifying the short format.
Other injected credentials retain `relay`/`token`. The browser decodes back to
exact original credentials before storage and request headers. No randomness,
authorization, routing, persistent format or server API changes. Old links
remain readable; invalid/mixed short links fail closed and cannot reuse an old
pairing. Credentials stay in the fragment, never query parameters.

Deployment order: the relay hosts its own bundled WebUI independently of the
CLI. Deploy the updated WebUI assets to the relay before releasing a CLI that
prints short links. Old frontends cannot understand the new short keys. The
relay repository's prepare-webui script bundles this build; no production
publish or relay asset update is performed as part of this workspace change.

Validation: shared fixed fixture in Rust and TypeScript checks exact producer /
consumer agreement. Rust also checks that the shortened link produces a
smaller QR using the actual TUI encoder. Browser-side tests cover old format,
reload, request headers, invalid encodings and mixed-format rejection. The
existing blocked-storage handoff applies to both formats.

A QR cannot be scaled by deleting modules. Link compression reduces the
encoded payload; a closable QR overlay is separate presentation work.

Results: 306 frontend tests and all 595 CLI nextest tests passed; TypeScript
check, production build, full compile gate, cargo fmt check and diff whitespace
check passed. The real TUI QR encoder confirms smaller width and height for
the shared fixture. A normal 64-hex route plus 32-hex token URL shrinks from
158 to 119 characters on the production hostname. No dependencies changed.

Audit: test credentials are deterministic SHA-256-derived synthetic fixtures,
never copied from a user pairing URL. Link length reduction is guaranteed for
normal generated credentials; QR version reduction depends on payload content
and encoder mode selection, so not every link must drop a QR version. A
sequential digit-heavy synthetic fixture illustrated that distinction during
review. The representative random-like fixture checks the intended reduction.

Pre-commit audit on release/v5.2.5 at 0b92af02a: full compile gate passed;
312 current frontend tests passed; all three relay URL tests passed after the
synthetic-fixture replacement. Prior 595-test CLI run remains applicable to
unchanged production code. Cargo.lock is unchanged, with no prohibited overlay
packages. Unrelated in-progress WebUI turn-navigation work is excluded.
