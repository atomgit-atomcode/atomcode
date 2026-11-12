> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

# Computer Use package-size measurement

Measured production revision: `75c9c8c71` on release/v5.2.7. This is a macOS Apple Silicon default-feature public-source measurement, not a publisher-signed/notarized or private-crypto release. The workspace still declares 5.2.6; no version/release configuration is changed by this audit. [Machine-readable results](2026-11-12-computer-use-package-size.json) preserve binary/package hashes, fingerprints, package members and comparisons. Private logs, binaries and full frontend manifests are under `/tmp/atomcode-computer-use-final-size`.

## Baselines and controls

The original browser-stage baseline `8966af165` already contains the first native Computer Use commit `d8be8b9bf`. It cannot prove the total feature-introduction interval. Therefore measure its parent `eb424a4be` too, retaining the former baseline as a stage comparison. The full 43-commit interval also contains provider/recovery corrections; these observations are cumulative interval sizes, not causal attribution of every byte exclusively to Computer Use.

Three clean, separate source worktrees and fresh Cargo target directories use rustc 1.94.0 (4a4ef493e), target aarch64-apple-darwin, Node v22.22.0 and macOS SDK 14.5. Root Cargo manifest, Cargo.lock, cargo configuration and frontend manifests/lock match; actual CLI Cargo fingerprints (compiler, default feature, release profile, rustflags/config/compile-kind) match. The capabilities manifest legitimately adds existing image/tracing/tokio-util dependencies to its mcp feature during this interval; unchanged locks do not mean all transitive feature edges are unchanged. No Cargo.lock package is added.

Each revision builds its own frontend first, then runs:

```sh
cargo build --release --locked --offline --target aarch64-apple-darwin -p atomcode --bin atomcode
```

The existing release profile remains opt-level=z, LTO=true, codegen-units=1, strip=true and panic=abort. No profile, linker, private overlay, lock flag or build gate is changed. Separate targets avoid the stale cross-worktree workspace artifacts documented by the earlier measurement. Serial cold-target builds took 404.19s (stage), 403.83s (current), 402.86s (initial). These times are context, not a build-performance optimization claim.

## Observed artifact bytes

| Artifact | Before Computer Use eb424a4be | Stage 8966af165 | Current 75c9c8c71 | Full-interval increase |
| --- | ---: | ---: | ---: | ---: |
| Installed standalone executable | 40,983,568 | 41,133,936 | 41,234,000 | 250,432 (0.61%) |
| Single-executable tar.gz | 12,800,596 | 12,893,247 | 12,965,947 | 165,351 (1.29%) |
| npm darwin-arm64 package tarball | 13,090,203 | 13,185,560 | 13,261,001 | 170,798 (1.30%) |
| npm core launcher tarball | 1,255 | 1,255 | 1,255 | 0 |
| Built frontend asset contents | 1,945,075 | 1,945,075 | 1,945,906 | 831 |

The full raw increase is about 244.6 KiB. Compared only with the old stage baseline, the raw increase is 100,064 bytes (0.24%), tar.gz increases 72,700 bytes and npm platform increases 75,441 bytes. Current unpacked npm content totals 41,237,035 bytes across core plus the one matching platform package; content bytes do not include filesystem allocation, npm caches or optional external backends.

The tar.gz uses the Homebrew archive's single `atomcode` layout with controlled uid/gid/mtime and gzip level 6, so changing timestamps do not masquerade as payload growth. It is a normalized local comparison, not byte identity with a timestamped publishing job. npm packages are built offline using the release script's generated platform/core layouts and optionalDependencies, without running its downloader or publisher. Existing `report-package-size.py` reports +0.61% without an advisory growth warning; its five tests pass. No threshold or updater manifest is changed.

## Artifact verification

Each raw binary reports the expected clean revision, and its help surface confirms initial has neither managed command, stage has computer but no browser, current has both. Every normalized archive has exactly one executable and its extracted SHA-256 matches the raw artifact. Every npm platform package contains only `package/bin/atomcode` and `package/package.json`; every core package contains only its launcher and package manifest. All extracted platform binary hashes match. The actual Node launcher runs each extracted package layout and returns the same version as the standalone binary. This is offline pack/layout/wrapper verification, not npm registry installation/resolution or publication.

Each actual release CLI starts an authenticated loopback WebUI with `--no-open --local` in its own isolated product directory. All 42 built resources per revision are fetched over HTTP and match their complete frontend manifests byte-for-byte (126 asset checks total). Each exact owned CLI is stopped by SIGINT and exits 0. No browser, relay, model call, native desktop permission or user configuration is used. HTTP resource serving is not rendered WebUI acceptance.

Source and packaging checks find no added binary-diff path among the interval's 193 changed paths. CLI computer/browser assets add guidance text; optional browser/native execution components remain external. Package member checks find no separate browser/native backend, Node distribution, node_modules, test fixture or target directory. Existing full binary contents, including built frontend and other pre-existing assets, are included in the raw measurement. The interval text patch is 1,378,169 bytes; a strictly validated local non-thin Git object pack is 2,891,091 bytes. Neither number is an actual negotiated network push measurement.

## Limits and delivery

Old first-stage 39,994,192-to-40,027,392 byte observations remain historical. Their record lacks an embedded frontend manifest, and the baseline checkout no longer contains that build's dist; it cannot establish the final complete embedded UI input. The new controlled runs explicitly rebuild and serve each revision's frontend. They do not imply the old build had or lacked any particular asset at its build time.

Only darwin-arm64 is measured. Other target sizes, publisher signing/notarization, the official private crypto feature, external backend installation/browser profile disk use and real frontend GUI workflows are unverified. No multi-platform or signed-release size claim is made, and the overall Computer Use delivery goal remains active. These offline build/pack/smoke checks acquire no optional backend packages; no registry installation was performed.

Builds, package checks and all asset checks pass. The temporary dependency symlinks in the three isolated source worktrees are removed; the user's installed dependencies are preserved. Scratch artifacts remain outside Git. Final cargo fmt/check, diff and clean-lock checks pass. The full compile gate passes workspace/all-targets, feature-gated targets and both negative controls. No production code or dependency is modified by this measurement.
