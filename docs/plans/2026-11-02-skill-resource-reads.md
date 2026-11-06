# Skill resource reads implementation plan

Goal: loaded skills remain usable in a root-fenced execution world without granting writes.

Baseline: release/v5.2.6 at f46be582d, clean worktree. The existing LocalFs owns containment;
coding prepare owns the loaded registry. Hook trust is execution trust, not file trust.

1. Add explicit read-only roots/files to LocalFs, canonicalized when mounted. Reads
   resolve real paths against these captured grants; writes retain existing checks.
2. Pass readable paths through existing fs row configuration. Coding derives paths
   from winning loaded skill definitions on each mount, including reload and cd.
   Directory SKILL.md grants its directory; flat Markdown grants only itself.
3. Test actual read_file routing, resource reads, denied writes, unrelated paths and
   symlink escapes. Keep independent sensitive-file approval rows unchanged.
4. Run affected crate nextest suites, compile gate, formatting and lock checks.

No protocol, session format, second runtime owner, plugin-wide trust or resources
frontmatter is introduced. Canonical path checks do not claim kernel-grade isolation
against concurrent filesystem mutation. Windows junction canonicalization follows
std::fs semantics; platform-specific execution requires Windows CI.

Validation completed on macOS: 3,621 tests passed across capabilities, coding and
harness (10 skipped), including the actual read_file route and registry-driven
mount/revocation tests. Compile gate (workspace, feature paths and negative
controls), cargo fmt check and diff whitespace check passed. Cargo.lock is unchanged;
hkdf/hmac/zeroize_derive counts are zero. Windows junction regression is present
behind cfg(windows) and was not executed on this host.
