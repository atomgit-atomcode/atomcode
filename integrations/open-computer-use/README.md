# Experimental macOS desktop-turn lease backend

This is a small source patch and protocol acceptance harness for **Open Computer Use v1.3.0**, pinned to commit `e551c13e7082ce4c097234137582fbf4a1bc647a`. It contains no backend binary, browser, profile or captured desktop data. Upstream code is MIT licensed; see UPSTREAM-LICENSE.

**Not a completed AtomCode feature:** host scope propagation and normal runtime turn-end wiring are implemented for negotiated stdio protocol v1. Isolated App/socket construction and no-GUI proxy acceptance are implemented. Actual GUI cancellation/fresh-observation acceptance and other-platform support are still required. The current shipped backend does not acquire these leases. This patch is not installed automatically and has not replaced the user's App.

## Why execution-side ownership

The macOS stdio proxy forwards to a shared App. That App retains a separate MCP server per connection. This patch puts the private per-user OS file lock on that actual server connection, not the frontend or proxy. Normal release is owner-addressed; disconnect drops the lock after the connection's accepted synchronous request processing finishes. It does not terminate the shared App, unlink live lock files or broadcast a global release. External GUI effects are not rolled back, and asynchronous application updates can still occur after a request returns.

The default lock namespace depends on the OS user, independently of product directories. A private directory override exists for controlled tests. Locks cooperate between participating clients; legacy clients and humans remain outside this protocol. This is not a sandbox against another local process.

## Build without installing

Use a separate, clean clone. The build script refuses another commit or dirty source and never resets files. The patch uses zero context (to avoid storing trailing-whitespace diff context); it is applied with --unidiff-zero only after verifying that exact clean source commit. The script applies the patch to that clone and writes build artifacts only to the supplied scratch directory.

```sh
git clone --depth 1 --branch v1.3.0 https://github.com/opensymph/open-computer-use.git /tmp/ocu-lease-source
bash integrations/open-computer-use/build-macos-lease.sh \
  /tmp/ocu-lease-source /tmp/ocu-lease-build /absolute/path/to/swift
python3 integrations/open-computer-use/verify_lease.py \
  --backend /tmp/ocu-lease-build/arm64-apple-macosx/debug/OpenComputerUse \
  --report /tmp/ocu-lease-protocol.json
```

The exact executable subdirectory depends on the host target. A repeat build of an already patched clone can use the printed Swift build command directly; the patch script deliberately refuses dirty clones.

The original lease build baseline was macOS 14.5 / SDK 14.5 with Swift 6.1.3; the isolated-App extension was freshly built on macOS 14.5 / SDK 15.2 with the same Swift 6.1.3 toolchain. The patch lowers the manifest's tools requirement from 6.2 to 6.1 and adds **compiler-checked Sendable conformance** to two immutable value types needed by Swift 6.1. It does not disable concurrency checking. This is a modified backend build, not proof that upstream's original manifest builds unchanged. The local Command Line Tools installation lacks XCTest: upstream XCTest targets were not run. Product compilation, an XCTest-independent native cache/identity/lease smoke executable and separate real-process protocol acceptance are the evidence here. The smoke tests verify cache invalidation without reissuing old screenshot identities, including a new connection's first observation, and perform no GUI operation. A separate native namespace smoke tests lifetime locking, stale/live sockets, replacement-inode cleanup and file defenses.

The protocol harness sends handshake, lease control, ping and nonexistent-tool requests. It requests no screenshots, application contents or GUI actions. Direct-process cases explicitly disable installed-App proxy discovery so they cannot attach to the user's default App socket. Shared-App cases launch the exact test executable on a unique private socket, close only test-owned connections and stop only that test-owned helper PID. No OS permissions are granted or bypassed. The test binary currently reports Accessibility=false; no GUI acceptance is claimed. Upstream raw development binaries can resolve an installed App through discovery. This derivative refuses that proxy fallback: use the explicitly packaged, protocol-marked development App. Direct mode exists for protocol tests only; it is not the real App/proxy acceptance path. Do not activate the experimental backend for normal use until actual GUI acceptance is complete.

## Isolated App packaging and diagnostics

The build recipe records source inputs before and after compilation/smoke, the pinned patch, compiler/SDK and executable measurements in `atomcode-build-provenance.json`. The optional packager requires that manifest to match both the current patch and the input executable, refuses a binary without both lease/isolation capabilities and refuses an existing output. These are local build measurements, not a publisher-signed attestation. It probes the copied payload, ad-hoc signs a separate development bundle, verifies its signature and exact permission-App discovery, and records upstream/patch/executable provenance. It does not install, replace another App, set default handlers or grant OS permissions. The bundle uses `com.ifuryst.opencomputeruse.dev` and the display name `Open Computer Use (AtomCode Lease)`.

```sh
python3 integrations/open-computer-use/package-macos-lease-app.py \
  --backend /tmp/ocu-lease-build/arm64-apple-macosx/debug/OpenComputerUse \
  --build-provenance /tmp/ocu-lease-build/atomcode-build-provenance.json \
  --output '/tmp/Open Computer Use (AtomCode Lease).app' \
  --report /tmp/ocu-lease-app-provenance.json
python3 integrations/open-computer-use/verify_app_isolation.py \
  --app '/tmp/Open Computer Use (AtomCode Lease).app' \
  --report /tmp/ocu-app-isolation.json
```

The proxy namespace hashes the POSIX-canonical App path and executable SHA-256 in a private per-user directory, independent of product profiles. Only the actual listener holds the separate lifetime lock and may reclaim a checked stale socket. Its versioned lock-file record binds the real listener PID and filesystem socket inode. Recovery requires a demonstrably dead PID and the same inode; connection refusal is insufficient because macOS can return it for a live full backlog. Unknown records and reused/live PIDs fail closed. Active/mismatched/hung listeners and their sockets are left intact; connection and identity negotiation are bounded. Cleanup compares the bound filesystem inode. These checks are cooperative same-user hygiene, not authentication against a malicious same-user process.

App acceptance launches the exact scratch bundle via LaunchServices, verifies two proxies/profiles share its helper, checks EOF/lease handover, mismatch refusal, hung identity reads, a full connection backlog and concurrent listener startup/stale recovery. A live legacy-default-socket sentinel must stay untouched. Cold-launch/proxy acceptance also rejects the reproduced MainActor data-race warning: the unused NSWorkspace completion closure was removed, while the existing bounded identity handshake remains the startup authority. The same regression fails on the prior App and passes on the corrected build. All these tests use diagnostics and lease RPCs only. The upstream patch also includes `docs/ATOMCODE_LEASE_OVERLAY.md` to distinguish this experimental behavior from upstream defaults.

## Opt-in real GUI acceptance

Build the dedicated AppKit fixture with `build-gui-fixture.py --swiftc /absolute/path/to/swiftc --output '/tmp/AtomCode CU Fixture.app'`. This only compiles/signs the target; it does not launch it. After the exact isolated backend App has Accessibility and Screen Recording grants, run:

```sh
python3 integrations/open-computer-use/verify_gui.py \
  --app '/tmp/Open Computer Use (AtomCode Lease).app' \
  --fixture '/tmp/AtomCode CU Fixture.app' \
  --report /tmp/ocu-real-gui.json --perform-gui
```

Without the explicit flag or both actual runtime grants, no fixture is launched. The intended test observes only its own window, assigns a known text value, presses a normal button and a bounded gated button, kills the owned proxy while the gated action is accepted, checks the native lease against a second process, then verifies fresh-connection observation IDs reject the old ID. The fixture state file witnesses actual widget actions; it is not an automation control bridge. This script is prepared and compiled where possible but real GUI acceptance remains pending OS permission; a no-GUI result is not a substitute.

## Protocol v1

A host opts in at initialize with `capabilities.experimental["atomcode/turn-scopes"].version = 1`. The backend advertises `capabilities.experimental["atomcode/desktop-turn-lease"].version = 1`. Unsupported or malformed requested scope versions fail rather than silently downgrading. A scoped connection cannot downgrade itself with a later initialize.

Control RPCs `atomcode/desktop/acquire` and `atomcode/desktop/release` take `params._meta["atomcode/turn"]`, a bounded nonempty opaque host owner. The same metadata is required on `tools/call`; model tool arguments cannot supply it. Successful acquire returns `result.acquired=true`; successful release returns `result.released=true`. The host must validate that exact acknowledgement, not treat any JSON-RPC result as success: backend errors can be represented as `result.isError=true`.

Acquire is idempotent for the same owner on the same connection. Another owner or process gets a busy error. A connection without the lease cannot release another connection, even if it supplies the same owner string. Matching release clears that connection's observation caches while preserving its monotonic screenshot identity sequence. Negotiated connections also use a random per-service screenshot namespace, so a new connection cannot reissue an old connection's ID. First negotiation clears legacy observations. Negotiated legacy app actions require an explicit observation; they cannot silently refresh and apply an old element index. Unaddressed legacy `notifications/turn-ended` does not release a negotiated lease. Legacy clients remain compatible but are not cooperatively isolated.

The host generates scope identity from the real executing agent's session ID and committed TurnStart sequence, hashes it into bounded opaque metadata outside model arguments, and uses the existing awaited TurnFinishing boundary. Adapters resolve the executing context at call time; delegated children do not inherit their lead's lease. Exact acknowledgements are validated. A negotiated connection cannot silently lose this capability during recovery, and scoped acquire/tool/release transport failures require explicit reconnect and re-observation rather than cross-connection replay. Cleanup errors are applied only to the captured client, never a replacement published under the same name. Unknown remote outcomes still require transport retirement/reconciliation. It must not add a kernel coding command, treat approvals as desktop ownership, call the global CLI turn-ended broadcast or kill a shared App as cleanup.

## Evidence and remaining acceptance

Native-process acceptance covers different product homes, same-owner reentrancy, different/wrong/stale owners, metadata injection refusal, scoped downgrade refusal, strict capability versions, normal EOF, direct process death, symlink directory/file, hardlink and unsafe-mode refusal, and UTF-8 owner limits. A private shared App verifies per-connection leases, exclusion against direct processes, lease release on disconnected connections and survival of neighboring connections/App.

Still unverified: an actual GUI operation executing when the host/proxy dies, fresh window/screenshot references after turnover, actual GUI approval/cancel/reload interaction and non-macOS backends. Host fixture tests cover normal runtime end/cancel, child identity, queued cancellation, malformed acknowledgements, release deadlines, connection loss and negotiation downgrade. An opt-in native test covers scoped calls and lease handover between two production registries without GUI actions. Do not mark desktop exclusivity complete until these are tested.

## Windows/Linux build evidence (not desktop acceptance)

The pinned upstream also contains separate Go runtimes for Windows and Linux. They are not covered by the macOS lease overlay. Both currently print prose from doctor rather than AtomCode's structured permission diagnostic, and neither advertises the experimental desktop-turn lease. AtomCode's managed computer setup remains macOS-only.

Use an unmodified pinned checkout, not the patched macOS clone, and a fresh output directory outside it:

```sh
python3 integrations/open-computer-use/verify_portable_builds.py \
  --source /tmp/ocu-portable-source --output /tmp/ocu-portable-builds
python3 integrations/open-computer-use/verify_portable_refusals.py \
  --source /tmp/ocu-portable-source --report /tmp/ocu-portable-refusals.json
```

This opt-in developer verifier uses an installed Go compiler with automatic toolchain fetching disabled; public locked Go dependencies may enter the Go cache. It builds Linux/Windows amd64/arm64 products and test binaries, records their module graphs, source/verifier digests and sizes, and never installs or runs the target products. On macOS only, Linux's non-Linux-stub logic tests also run. Their pass/skip counts remain separate from target compilation. Pinned input bytes are checked against committed blobs even when Git index flags hide edits; existing/nested output and dirty/wrong source are refused. Reports are local measurements, not signed upstream attestations.

The recorded Go 1.26.3 macOS run passed 97 top-level Linux logic tests and skipped two Linux desktop-session discovery tests (110 pass events including subtests). All four product and test-binary cross-compiles passed. Stripped optional backend products measured Linux amd64 4,595,874 bytes, Linux arm64 4,391,074 bytes, Windows amd64 3,802,624 bytes and Windows arm64 3,513,344 bytes. These external unsigned products are not added to AtomCode or its installer. Five real CLI refusal groups passed. None of this proves actual desktop screenshots/input, Wayland support, permissions, cancellation, app policy or desktop lease handover. See [platform verification](../../docs/plans/2026-11-11-portable-backend-verification.md).

## Experimental macOS application scope

The patched backend additionally advertises `atomcode/application-scope` v1 in MCP initialize and `application_scope.version=1` in doctor. Trusted host environment `OPEN_COMPUTER_USE_ALLOWED_BUNDLE_IDS` is a bounded JSON array of case-normalized exact bundle IDs. Absence preserves legacy behavior; `[]` denies every app; malformed arrays refuse all 15 tools, including `capture_display`. Model tool arguments cannot configure this environment. Existing password-manager denial remains authoritative.

Resolved app/window identities are checked before observation/input, unknown IDs are refused when scoped, recent/running/window catalogs are filtered, refusal messages do not disclose unallowed resolved identities, and cached observations are checked again against the current policy. Scoped observations bypass the legacy name-selected FixtureBridge and do not select focus roots/elements from racing system-wide focus. Full-display capture and global-pointer fallback are refused, independently of desktop lease ownership. Window images use the existing desktop-independent ScreenCaptureKit filter.

The managed setup path now accepts `atomcode computer enable --executable /path/to/patched/backend --allow-app com.apple.TextEdit` (repeatable), and `/computer enable --allow-app com.apple.TextEdit` for an already configured backend. `--deny-all-apps` stores an empty allow-list; `--unrestricted-apps` explicitly removes only the saved application restriction. Plain enable retains the saved policy, even after disable. Setup checks doctor scope v1 and records `requiredExperimentalCapabilities: {"atomcode/application-scope": 1}` alongside the environment policy. The actual MCP handshake and every recovery independently check this requirement. Existing sessions still require explicit `/computer reload`; inspect `/mcp` after reload rather than treating config success as live activation.

This remains an optional backend prototype with unaccepted real GUI effects, not a signed production install. The stock pinned npm backend lacks this extension. Scope is not an OS sandbox against shell commands, malicious same-user processes, other clients, publisher spoofing or effects inside an allowed app. Actual allowed-window screenshots/input and privacy/effect acceptance still require the exact App's OS grants and controlled GUI fixtures.

```sh
python3 integrations/open-computer-use/verify_application_scope.py \
  --app '/tmp/Open Computer Use (AtomCode Lease).app' \
  --report /tmp/ocu-application-scope.json
```

Native smoke uses synthetic window descriptors to prove denied/unknown bundles are refused before AX liveness reads, allowed descriptors proceed, and the actual cached window reader refuses a tightened scope. The real App/proxy harness covers all 15 tools under malformed policy, empty catalogs, model-argument override refusal, three client environments sharing one helper, and valid lease ownership failing to override full-display denial. It requests no screenshots or GUI input. An older lease-only App is rejected at capability validation before tool dispatch.
