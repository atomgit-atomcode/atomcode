# Portable backend build verification

Baseline AtomCode 505882784, isolated clean worktree. Backend source is pinned upstream e551c13e7082ce4c097234137582fbf4a1bc647a. It contains distinct Go Windows/Linux runtimes. Both doctor commands print prose rather than the JSON permission schema expected by the macOS setup. Their MCP initialize does not advertise negotiated desktop-turn leases. Do not relax ComputerCli platform checks or invent equivalent safety capabilities.

Add an opt-in source-only verifier under integrations/open-computer-use. Require an unmodified pinned checkout and a fresh output directory outside that checkout; never install or run cross-built products. Record exact Go toolchain, source closure, readonly module/dependency graph, output digests and sizes. Build Linux/Windows amd64/arm64 and compile the corresponding test binaries. On the current macOS host, execute only Linux's host-safe tests (non-Linux desktop stubs); this proves pure logic, not Linux GUI. No target test execution on another host by implication.

Keep report dimensions separate: compilation, host-only logic, actual GUI acceptance (always false here). Refuse dirty/wrong source and existing/nested output before writing. Recheck source bytes/status afterward. No AtomCode Rust dependency, runtime lifecycle, credentials, live-agent state or installed backend is changed. This is an input to platform integration, not finished platform support. Record structured doctor, app policy, screenshots/input, lease ownership and actual OS desktop fixture acceptance as remaining work.

Verify refusal paths and execute all builds/tests against the fixed source. Preserve artifacts outside Git. Run Python syntax, source checks, format and compile gate before release-branch integration.

## Measured result

Fresh unmodified pinned source and all 60 measured inputs stayed unchanged. Go 1.26.3 darwin/amd64 compiled Linux amd64/arm64 and Windows amd64/arm64 products plus each corresponding test executable. Target products and test binaries were never executed. Linux's macOS-stub test run: 97 top-level passed, 2 skipped, 110 pass events including subtests. The skipped tests discover a Linux desktop session/canonical runtime bus and explicitly require Linux.

| Optional stripped backend | Bytes | Actual target execution |
| --- | ---: | --- |
| Linux amd64 | 4,595,874 | No |
| Linux arm64 | 4,391,074 | No |
| Windows amd64 | 3,802,624 | No |
| Windows arm64 | 3,513,344 | No |

The report records executable/test SHA-256, exact verifier SHA-256 and module graphs. These are local unsigned measurements, not AtomCode installation-size deltas. Artifacts remain outside Git in the portable-backend acceptance scratch directory. Five real CLI refusal groups passed: existing output preserved, nested output refused, dirty source preserved, index-hidden modified bytes rejected and wrong clean revision rejected. A local disposable commit supplies the wrong-revision fixture even for shallow pinned clones; no upstream commit or push occurs.

Next platform integration needs a structured, platform-appropriate readiness/capability contract (the current doctor prose cannot be parsed as macOS permission success), execution-side app scope and desktop ownership, and controlled fixtures on actual Windows and Linux X11/Wayland hosts. Compilation does not justify changing the managed-enable guard. HarmonyOS PC remains unselected/unverified.

Delivery checks: Python syntax and five refusal groups passed; final report verifier digest matches the current source. Full compile gate (default, features and negative controls), cargo fmt check and diff check passed. Cargo.lock unchanged with H index flag; hkdf/hmac/zeroize_derive counts remain zero. No Rust code/dependency, bundled product or user platform configuration changed.
