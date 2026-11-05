# Allowance follows the active endpoint

Baseline: release/v5.2.6, 63e9fcab3. The host mounts a CodingPlan meter for every
runtime. Its existing applies_to endpoint predicate was used by the 429 policy
but not by usage queries. Reuse that predicate at CodingParts' meter accessor:
usage, goal budgeting and mounted rate-limit policy receive a meter only for
an applicable active endpoint. No kernel business rule, new public protocol,
credential field or second account owner is introduced.

A non-applicable endpoint answers usage with an empty, available result and does
not contact the official account service. Third-party usage is not invented.
TUI descriptions of the visible agent or its root invalidate the old footer
allowance, reset its throttle, and request new data. A local request generation
and addressed-session check reject late responses before applying quota or an
exhaustion nudge. Equal model wire names do not preserve an old account's quota.
A network failure for the same unchanged model still retains its previous value.

Tests: switch a production-mounted runtime managed -> external -> managed using
the same model name, verify both window-only/full usage and meter call counts;
delay an old exhausted TUI response until after an empty replacement response,
verify no stale badge or exhaustion warning. No real credentials/network calls
or native-terminal manual verification are required by these regressions.

Validation: coding 1243 passed (10 skipped), TUI 1917 passed; the production
endpoint-switch and delayed-response regressions passed. Workspace/feature
compile gate, rustfmt check and diff check passed. Cargo.lock/latest.json are
unchanged. Native-terminal manual validation remains outstanding.
