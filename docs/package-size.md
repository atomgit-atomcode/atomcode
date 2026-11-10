# Executable size and embedded WebUI

The full CLI retains its TUI, offline WebUI, language parsers, MCP and other capabilities. Release optimization remains `opt-level=z`, LTO, one codegen unit, stripped symbols and aborting panics. This change does not introduce a reduced-function edition.

## Embedded assets

The Vite pre-transform keeps every KaTeX and Source Serif 4 WOFF2 face and its weight, style and unicode range, removing only WOFF/TTF fallback sources. The supported Chromium 99, Safari 15 and Firefox 100 browser targets support WOFF2; no external font service or runtime download is introduced. `npm --prefix webui run test:assets` validates built CSS URLs and local formula rendering.

`rust-embed` compresses assets at compile time and transparently returns the original bytes. Its lazy per-file decompression cache avoids decoding the same file on every request. HTTP URLs, MIME types, SPA fallback, API 404s, and `ATOMCODE_WEBUI_DEV` retain their behavior. The binary remains self-contained. First access decompresses a file and retains that decoded allocation, a small CPU/heap tradeoff for less disk storage; this does not promise a reduction in runtime memory.

## Release reports

The main release script compares artifacts before replacing `latest.json`. macOS cross-build helpers and release CI also produce `package-size.json`. CI keeps the JSON as a build artifact. Only CLI executables for the same platform target are compared; daemon binaries, compressed archives and missing targets are excluded.

```sh
python3 scripts/report-package-size.py \
  --artifacts dist/v5.2.7 --baseline latest.json \
  --output dist/v5.2.7/package-size.json
python3 scripts/report-package-size.spec.py
```

The default warning thresholds are 5% or 1 MiB positive growth, configurable with `--warn-percent` and `--warn-bytes`. Growth, a missing baseline or no matching artifacts produce advisory messages, not release failures. Invalid command arguments and output I/O errors still fail. `latest.json` remains an input with its existing updater schema; reports are not additional download assets used by the updater.

A release-to-release warning is a trend signal, not an A/B experiment: features, signing, compiler versions and platform libraries can change. For an optimization measurement, build the same source baseline and candidate on the same target with identical features, compiler and release settings, then compare exact executable bytes. Report frontend bytes separately and count decompression code in the final executable. Signed official builds must be measured separately from an unsigned local build.

## Remaining large components

Tree-sitter parser object constants were about 22 MiB before linking in a prior local release. That is a diagnostic clue, not a measured net executable saving. Language packs/parser changes require separate offline and compatibility work and are outside this optimization.

## Measured result (2026-11-10)

Clean baseline `c042a8226`, macOS ARM64, Rust 1.94.0, default full CLI features, unsigned/open-source build. The baseline and candidate used the same worktree path and release settings. Only the embedded asset configuration and font-source transform changed in the release experiment.

| Metric | Before | After | Reduction |
|---|---:|---:|---:|
| CLI executable | 42,766,608 bytes | 40,883,856 bytes | 1,882,752 bytes (4.40%) |
| WebUI build output | 2,942,167 bytes | 1,945,075 bytes | 997,092 bytes (33.89%) |
| Embedded files | 94 | 42 | 52 legacy font files removed |

The executable saving already includes the decompression implementation; frontend and compression savings are not summed separately. Exact hashes and build identity are in [the measurement record](measurements/2026-11-10-package-size.json). This is an installed-executable comparison, not an archive-size estimate. Windows/Linux and private signed release artifacts were not size-tested locally. A Windows GNU compile attempt could not run without `x86_64-w64-mingw32-gcc`; official Windows/MSVC release CI remains the cross-platform build check.
