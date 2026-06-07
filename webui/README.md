# AtomCode webui

A local browser UI for AtomCode (Preact + Vite + Tailwind), served by the
`atomcode-daemon` HTTP server. Launch it with `/webui` inside the TUI or
`atomcode webui` from the CLI — both open a loopback-only page in your browser.

## Develop the frontend

```bash
cd webui
npm install
npm run dev          # vite dev server on http://localhost:5173
```

For hot reload against a running daemon, set `ATOMCODE_WEBUI_DEV` so the daemon
redirects page requests to the vite dev server instead of serving the embedded
bundle:

```bash
ATOMCODE_WEBUI_DEV=http://localhost:5173 atomcode webui
# (or run the daemon directly)
ATOMCODE_WEBUI_DEV=http://localhost:5173 cargo run -p atomcode-daemon -- --port 13456
```

API calls still hit the daemon; only the static page is redirected, so you keep
live HMR while talking to the real backend.

## Build for release

```bash
cd webui
npm run build        # outputs webui/dist/
cargo build          # re-embeds webui/dist/ into the binary
```

The compiled assets in `webui/dist/` are committed to the repo and embedded into
the binary at build time via `rust-embed` (see
`crates/atomcode-daemon/src/webui.rs`). After changing frontend code, run
`npm run build` and commit the updated `dist/` so the embedded bundle stays in
sync.

## Build artifacts

`webui/dist/` is **intentionally committed** to the repository. This allows
`cargo build` to produce a working binary in any environment without requiring a
Node.js / npm toolchain — the embedded UI is always available from the committed
snapshot.

The release scripts (`scripts/release.sh`, `scripts/release-daemon.sh`,
`scripts/build-official.sh`, `scripts/linux-release-linux.sh`,
`scripts/macos-release-linux.sh`, `scripts/macos-release-windows.sh`) each
rebuild `webui/dist/` via `npm ci && npm run build` before invoking `cargo
build`, so release binaries always embed the latest frontend. If `npm` is not
available in the build environment, the scripts fall back to the committed
`webui/dist/` with a warning rather than failing.
