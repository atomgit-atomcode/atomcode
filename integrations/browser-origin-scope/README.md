# Optional browser origin-scope backend

This source-only prototype adds an execution boundary to the externally installed, pinned Playwright MCP runtime. It integrates with explicit `atomcode browser enable` options and bundles neither Node nor Chrome. Codex's complete per-origin access/download/upload/full-CDP matrix remains unfinished.

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
- No caller-chosen artifact filenames or `_meta` argument overrides. Discovery removes these unsupported properties/required entries from the upstream tool schema; direct calls still reject them. Generated screenshots/snapshots use the explicit output root’s `files` directory. The root must be empty or carry this backend’s recognized ownership marker; foreign nonempty directories, leaf symlinks and filesystem roots are refused. Retention never touches the marker or unrelated directories. This is not a filesystem sandbox against the external runtime or another process.

Version 1 rejects additional permission fields. Version 2 below adds explicit controlled HTTP(S) downloads; upload and full-CDP allow settings still fail closed. Do not treat this fixed-denial prototype as the complete per-origin permission matrix.

## Execution and lifecycle

The backend lazily creates its own temporary Chrome profile/context through Playwright's local pipe. It never attaches to a personal browser, imported storage or external CDP endpoint. MCP environment/config-file attachment options are not consumed by this API-based launcher. Service workers, QUIC and non-proxied WebRTC are disabled; the proxy explicitly includes loopback destinations.

A loopback forward proxy checks every HTTP destination and CONNECT authority, including new requests caused by redirects, frames, subresources and popups. Chromium also uses CONNECT for plain WebSockets. CONNECT therefore checks the initial protocol before forwarding: TLS requires HTTPS permission; plain traffic must pass Node's HTTP parser, exact-authority and WebSocket checks. Permission for one scheme is not a raw TCP tunnel for the other scheme on the same port. WSS uses the HTTPS boundary.

`browser_close` waits for this backend's browser/proxy cleanup. Subsequent explicit calls may create a new isolated browser. Pure stdin EOF, a broken stdout pipe and SIGTERM use the same owned cleanup path; a five-second process exit deadline bounds graceful cleanup. Test evidence checks observed descendant PIDs, not arbitrary detached descendants. Run this through an owner such as AtomCode's normal MCP stdio process-group supervision for escalation on hangs. No replay of a lost or cancelled operation is introduced here.

This is a boundary for this browser interface's tested HTTP(S)/WS(S) traffic, not an OS network sandbox or a guarantee about all browser protocols. Shell tools, other browser servers/clients, malicious same-user processes and effects performed by a permitted remote server are outside it. Do not infer global agent restrictions or signed backend attestation from a capability declaration.

## Explicit use

Inspect dependencies/policy without launching a browser or creating an output tree:

```sh
ATOMCODE_BROWSER_ORIGIN_POLICY='{"version":1,"origins":["https://example.test"]}' \
  node /absolute/path/to/atomcode/integrations/browser-origin-scope/backend.cjs \
  --backend-root /absolute/installed/node_modules/@playwright/mcp --doctor
```

Managed setup requires explicit installed paths and an origin policy on first use:

```sh
atomcode browser enable \
  --origin-backend /absolute/path/to/atomcode/integrations/browser-origin-scope/backend.cjs \
  --backend-root /absolute/installed/node_modules/@playwright/mcp \
  --allow-origin https://example.test
/browser enable --allow-origin https://example.test --allow-origin https://example.test:8443
/browser enable --origin-policy-file /absolute/trusted-policy.json
/browser enable --deny-all-origins
/browser enable --unrestricted-origins
/browser reload
```

Repeat `--allow-origin` to permit more exact origins. Plain `enable`, including after disable, preserves the saved scope. Unrestriction is explicit and returns to the stock `cli.js` in the saved package (or terminal `--server`). Other required capabilities and narrower tool restrictions remain. Missing/malformed scope metadata fails rather than falling back to stock. A changed config snapshot during doctor is refused without overwriting it. Doctor launches neither a browser nor an installer and creates no output tree. Scope setup writes the validated policy contents, exact arguments, managed mode marker and exact-version required MCP capability together. `--origin-policy-file` accepts bounded v1/v2 JSON from an absolute trusted execution-host file; its contents are saved rather than re-read on later enable/doctor. It conflicts with the other policy selectors. CLI paths follow normal shell quoting; in the slash command the entire remainder after the option is the path, without quotes, so spaces are supported. Existing sessions still need explicit reload; status describes configuration, not live adoption or project overrides.

The installed bundled browser guidance recognizes either backend. Its exact previous unmodified version can upgrade; customized guidance remains protected. Other unmanaged MCP entries are never rewritten by these commands.

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

The required capability is checked by AtomCode on every initialize/recovery. It does not activate policy on the stock backend. This prototype requires its own script and policy together. It remains subject to normal MCP approvals and explicit reload. Another unrestricted browser MCP entry is a different interface with different permissions. The managed `/browser` commands manage only their own `browser-use` entry, not this separately named manual entry. The browser-use skill remains explicitly selected rather than automatically loaded. `--headed` is an explicit optional launch flag; acceptance below uses headless Chrome.

## Remote execution and approvals

Configure this optional backend on the machine running the MCP host. For daemon `/chat`, that is the daemon machine; the browser/frontend does not move execution to its own desktop. ACP similarly uses the agent-side configured MCP host. A loaded browser skill does not prove a frontend-local browser is being controlled.

Daemon `/chat` clients must echo both `call_id` and the one-shot `approval_id` from each `permission_request`, together with `session_id`, for every `/chat/permission` choice. Missing/stale/duplicate IDs return `success:false`; older clients need to upgrade with the daemon. This does not replace origin policy or grant a new permission. The current WebUI and IDE senders carry the IDs. See [authenticated daemon acceptance](../../docs/plans/2026-11-12-remote-browser-approval-correlation.md) for actual API allow/deny/disconnect/stop/cancel evidence and the limits of that acceptance. SSE disconnection alone leaves the operation available for recovery; use explicit stop when cancellation is intended.

ACP v1/v2 [actual stdio acceptance](../../docs/plans/2026-11-12-acp-browser-images.md) additionally covers ordinary approvals with argument previews, a submitted form, standard live image content, pending denial/cancellation and historical screenshots. v1 `session/load` replays history; plain v1 `session/resume` is silent. The v2 draft uses `session/resume` with `replayFrom: {"type":"start"}` and reports turn completion through an idle state update. A cancelling ACP client must also answer outstanding permission requests with the protocol's `Cancelled` outcome. These checks do not prove in-flight/disconnect faults, arbitrary editor rendering or another host OS.

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

The verifier owns two allowed HTTP/HTTPS fixtures, a forbidden fixture, temporary profiles and a temporary certificate. A test-only SPKI pin trusts that one fixture certificate; production does not ignore HTTPS errors. It checks positive permitted traffic as well as zero forbidden TCP connections/HTTP requests, transfers, raw MCP refusals, image output, browser close/reopen and process cleanup. Pure EOF is tested with a raw transport that sends no signals; broken stdout is tested during an in-flight call while stdin remains open. SDK transport close is a separate case because it escalates to SIGTERM/SIGKILL. A PNG accompanies the report. No personal browser profile, external website, account or user credential is used. Report hashes measure selected dependency files; they are provenance, not complete package attestation.

The production Rust registry/adapter is also exercised explicitly:

```sh
ATOMCODE_BROWSER_ORIGIN_TEST_ROOT=/absolute/installed/node_modules/@playwright/mcp \
  cargo nextest run -p atomcode --test browser_origin --run-ignored all
```

This exercises real managed setup followed by production configuration loading, redirects, native image results, risky approval classification, required capability mismatch and stale-adapter withdrawal. The backend itself is also independently tested through raw MCP in the Node verifier. It does not substitute for full CodingRuntime approval/cancel/unknown-result or real-model acceptance of this new backend.

Local recorded acceptance is macOS 14.5 / Apple Silicon, Chrome 157.0.8103.39, Node 22.22.0. Other operating systems/browser versions are unverified. Managed setup, actual runtime approval-denial, and real adapter/transport fault acceptance have local evidence. The latter covers a successful submitted click whose response is lost, committed effects followed by cancellation/forced future drop, late success which cannot restore the old client, explicit reconnect without replay, Drop cleanup and awaited shutdown. These adapter tests do not by themselves prove the complete CodingRuntime turn-cancel or remote-driver flow. A [real-model scoped workflow](../../docs/plans/2026-11-12-origin-browser-real-model-acceptance.md) separately passed the actual TUI approval, form/snapshot/image, forbidden-navigation and close path on the recorded macOS baseline. [Actual local TUI runtime cancellation](../../docs/plans/2026-11-12-browser-runtime-cancel-acceptance.md) additionally verifies an externally committed/held submission, native Cancelled terminal, late HTTP response isolation, explicit reload, re-observation and screenshot without replay. The same acceptance exposed and corrected an advertised filename argument that execution refused. Pending approval dismissal was separately denied before dispatch. Richer per-origin permissions, remote fault acceptance and native GUI/platform work remain in the overall delivery audit.

Run the stock and scoped fault suite explicitly (all pages/profiles are temporary local fixtures):

```sh
ATOMCODE_BROWSER_TEST_SERVER=/absolute/installed/node_modules/@playwright/mcp/cli.js \
ATOMCODE_BROWSER_ORIGIN_TEST_ROOT=/absolute/installed/node_modules/@playwright/mcp \
ATOMCODE_BROWSER_TEST_ARTIFACTS=/absolute/private/fault-evidence \
  cargo nextest run -p atomcode --test browser_live --run-ignored all --test-threads 1
```

Evidence is separated by stock/scoped case directories. Result JSON records the external submission counter and, for fault cases, actual click RPC/spawn counts. Scoped cancellation and forced drop require one old spawn/one click, an observed late successful reply which leaves the client poisoned, and exactly one explicitly requested replacement. Captured PIDs are checked synchronously with signal 0 before an awaited cleanup assertion, not after spawning a separate PID-check process.

The first strengthened real Drop test exposed missing EOF handling in the optional backend: the pinned SDK server transport listens to data/error but not stdin end. Its client close helper's signal escalation had masked this in earlier verification. The process host now sends EOF/broken-stdout into the existing cleanup path. Earlier SDK-close evidence should not be read as pure-EOF proof; the separate raw transport cases provide that proof now.

ACP stdio in-flight cancellation and EOF acceptance is recorded in [the fault report](../../docs/plans/2026-11-12-acp-disconnect-acceptance.md). A disconnected client cannot receive a wire terminal: the host now awaits its existing bounded session teardown so the native cancellation is committed before exit. Explicit session close/resume permits re-observation of an uncertain effect; it does not retry that operation. Other transports and pending-permission EOF are not established by these runs.

The [WebUI image report](../../docs/plans/2026-11-12-webui-tool-images.md) records corrected chat/live tool-result image payloads and native historical image association. WebUI preserves them in expandable tool details and counts their bytes in its existing memory budget. Actual /chat and history API image equality is verified; rendered WebUI/live browser workflows remain unaccepted.

## Download expansion: completed-artifact counterexample

The original cancellation-based per-origin download draft was rejected. The current v2 implementation instead owns the explicit HTTP(S) download publication path. The [publication-boundary decision](../../docs/adr/0032-origin-downloads-remain-backend-owned.md) records why a download-event cancel listener is insufficient: in a real delayed-event case, cancel resolves but the pinned MCP still saves an already completed denied artifact. That event-gate draft was withdrawn; both current policy versions retain acceptDownloads=false and HTTP attachment refusal for browser actions.

Run the source-only opt-in characterization on the recorded macOS host:

```sh
node integrations/browser-origin-scope/characterize-downloads.cjs \
  --backend-root /absolute/installed/node_modules/@playwright/mcp \
  --report /absolute/new-download-characterization.json
```

The report path must be new. This is a test, never an MCP backend to register: it intentionally enables automatic downloads only inside isolated synthetic loopback TLS fixtures, pins only their ephemeral certificate, and preserves one denied fixture payload as the negative-control evidence. Ordinary samples (one allowed and seven denied/redirects) look successful; a separate one-second delayed event exposes the completed-download race. The delay represents possible scheduling latency, not a measured frequency. Report data includes actual external runtime/source hashes, verifier hash, cases, cancellation events and exact captured process cleanup. Owned fixture/artifact directories remain private outside Git; no personal browser, model call, user configuration or OS grant is used.

The explicit publisher below now owns validation at every redirect and artifact publication. Further transfer support must preserve that boundary; blob handling, authenticated context, file capabilities for uploads, full-CDP boundaries remain required work.


## Version 2 controlled downloads

A separately named explicit experimental MCP entry can select this policy:

```json
{"version":2,"origins":{"https://example.test":{"access":"allow","downloads":"allow","uploads":"deny","full_cdp_access":"deny"},"https://read-only.test":{"access":"allow","downloads":"deny"}}}
```

Set its `ATOMCODE_BROWSER_ORIGIN_POLICY` environment value to that JSON string and `requiredExperimentalCapabilities` to `{"atomcode/browser-origin-scope":2}`. Keep `trust:false`, `autoApprove:[]`, the absolute backend script/package paths and owned output directory shown above. For managed setup, use terminal `atomcode browser enable --origin-backend /absolute/backend.cjs --backend-root /absolute/installed/node_modules/@playwright/mcp --origin-policy-file /absolute/trusted-policy.json`, then `/browser reload`. Later `/browser enable --origin-policy-file /absolute/trusted-policy.json` can replace the saved policy. Plain enable and disable/re-enable preserve it even if the source file is removed. Managed metadata, policy and capability versions must match; a v1 doctor cannot satisfy v2.

Omitted permissions deny. Access and download grants are required at **every** HTTP(S) redirect. Duplicate canonical origins and upload/full-CDP allow settings are refused. Only policies with at least one download grant advertise `browser_download`; direct calls to it otherwise fail. Its required argument is `url`; optional `use_browser_context` and `sync_response_cookies` both default to false. Synchronization requires explicit browser-context authentication. It writes complete identity-encoded HTTP 200 responses up to 64 MiB into private staging, then publishes an exclusive generated `.bin` path and reports byte count/SHA-256. Response filenames, caller paths/headers and URL credentials are never accepted. Compressed responses and blob URLs fail closed in this initial implementation. Default calls forward no cookies. Explicit browser-cookie authentication is described below; ordinary response-cookie synchronization is available explicitly; partitioned cookies and other authentication methods remain unfinished.

Normal navigation/click downloads remain disabled, even with a grant. Invoke the explicit tool initially for a known link, rather than automatically fetching a URL after an uncertain browser action. Download GETs can have remote effects. Cancellation or failure never retries automatically; cancellation before publication leaves no accepted artifact. Once publication commits, a lost response or late cancellation cannot undo the file, so an uncertain result must be re-observed rather than replayed. Existing SDK artifact retention may later evict files; this is not durable personal document storage.

New download-enabled entries include `browser_download` in their tool filter. Existing `allowedTools` restrictions are preserved, so an upgraded entry may still hide it: explicitly add that one name to its saved `allowedTools` if desired, then reload. An origin download grant never widens an existing tool filter or auto-approves a call.

MCP cancellation notifications target the owned request ID. `browser_close` drains transfers and awaits browser cleanup before completing; later explicit work is possible. EOF/output loss/termination permanently fences new work and drains owned transfers. These controls add no Node, Chrome or npm dependencies to AtomCode's package.

Run both fast suites and opt-in actual MCP/isolated Chrome acceptance:

```sh
node --test integrations/browser-origin-scope/policy.test.cjs integrations/browser-origin-scope/transfers.test.cjs
node integrations/browser-origin-scope/verify-downloads.cjs \
  --backend-root /absolute/installed/node_modules/@playwright/mcp \
  --report /absolute/new-controlled-download-report.json
```

The verifier requires Chrome, Node 20+, `ps`, `openssl req -addext` and loopback IPv6; it uses synthetic loopback HTTP(S) fixtures and a private temporary output tree. It checks bytes/hash, denied redirects before dispatch, argument refusal, cancellation without replay, browser implicit download denial, close/reuse and pure EOF. The current verifier additionally covers explicit HttpOnly cookie authentication, cross-host cookie isolation, HTTPS with a generated fixture-only CA, untrusted certificate refusal and authenticated cancellation/close. Ordinary response-cookie rotation/deletion and original-tab preservation are additionally verified. Partitioned response-cookie writing, embedded partition contexts, other authentication methods, blob/compression, native runtime turn cancellation of the new tool and the complete upload/CDP matrix still require further implementation/acceptance. It does not open a personal profile or change OS grants.


### Explicit browser-cookie downloads

Login through the backend's isolated browser first, then call:

```json
{"url":"https://example.test/private/report","use_browser_context":true}
```

The argument is explicit and visible in normal MCP approval previews. It never imports a personal profile, launches a missing browser or adopts a replacement context. Access and download grants must hold before any credential lookup and at every redirect. The existing context's public `cookies(url)` API selects each destination's cookies anew; the publisher additionally checks domain, path, expiration and Secure rules, uses explicit first-party native storage selection for HTTPS partitioned cookies, excluding foreign-site, cross-site-ancestor and opaque keys, and refuses malformed or oversized headers. Cookie values and authentication headers are not returned as tool metadata or diagnostics. HTTPS explicitly requires certificate validation, including when a host environment has disabled its default; fixture trust is injected only into the verifier child, never a system trust store.

This is an explicit first-party cookie request, not a replay of page JavaScript with its initiator, service workers, referrer or HTTP authentication cache. Cookie headers may legitimately apply across ports or subdomains according to the browser's cookie rules, but grants remain exact scheme/host/port checks. Caller headers, Authorization authentication, arbitrary embedded/foreign partition selection and imported storage remain unsupported. HTTPS reads can use the matching first-party partition as described below. Cookies are read-only unless the separate synchronization argument below is enabled; a read-only request to a server that rotates its session may require a new explicit browser authentication workflow. A failed or uncertain request is never retried automatically.

Cancellation during an existing-context or cookie lookup stops the job without waiting for a late read to resume dispatch. Closing the owned browser invalidates this authentication source; a later authenticated call fails until a browser context exists again. Default unauthenticated downloads continue to work after an explicit close. The doctor's existing `downloads` value describes the default mode; its separate `download_authentication` field reports whether the explicit cookie mode is available under the saved grants. CLI scope summaries likewise describe the default mode.


### Response-cookie synchronization

For an authenticated workflow that updates its login cookies, explicitly enable both arguments:

```json
{"url":"https://example.test/private/report","use_browser_context":true,"sync_response_cookies":true}
```

Response `Set-Cookie` is applied to the captured browser context before each redirect/body, including cookie deletion and headers on a failed HTTP response. A later denied redirect, body failure or cancellation does not roll back an already applied cookie effect. Existing read-only calls retain their semantics. Normal MCP approval previews show both arguments; this mode may update or log out the browser session.

The backend uses a temporary page and a locally fulfilled empty text response for the authorized source URL. Only bounded cookie headers are carried across; remote bodies, Location and other response headers are never executed there. Every main-frame GET is intercepted and no second remote HTTP GET is issued. The temporary page is closed before success and the original tab list is preserved. Chromium handles the verified Domain/public-suffix/Path/Secure/HttpOnly/SameSite/prefix/expiry rules, including secure-cookie overwrite protection. Chromium can still attempt speculative CONNECTs to the granted source through the same origin-enforcing proxy: this is not a zero-TCP-connection claim. A per-page offline/CDP experiment did not remove those preconnects and was withdrawn.

An uncertain synchronization failure or cancellation during the native operation closes **the captured browser instance**, including its tabs, and requires explicit re-observation/login afterward. Late cleanup cannot close a replacement browser. Cookie parsing is not implemented by a new npm dependency; neither a browser binary nor a cookie library is bundled.

**Partitioned responses fail closed.** On the pinned runtime, a native interception characterization stored a `Partitioned` cookie without its partition key, even with an origin-preserving sandbox variant. The publisher therefore rejects any such response-cookie batch before creating a synchronization page; it never silently downgrades it to an ordinary cookie. Full partitioned-cookie support remains unfinished.

Run the native-cookie and lifecycle checks explicitly (all URLs are synthetic local fulfillments):

```sh
node --test integrations/browser-origin-scope/policy.test.cjs integrations/browser-origin-scope/transfers.test.cjs integrations/browser-origin-scope/cookies.test.cjs integrations/browser-origin-scope/partition-cookies.test.cjs
node integrations/browser-origin-scope/verify-cookie-sync.cjs \
  --backend-root /absolute/installed/node_modules/@playwright/mcp \
  --report /absolute/new-native-cookie-report.json
```

This verifier records native cookie behavior, source/dependency hashes, proxy request/CONNECT counters, actual cancellation, explicit empty replacement and observed descendant disposal. The separate `verify-downloads.cjs` checks real loopback HTTP(S) rotation/deletion, one remote GET per hop, denied redirect effects, Secure cookie isolation, unchanged tabs and Partitioned refusal through MCP. Neither suite establishes other-platform GUI acceptance or the complete upload/CDP matrix.


### First-party partitioned-cookie reads

With `use_browser_context:true`, HTTPS downloads now include only the matching first-party partition in addition to applicable ordinary cookies. Chromium canonicalizes the approved target URL's schemeful site in a separate empty probe context, which receives no existing credentials. Fixed public protocol calls obtain the captured original context's cookie metadata. Domain, path, Secure, expiry, exact canonical site, `hasCrossSiteAncestor:false` and non-opaque metadata are checked before a header is returned. The caller cannot choose a site/context ID or send protocol commands; model-facing full CDP remains blocked.

Temporary probe contexts, blank identity pages and protocol sessions are closed before dispatch. No helper navigation or additional remote GET is needed, and the original login cookie jar is not mutated. Unknown identity, unavailable metadata, cleanup failure or cancellation closes only the captured browser; a late cleanup cannot act on its replacement. This lookup is a direct first-party request, not selection of an arbitrary iframe's parent partition. HTTP retains its ordinary-cookie/Secure restrictions; Partitioned response sync remains refused.

Characterize the pinned behavior and test the actual first-party publisher explicitly:

```sh
node integrations/browser-origin-scope/characterize-partitions.cjs \
  --backend-root /absolute/installed/node_modules/@playwright/mcp \
  --report /absolute/new-partition-characterization.json
```

The verifier requires Chrome, `ps`, `openssl req -addext`, and Node exposing `tls.setDefaultCACertificates` (recorded Node 22.22.0). Production modules still use Node 20+ APIs. It changes fixture CA trust only inside its own process and restores it afterward; no system trust store or personal browser is changed. All server/cookie data are disposable local fixtures. It compares real HTTPS cookie behavior, the intentionally unsafe interception negative control, explicit context-bound Storage identities and expiry, exact published bytes, no duplicate GET, helper disposal and header ordering in both creation orders. Cookie/header values are not included in the report.

This evidence also rules out the earlier launch-flag hypothesis: the unchanged default runtime can create/send real CHIPS despite disabling `ThirdPartyStoragePartitioning`; removing that disable did not repair intercepted response writes. No startup flags are changed. The safe validated storage bridge is currently used for **reads**; a complete validated Partitioned response writer and broader embedded/authentication workflows remain unfinished.
