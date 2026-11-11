# Experimental macOS desktop-turn lease backend

This is a small source patch and protocol acceptance harness for **Open Computer Use v1.3.0**, pinned to commit `e551c13e7082ce4c097234137582fbf4a1bc647a`. It contains no backend binary, browser, profile or captured desktop data. Upstream code is MIT licensed; see UPSTREAM-LICENSE.

**Not a completed AtomCode feature:** host scope propagation, normal runtime turn-end wiring, actual GUI cancellation/fresh-observation acceptance and other-platform support are still required. The current shipped backend does not acquire these leases. This patch is not installed automatically and has not replaced the user's App.

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

The checked build baseline is macOS 14.5 / SDK 14.5 with Swift 6.1.3. The patch lowers the manifest's tools requirement from 6.2 to 6.1 and adds **compiler-checked Sendable conformance** to two immutable value types needed by Swift 6.1. It does not disable concurrency checking. This is a modified backend build, not proof that upstream's original manifest builds unchanged. The local Command Line Tools installation lacks XCTest: upstream XCTest targets were not run. Product compilation, an XCTest-independent native cache/identity/lease smoke executable and separate real-process protocol acceptance are the evidence here. The smoke tests verify cache invalidation without reissuing old screenshot identities and perform no GUI operation.

The protocol harness sends handshake, lease control, ping and nonexistent-tool requests. It requests no screenshots, application contents or GUI actions. Direct-process cases explicitly disable installed-App proxy discovery so they cannot attach to the user's default App socket. Shared-App cases launch the exact test executable on a unique private socket, close only test-owned connections and stop only that test-owned helper PID. No OS permissions are granted or bypassed. The test binary currently reports Accessibility=false; no GUI acceptance is claimed. Ordinary invocation of a raw development binary can resolve the installed App through upstream discovery. Do not put this raw binary into the managed desktop configuration yet: an isolated App bundle/default-socket identity and host scope wiring are still required before activation.

## Protocol v1

A host opts in at initialize with `capabilities.experimental["atomcode/turn-scopes"].version = 1`. The backend advertises `capabilities.experimental["atomcode/desktop-turn-lease"].version = 1`. Unsupported or malformed requested scope versions fail rather than silently downgrading. A scoped connection cannot downgrade itself with a later initialize.

Control RPCs `atomcode/desktop/acquire` and `atomcode/desktop/release` take `params._meta["atomcode/turn"]`, a bounded nonempty opaque host owner. The same metadata is required on `tools/call`; model tool arguments cannot supply it. Successful acquire returns `result.acquired=true`; successful release returns `result.released=true`. The host must validate that exact acknowledgement, not treat any JSON-RPC result as success: backend errors can be represented as `result.isError=true`.

Acquire is idempotent for the same owner on the same connection. Another owner or process gets a busy error. A connection without the lease cannot release another connection, even if it supplies the same owner string. Matching release clears that connection's observation caches while preserving its monotonic screenshot identity sequence. Negotiated legacy app actions require an explicit observation; they cannot silently refresh and apply an old element index. Unaddressed legacy `notifications/turn-ended` does not release a negotiated lease. Legacy clients remain compatible but are not cooperatively isolated.

The host must generate scope identity from the real executing agent and turn, propagate it outside model arguments, and use the existing awaited TurnFinishing boundary. Unknown remote outcomes still require transport retirement/reconciliation. It must not add a kernel coding command, treat approvals as desktop ownership, call the global CLI turn-ended broadcast or kill a shared App as cleanup.

## Evidence and remaining acceptance

Native-process acceptance covers different product homes, same-owner reentrancy, different/wrong/stale owners, metadata injection refusal, scoped downgrade refusal, strict capability versions, normal EOF, direct process death, symlink directory/file, hardlink and unsafe-mode refusal, and UTF-8 owner limits. A private shared App verifies per-connection leases, exclusion against direct processes, lease release on disconnected connections and survival of neighboring connections/App.

Still unverified: an actual GUI operation executing when the host/proxy dies, fresh window/screenshot references after turnover, host turn scope wiring, approval/cancel/reload interaction and non-macOS backends. Do not mark desktop exclusivity complete until these are tested.
