# Continuous reasoning budget default

Approved scope: raise the default continuous reasoning budget from 120 to 300
seconds and identify budget exhaustion separately from a network idle timeout.

The coding configuration remains the owner of the default. The existing harness
agent loop and stream watchdog continue to enforce it. No new timer, retry policy,
model override, lifecycle hook, or public error code is introduced.

`ATOMCODE_REASONING_TIMEOUT_SECS` still overrides the default; `0` still disables
the reasoning budget. Prefill and stream idle limits remain independent. Existing
partial-response preservation and optional recovery behavior remain in place.

The 300-second value is an interim budget, not an equivalence with other agents'
300-second stream idle limits or a benchmark-proven optimum. The three-task
DeepSWE trial uses a frozen binary and an explicit zero override, so source
changes do not alter that comparison.

Validation: existing reasoning-deadline tests cover preserved partial work,
prefill exclusion, explicit continuation, and bounded recovery. Update the error
assertion to verify the budget-specific explanation; run affected crate tests
and formatting checks.
