# Bound instruction-file reads

Baseline: release/v5.2.5, 55ad4ee30. Unrelated changelog edits are preserved.

Both config::instructions and capabilities::instructions read whole files before
applying their 1 MiB output limit. Replace that with a common config-leaf utility:
check ordinary-file metadata, open and recheck the descriptor, then read at most
1 MiB + 1 byte. Never size an allocation from untrusted file metadata. On Unix,
nonblocking open prevents a stat/open race replacing the path with a FIFO from
hanging startup. Symlinks to ordinary files remain supported.

The extra byte determines whether to append each loader's existing truncation
marker. Cut at a UTF-8 boundary; malformed bytes inside the retained prefix still
fail as before. Each loader retains its existing missing/unreadable/empty and
project precedence behavior. Three fixed tiers retain at most roughly 3 MiB of
source text (rendering may make bounded copies), rather than an arbitrary source.
This is not a universal limit for other configuration or skill reads.

The instructions feature now explicitly depends on the existing config leaf so
both loaders share the same implementation. No runtime/session state changes,
new crate, or lockfile update is needed.

Regression coverage: exact boundary, multi-byte truncation in both callers,
malformed UTF-8, directory and FIFO rejection, and an 8 GiB sparse-file read with
a tiny budget on Unix. No huge actual file allocation is used by tests.
