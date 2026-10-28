# Bounded native session references

Baseline: release/v5.2.5 at 7d6b291dc18dea74a0078975ef12acae13f10742.
Existing changelog edits are unrelated and must be preserved.

The native event log remains the only authority. References are read-only and
never resume, migrate, or write the source. A shared read lock excludes managed
appends during the scan. Existing filesystem, line-size, format/version and ID
checks remain in force. The reference scanner additionally verifies its header
identity and increasing event sequence.

Use the existing safe JSONL scanner with a 64 MiB source scan cap and existing
16 MiB line cap. After validating each original event, trim reference-only text
to 8,000 characters per field and discard reasoning/images. Retain at most 8 MiB
of serialized event payload plus per-record accounting, then feed those facts to
the existing canonical projector. Compaction and rewind are not reimplemented.
This is a bounded read projection, never a persisted conversation or new store.
Oversized projections fail explicitly rather than substituting an unvalidated tail.

Select the effective summary by canonical provenance, not by accepting arbitrary
synthetic messages. Reserve up to 3,000 of the existing 8,000-character excerpt
budget for the current summary, then retain recent visible dialogue. Keep the
four-source limit, untrusted-data framing, and no-tool/no-reasoning policy.
Report omitted messages, partial content, and the captured sequence. Legacy
snapshot references retain their existing bounded loader; large snapshot support
is not silently enabled by migration.

Make the generic terminal rejection neutral instead of guessing that the provider
is missing; the existing following error supplies the actual reference failure.
No new model calls, protocol error variants, or fallback engine are introduced.

Verification: >8 MiB/139-message event fixture, output budgets and Unicode,
effective summary replacement, rewind, unsupported versions, malicious identity,
read-only behavior, explicit scan cap, native session regressions and runtime
reference ownership. Run the compile gate and formatting checks before delivery.


Verification completed: 261 session tests and the runtime reference ownership
regression passed. The complete compile gate, formatting, and lock checks passed.
A cached single-case run of the >8 MiB/139-message regression completed in 0.65s
wall time with maximum RSS 107,036,672 bytes (~102 MiB), measured with macOS
`/usr/bin/time -l cargo nextest run ...`. This includes fixture construction and
the test harness; it is not the isolated production reader's allocation or an
8 MiB process-memory claim. No Windows machine or original user log was used.
