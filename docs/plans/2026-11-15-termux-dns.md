# Termux DNS implementation plan

Goal: allow musl Linux binaries in Termux to resolve hosts without proot.
Architecture: atomcode-auth owns a shared reqwest resolver adapter, consumed by OAuth, CodingPlan and capabilities provider clients. No runtime/session lifecycle changes. Preserve existing proxy/TLS policy. No public DNS fallback or binary patching.
Baseline: release/v5.2.8 at 7afd8b557. Existing bash background and WebUI transport changes belong to other work; preserve them.

1. Detect Termux on Linux and select its readable PREFIX/etc/resolv.conf only when /etc/resolv.conf is unavailable. Validate absolute PREFIX and a nonempty nameserver configuration; never invent DNS servers.
2. Implement a bounded resolver using the library DNS parser/transport; install through reqwest's resolver seam for async and blocking clients. Invalid fallback configuration fails DNS lookup explicitly rather than using a library default.
3. Wire OAuth, CodingPlan and provider client factories, preserving proxy behavior and TLS validation. Keep other platforms on their original resolver.
4. Test detection/config selection, invalid configuration, DNS queries to a local fixture, and async/blocking client wiring. Real Termux validation remains required.
5. Run affected crate nextest, compile gate and formatting; review dependency and lockfile changes. Do not commit/push without a request.

Implementation notes: hickory-resolver (default features disabled) provides DNS transport; resolv-conf parses the alternate file. Only detected Termux Linux with an unreadable system resolver configuration installs the adapter. Configuration reads are capped at 64 KiB and repeated for each lookup; DNS attempts are bounded. Neither public nameservers nor TLS/HTTP proxy bypass is introduced. MCP-only builds without provider remain independent of auth.

Validation so far: 1,652 affected-crate tests passed, one pre-existing skipped. Actual local DNS/HTTP fixtures cover async and blocking requests, invalid configuration after a successful lookup, and proxy-side destination resolution. Linux musl cross-check was attempted but the host lacks x86_64-linux-musl-gcc; no claim of Linux cross-build or Android device validation. User guide: docs/troubleshooting/termux-dns.md.

Final verification: the complete compile gate passed in an isolated HEAD checkout containing only the DNS candidate files, using an independent CoW target cache. This was necessary because concurrent unrelated background-output experiment tests in the shared worktree did not compile. Formatting and lockfile checks passed; hkdf/hmac/zeroize_derive counts remain zero. Existing parallel edits were preserved.
