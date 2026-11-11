# Optional browser origin-scope backend

This source-only prototype adds an execution boundary to the externally installed, pinned Playwright MCP runtime. It does **not** change `atomcode browser enable` or bundle Node/Chrome. Managed setup and Codex's complete per-origin access/download/upload/full-CDP matrix remain unfinished.

The local reference is `/Users/theo/Documents/workspace/codex` at `806d9732c9`, especially `codex-rs/config/src/browser_use.rs`. Those types define policy vocabulary; they are not a reusable browser executor. The installed Playwright MCP's own `allowed-origins` option explicitly does not cover redirects, so this prototype does not use it as the enforcement boundary.

## Requirements and policy

- Node 20+ (the pinned Playwright runtime's requirement), installed Chrome, external `@playwright/mcp` **0.0.85** and `playwright-core` **1.66.0-alpha-2026-11-09**. Nothing is installed automatically.
- A trusted `ATOMCODE_BROWSER_ORIGIN_POLICY` environment value, outside model arguments:

```json
{"version":1,"origins":["https://example.test","https://example.test:8443"]}
```

Only exact HTTP(S) origins are accepted; scheme/port matter. Host case/default ports normalize. Credentials, non-root paths, query/fragment, wildcards, unknown fields and malformed/oversized data are refused. An empty list denies all network targets. Omission refuses startup; it never means unrestricted access.

Version 1 permits the listed origins and fixes these additional restrictions:

- No local-file upload, code/evaluate, routing or full-CDP tools. The backend itself rejects direct calls, independently of discovery and AtomCode approvals.
- No accepted download artifacts (`acceptDownloads:false`); HTTP attachment responses are also refused. HTTPS traffic is not decrypted by the proxy, so this is **not** a claim that no download response bytes enter Chrome. Blob/HTTPS download persistence is tested. Normal page reads, text forms and their effects at permitted sites remain possible.
- No caller-chosen artifact filenames or `_meta` argument overrides. Generated screenshots/snapshots use the explicit output root’s `files` directory. The root must be empty or carry this backend’s recognized ownership marker; foreign nonempty directories, leaf symlinks and filesystem roots are refused. Retention never touches the marker or unrelated directories. This is not a filesystem sandbox against the external runtime or another process.

Unsupported allow settings for uploads/downloads/CDP are rejected as unknown policy fields. Do not treat this fixed-denial prototype as the complete per-origin permission matrix.

## Execution and lifecycle

The backend lazily creates its own temporary Chrome profile/context through Playwright's local pipe. It never attaches to a personal browser, imported storage or external CDP endpoint. MCP environment/config-file attachment options are not consumed by this API-based launcher. Service workers, QUIC and non-proxied WebRTC are disabled; the proxy explicitly includes loopback destinations.

A loopback forward proxy checks every HTTP destination and CONNECT authority, including new requests caused by redirects, frames, subresources and popups. Chromium also uses CONNECT for plain WebSockets. CONNECT therefore checks the initial protocol before forwarding: TLS requires HTTPS permission; plain traffic must pass Node's HTTP parser, exact-authority and WebSocket checks. Permission for one scheme is not a raw TCP tunnel for the other scheme on the same port. WSS uses the HTTPS boundary.

`browser_close` waits for this backend's browser/proxy cleanup. Subsequent explicit calls may create a new isolated browser. EOF/SIGTERM close owned resources; a five-second process exit deadline bounds graceful cleanup. Test evidence checks observed descendant PIDs, not arbitrary detached descendants. Run this through an owner such as AtomCode's normal MCP stdio process-group supervision for escalation on hangs. No replay of a lost or cancelled operation is introduced here.

This is a boundary for this browser interface's tested HTTP(S)/WS(S) traffic, not an OS network sandbox or a guarantee about all browser protocols. Shell tools, other browser servers/clients, malicious same-user processes and effects performed by a permitted remote server are outside it. Do not infer global agent restrictions or signed backend attestation from a capability declaration.

## Explicit use

Inspect dependencies/policy without launching a browser or creating an output tree:

```sh
ATOMCODE_BROWSER_ORIGIN_POLICY='{"version":1,"origins":["https://example.test"]}' \
  node /absolute/path/to/atomcode/integrations/browser-origin-scope/backend.cjs \
  --backend-root /absolute/installed/node_modules/@playwright/mcp --doctor
```

For an explicit experimental MCP entry, use resolved absolute paths in the ordinary MCP configuration. This example uses a separate name to avoid overwriting a managed entry:

```json
{
  "mcpServers": {
    "origin-scoped-browser": {
      "command": "/absolute/path/to/node",
      "args": [
        "/absolute/path/to/atomcode/integrations/browser-origin-scope/backend.cjs",
        "--backend-root", "/absolute/installed/node_modules/@playwright/mcp",
        "--output-dir", "/private/owned/browser-artifacts"
      ],
      "env": {
        "ATOMCODE_BROWSER_ORIGIN_POLICY": "{\"version\":1,\"origins\":[\"https://example.test\"]}"
      },
      "requiredExperimentalCapabilities": {"atomcode/browser-origin-scope":1},
      "trust": false,
      "autoApprove": []
    }
  }
}
```

The required capability is checked by AtomCode on every initialize/recovery. It does not activate policy on the stock backend. This prototype requires its own script and policy together. It remains subject to normal MCP approvals and explicit reload. Another unrestricted browser MCP entry is a different interface with different permissions. The existing `/browser` setup commands do not manage this experimental entry; no automatic skill-loading/product integration is claimed. `--headed` is an explicit optional launch flag; acceptance below uses headless Chrome.

## Verification

Fast standard-library-only tests (no browser or external MCP install):

```sh
node --test integrations/browser-origin-scope/policy.test.cjs
```

Explicit real-Chrome acceptance on a Unix host with Chrome, `openssl` and `ps`:

```sh
node integrations/browser-origin-scope/verify-browser.cjs \
  --backend-root /absolute/installed/node_modules/@playwright/mcp \
  --report /absolute/private/acceptance.json
```

The verifier owns two allowed HTTP/HTTPS fixtures, a forbidden fixture, temporary profiles and a temporary certificate. A test-only SPKI pin trusts that one fixture certificate; production does not ignore HTTPS errors. It checks positive permitted traffic as well as zero forbidden TCP connections/HTTP requests, transfers, raw MCP refusals, image output, browser close/reopen and process cleanup. A PNG accompanies the report. No personal browser profile, external website, account or user credential is used. Report hashes measure selected dependency files; they are provenance, not complete package attestation.

The production Rust registry/adapter is also exercised explicitly:

```sh
ATOMCODE_BROWSER_ORIGIN_TEST_ROOT=/absolute/installed/node_modules/@playwright/mcp \
  cargo nextest run -p atomcode --test browser_origin --run-ignored all
```

This verifies direct backend restrictions, redirects, native image results, risky approval classification, required capability mismatch and stale-adapter withdrawal. It does not substitute for full CodingRuntime approval/cancel/unknown-result or real-model acceptance of this new backend.

Local recorded acceptance is macOS 14.5 / Apple Silicon, Chrome 157.0.8103.39, Node 22.22.0. Other operating systems/browser versions are unverified. Full CLI/runtime approval/cancel/unknown-result acceptance for this new backend, managed origin setup, richer per-origin permissions and native GUI/platform work remain in the overall delivery audit.
