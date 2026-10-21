# WebUI relay implementation plan

**Goal:** Open the existing WebUI on a phone through the AtomCode relay without a VPN.

**Baseline:** atomcode release/v5.2.4 at 12eaa7018; relay main at 7839cab. Preserve the ongoing LAN QR changes in the CLI, daemon, docs and tests.

**Architecture:** The relay serves the trusted, built WebUI at /webui/. Its existing /api HTTP/SSE tunnel remains unchanged. The browser sends a route credential in X-Atom-Token and the independent WebUI credential in Authorization; no unauthenticated App server is used. Fragment credentials never enter HTTP requests or relay access logs, are removed from the address bar, and survive reload in tab-scoped sessionStorage only. The relay never serves tenant-provided HTML or assets as its trusted UI. Credentials are never attached to external URLs or cookies. Existing local cookie authentication continues to work.

The CLI owns a separate relay client child for WebUI, reuses /app binary discovery, registration and reconnect protocol, and waits for a genuinely authenticated round trip before advertising a QR. /webui stop, terminal logout and process shutdown stop that tunnel. Local browser access remains available if the relay is disabled, old, unavailable or rejects registration. Explicit LAN/custom binding stays local; --local disables remote access. No new runtime owner or conversation persistence.

**Steps:**
1. Add configurable relay static assets, capability discovery, safe headers and deployment instructions. Verify static assets cannot enter the tunnel or escape the configured directory.
2. Add one browser transport shared by API, login and initial auth probe. Test prefix routing, token separation, reload, credential replacement and external URL rejection.
3. Add shared CLI launcher for TUI and standalone WebUI, typed daemon launch target, child cleanup and bounded readiness. Test URL construction, options and readiness failure.
4. Build WebUI; run affected tests and compilation/format gates. Locally exercise the real relay server/client with an authenticated mock daemon and streaming response. Production deployment is a separate action requiring the running relay deployment to be located; do not claim the hosted service was upgraded.

## Verification

- CLI and daemon library regressions: 642 passed. Browser tests: 278 passed; typecheck and production build passed.
- Relay static/auth regression passed; relay workspace all-targets check passed.
- Real relay-server/client test verified trusted static isolation, independent route and daemon authentication, an unbuffered first SSE frame, disconnection and reconnection.
- The optional real AtomCode launcher test used an isolated product directory: readiness completed through the relay; /tunnel/status returned the relay QR; incorrect daemon credentials were rejected; Ctrl+C removed the tunnel.
- Production capability probe returned HTTP 404. The hosted relay has not been upgraded by this task. The user confirmed deployment is on their own server through an internal pipeline. Dockerfiles now package webui-dist; prepare-webui.sh copies a matching release bundle; ATOM_RELAY_WEBUI_DIR selects it. Existing relay-client protocol is unchanged.

## Pre-commit audit

- Fixed unpaired relay pages falling back to local-style requests: they now return an authentication-required result before any network request and ask for a fresh pairing link. Added the regression and retained local cookie behavior.
- Clarified that terminal sign-out stops sharing; WebUI platform-account sign-out retains its existing independent behavior.
- Destinations: AtomCode release/v5.2.4; atomcode-relay main, as explicitly requested.
