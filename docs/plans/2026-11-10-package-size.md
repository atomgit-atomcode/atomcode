# Package size optimization implementation plan

**Goal:** Reduce the full offline binary without removing features, and report release size growth.

**Architecture:** Keep the existing runtime, asset API, and release profile. Prefer rust-embed's existing compression support; retain transparent decompression through its API. Filter only redundant KaTeX/Source Serif font source formats before Vite emits assets, preserving every WOFF2 face. Add a report-only size comparison beside release artifacts, using the previous manifest rather than changing the updater schema.

**Tech Stack:** Rust/rust-embed, Vite/Preact, Python standard library, nextest.

## Baseline

`release/v5.2.7` at `c042a8226`; preserve unrelated CLI headless changes. A clean temporary worktree builds the same commit before and after with the same release profile, toolchain, architecture and feature selection. Published v5.2.6 sizes and the November 7 local executable are context, not this experiment's baseline. Parser object constants (~22 MiB before linking) are outside this phase. Do not modify release optimization settings or remove language support.

## Steps

1. Build current unmodified WebUI and macOS arm64 CLI in the clean worktree. Record exact bytes, compiler identity and manifest baseline.
2. Filter KaTeX/Source Serif WOFF/TTF fallbacks while retaining WOFF2 faces, local URLs and the existing browser targets. Test source transformation and final font URL availability; keep browser math rendering and offline behavior.
3. Enable rust-embed compression. Verify index/SPA/API behavior and embedded bytes/MIME types. Measure net release size including decompressor dependencies; retain only a measured improvement.
4. Add a standard-library size reporter that compares same-target artifacts with the previous `latest.json`, emits JSON and readable output, and warns on configurable growth without blocking releases. Integrate it into primary release packaging and CI artifact preparation. Unit-test missing baselines, growth and invalid input.
5. Run WebUI typecheck/build/tests/browser checks, affected daemon tests, format and compile gates. Record measured before/after and platform/official-signing limitations. Never claim downloaded-archive savings as installed-binary savings.

## Outcome

- Same-commit macOS ARM64 executable: 42,766,608 → 40,883,856 bytes, net saving 1,882,752 bytes / 4.40%, including compression code.
- WebUI output: 2,942,167 → 1,945,075 bytes; all WOFF2 faces remain, 94 → 42 embedded files.
- Tests: 430 frontend unit tests; local font/formula rendering; session browsing regression; 418 daemon tests including exact embedded byte/MIME checks; five size reporter tests; compile and format gates.
- Release report integration includes the primary script, Linux-native and macOS cross-build helpers, and three platform release CI jobs. CI builds the WebUI before Rust compilation and retains report JSON; no updater schema/version/profile changes.
- Windows GNU cross-check unavailable locally because the MinGW C compiler is missing. No cross-platform or official-signing size claim is made.
